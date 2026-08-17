//! Bounded provider/tool loop shared by the headless CLI and TUI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{
    ChatMessage, ChatRequest, ContextAudit, ContextManager, Provider, ProviderEvent, Role,
    ToolCall, ToolExecutor, ToolResult, Usage,
};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub model: String,
    pub max_steps: usize,
    pub retries: usize,
    pub retry_delay: Duration,
}

impl AgentOptions {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_steps: 12,
            retries: 2,
            retry_delay: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Delta(String),
    ToolStarted(ToolCall),
    ToolFinished(ToolResult),
    Retry { attempt: usize, error: String },
    ContextTrimmed(ContextAudit),
    Completed { usage: Usage, steps: usize },
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOutcome {
    #[serde(skip_serializing)]
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub usage: Usage,
    pub steps: usize,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<dyn ToolExecutor>,
    context: ContextManager,
    options: AgentOptions,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        context: ContextManager,
        options: AgentOptions,
    ) -> Self {
        Self {
            provider,
            tools,
            context,
            options,
        }
    }

    pub async fn run_new(
        &self,
        system_prompt: impl Into<String>,
        prompt: impl Into<String>,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<AgentOutcome> {
        self.continue_messages(
            vec![ChatMessage::new(Role::System, system_prompt)],
            prompt,
            events,
        )
        .await
    }

    pub async fn continue_messages(
        &self,
        mut messages: Vec<ChatMessage>,
        prompt: impl Into<String>,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<AgentOutcome> {
        messages.push(ChatMessage::new(Role::User, prompt));
        let definitions = self.tools.definitions();
        let mut total_usage = Usage::default();

        for step in 1..=self.options.max_steps {
            let (prepared, audit) = self.context.prepare(&messages);
            if audit.dropped_messages > 0 {
                send_event(events, AgentEvent::ContextTrimmed(audit));
            }
            let request = ChatRequest::new(self.options.model.clone(), prepared)
                .with_tools(definitions.clone());
            let turn = self.stream_turn(&request, events).await?;
            total_usage.add_assign(&turn.usage);
            messages.push(ChatMessage::assistant(
                turn.text.clone(),
                turn.calls.clone(),
            ));

            if turn.calls.is_empty() {
                return Ok(AgentOutcome {
                    messages,
                    response: turn.text,
                    usage: total_usage,
                    steps: step,
                });
            }
            if step == self.options.max_steps {
                bail!(
                    "agent reached the maximum of {} provider steps",
                    self.options.max_steps
                );
            }

            for call in turn.calls {
                send_event(events, AgentEvent::ToolStarted(call.clone()));
                let result = self.tools.execute(&call).await;
                send_event(events, AgentEvent::ToolFinished(result.clone()));
                messages.push(ChatMessage::tool_result(&call, result.model_content()));
            }
        }

        unreachable!("the bounded loop always returns or errors")
    }

    async fn stream_turn(
        &self,
        request: &ChatRequest,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<CompletedTurn> {
        'attempts: for attempt in 0..=self.options.retries {
            let mut stream = match self.provider.stream_chat(request).await {
                Ok(stream) => stream,
                Err(error) if attempt < self.options.retries => {
                    self.retry(attempt, &error.to_string(), events).await;
                    continue;
                }
                Err(error) => return Err(error).context("starting provider stream"),
            };
            let mut text = String::new();
            let mut calls = Vec::new();
            let mut usage = None;
            let mut visible_output = false;

            while let Some(event) = stream.next().await {
                match event {
                    ProviderEvent::Delta(delta) => {
                        visible_output = true;
                        text.push_str(&delta);
                        send_event(events, AgentEvent::Delta(delta));
                    }
                    ProviderEvent::ToolCall(call) => {
                        visible_output = true;
                        calls.push(call);
                    }
                    ProviderEvent::Done(done_usage) => usage = Some(done_usage),
                    ProviderEvent::Error(error)
                        if !visible_output && attempt < self.options.retries =>
                    {
                        self.retry(attempt, &error, events).await;
                        continue 'attempts;
                    }
                    ProviderEvent::Error(error) => bail!("provider stream failed: {error}"),
                }
            }

            if usage.is_none() {
                if !visible_output && attempt < self.options.retries {
                    self.retry(attempt, "provider stream ended before completion", events)
                        .await;
                    continue;
                }
                bail!("provider stream ended before completion");
            }

            return Ok(CompletedTurn {
                text,
                calls,
                usage: usage.expect("checked above"),
            });
        }

        unreachable!("the retry loop always returns on its final attempt")
    }

    async fn retry(
        &self,
        zero_based_attempt: usize,
        error: &str,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) {
        send_event(
            events,
            AgentEvent::Retry {
                attempt: zero_based_attempt + 2,
                error: error.to_string(),
            },
        );
        tokio::time::sleep(self.options.retry_delay).await;
    }
}

struct CompletedTurn {
    text: String,
    calls: Vec<ToolCall>,
    usage: Usage,
}

fn send_event(events: Option<&mpsc::UnboundedSender<AgentEvent>>, event: AgentEvent) {
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use futures_util::stream::{self, BoxStream};

    use super::*;
    use crate::{ToolDefinition, ToolRisk};

    struct ScriptedProvider {
        turns: Mutex<VecDeque<Vec<ProviderEvent>>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        async fn stream_chat<'a>(
            &'a self,
            request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            self.requests.lock().unwrap().push(request.clone());
            let events = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider turn");
            Ok(Box::pin(stream::iter(events)))
        }
    }

    struct ScriptedTools;

    #[async_trait]
    impl ToolExecutor for ScriptedTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            }]
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            ToolResult::success(call, "contents")
        }
    }

    fn agent(provider: Arc<ScriptedProvider>, max_steps: usize) -> Agent {
        let mut options = AgentOptions::new("model");
        options.max_steps = max_steps;
        options.retries = 0;
        Agent::new(
            provider,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
    }

    #[tokio::test]
    async fn performs_structured_tool_loop() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([
                vec![
                    ProviderEvent::ToolCall(call.clone()),
                    ProviderEvent::Done(Usage::default()),
                ],
                vec![
                    ProviderEvent::Delta("fixed".into()),
                    ProviderEvent::Done(Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    }),
                ],
            ])),
            requests: Mutex::new(Vec::new()),
        });

        let outcome = agent(provider.clone(), 3)
            .run_new("system", "inspect", None)
            .await
            .unwrap();
        assert_eq!(outcome.response, "fixed");
        assert_eq!(outcome.steps, 2);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[1].messages.last().unwrap().role, Role::Tool);
        assert!(requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("contents"));
    }

    #[tokio::test]
    async fn enforces_step_limit() {
        let call = ToolCall {
            id: "loop".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        };
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![
                ProviderEvent::ToolCall(call),
                ProviderEvent::Done(Usage::default()),
            ]])),
            requests: Mutex::new(Vec::new()),
        });
        let error = agent(provider, 1)
            .run_new("system", "loop", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    #[tokio::test]
    async fn surfaces_error_after_visible_output_without_retrying() {
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![
                ProviderEvent::Delta("partial".into()),
                ProviderEvent::Error("disconnect".into()),
            ]])),
            requests: Mutex::new(Vec::new()),
        });
        let mut options = AgentOptions::new("model");
        options.retries = 2;
        options.retry_delay = Duration::ZERO;
        let agent = Agent::new(
            provider.clone(),
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        );
        let error = agent.run_new("system", "hello", None).await.unwrap_err();
        assert!(error.to_string().contains("disconnect"));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_incomplete_stream_after_visible_output() {
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![ProviderEvent::Delta(
                "truncated".into(),
            )]])),
            requests: Mutex::new(Vec::new()),
        });
        let error = agent(provider, 2)
            .run_new("system", "hello", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("before completion"));
    }
}
