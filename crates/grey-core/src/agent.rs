//! Bounded provider/tool loop shared by the headless CLI and TUI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{
    CachedResponse, ChatMessage, ChatRequest, ContextAudit, ContextManager, Provider,
    ProviderEvent, ProviderModelRef, RequestCache, Role, ToolCall, ToolExecutor, ToolResult, Usage,
    UsageTracker,
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
    Retry {
        attempt: usize,
        error: String,
    },
    ContextTrimmed(ContextAudit),
    ProviderSwitched {
        from: String,
        to: String,
        reason: String,
    },
    CacheHit {
        model: String,
    },
    Completed {
        usage: Usage,
        steps: usize,
    },
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOutcome {
    #[serde(skip_serializing)]
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub usage: Usage,
    pub steps: usize,
    pub cached: bool,
    pub provider_id: String,
    pub model: String,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    provider_id: String,
    tools: Arc<dyn ToolExecutor>,
    context: ContextManager,
    options: AgentOptions,
    cache: Option<Arc<RequestCache>>,
    usage: Option<Arc<UsageTracker>>,
    fallback_chain: Vec<ProviderModelRef>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        context: ContextManager,
        options: AgentOptions,
    ) -> Self {
        Self {
            provider_id: provider.id().to_string(),
            provider,
            tools,
            context,
            options,
            cache: None,
            usage: None,
            fallback_chain: Vec::new(),
        }
    }

    pub fn new_legacy(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolExecutor>,
        context: ContextManager,
        options: AgentOptions,
    ) -> Self {
        Self::new(provider, tools, context, options)
    }

    pub fn with_cache(mut self, cache: Arc<RequestCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    pub fn with_usage(mut self, usage: Arc<UsageTracker>) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_fallback_chain(mut self, chain: Vec<ProviderModelRef>) -> Self {
        self.fallback_chain = chain;
        self
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
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
        let mut cached = false;

        for step in 1..=self.options.max_steps {
            let (prepared, audit) = self.context.prepare(&messages).await;
            if audit.dropped_messages > 0 {
                send_event(events, AgentEvent::ContextTrimmed(audit));
            }
            let request = ChatRequest::new(self.options.model.clone(), prepared)
                .with_tools(definitions.clone());

            if let Some(cache) = &self.cache {
                if let Some(cached_resp) = cache.get(&self.options.model, &request.messages) {
                    cached = true;
                    send_event(
                        events,
                        AgentEvent::CacheHit {
                            model: self.options.model.clone(),
                        },
                    );
                    let turn = CompletedTurn {
                        text: cached_resp.text,
                        calls: cached_resp.tool_calls,
                        usage: cached_resp.usage,
                    };
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
                            cached,
                            provider_id: self.provider_id.clone(),
                            model: self.options.model.clone(),
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
                    continue;
                }
            }

            let turn = self.stream_turn(&request, events).await?;
            total_usage.add_assign(&turn.usage);

            if let Some(usage_tracker) = &self.usage {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let turn_usage = crate::TurnUsage {
                    provider: self.provider_id.clone(),
                    model: self.options.model.clone(),
                    input_tokens: turn.usage.input_tokens,
                    output_tokens: turn.usage.output_tokens,
                    cost_usd: 0.0,
                    cached,
                    timestamp,
                };
                usage_tracker.record(&self.session_id_for_usage(), turn_usage);
            }

            if !cached {
                if let Some(cache) = &self.cache {
                    let cached_resp = CachedResponse {
                        text: turn.text.clone(),
                        tool_calls: turn.calls.clone(),
                        usage: turn.usage.clone(),
                        cached_at: 0,
                    };
                    let _ = cache.put(&self.options.model, &request.messages, &cached_resp);
                }
            }

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
                    cached,
                    provider_id: self.provider_id.clone(),
                    model: self.options.model.clone(),
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

    fn session_id_for_usage(&self) -> String {
        "default".to_string()
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

    #[tokio::test]
    async fn cache_hit_returns_without_provider_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            RequestCache::open(
                &dir.path().join("cache.db"),
                crate::cache::RequestCacheConfig::default(),
            )
            .unwrap(),
        );

        let cache_messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::new(Role::User, "hello"),
        ];
        cache
            .put(
                "model",
                &cache_messages,
                &CachedResponse {
                    text: "cached response".into(),
                    tool_calls: vec![],
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                    },
                    cached_at: 0,
                },
            )
            .unwrap();

        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });

        let mut options = AgentOptions::new("model");
        options.retries = 0;
        let a = Agent::new(
            provider.clone(),
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_cache(cache);

        let outcome = a.run_new("system", "hello", None).await.unwrap();
        assert_eq!(outcome.response, "cached response");
        assert!(outcome.cached);
        assert_eq!(provider.requests.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn usage_recorded_after_turn() {
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![
                ProviderEvent::Delta("hi".into()),
                ProviderEvent::Done(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
            ]])),
            requests: Mutex::new(Vec::new()),
        });

        let usage_config = crate::UsageConfig::default();
        let tracker = Arc::new(UsageTracker::new(&usage_config));

        let mut options = AgentOptions::new("model");
        options.retries = 0;
        let a = Agent::new(
            provider,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_usage(tracker.clone());

        let outcome = a.run_new("system", "hello", None).await.unwrap();
        assert_eq!(outcome.response, "hi");
        assert!(!outcome.cached);

        let session_usage = tracker.session_usage("default").unwrap();
        assert_eq!(session_usage.total_input_tokens, 10);
        assert_eq!(session_usage.total_output_tokens, 5);
        assert_eq!(session_usage.turns.len(), 1);
    }

    #[tokio::test]
    async fn cached_response_stored_after_provider_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            RequestCache::open(
                &dir.path().join("cache.db"),
                crate::cache::RequestCacheConfig::default(),
            )
            .unwrap(),
        );

        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![
                ProviderEvent::Delta("fresh".into()),
                ProviderEvent::Done(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            ]])),
            requests: Mutex::new(Vec::new()),
        });

        let mut options = AgentOptions::new("model");
        options.retries = 0;
        let a = Agent::new(
            provider,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_cache(cache.clone());

        let outcome = a.run_new("system", "hello", None).await.unwrap();
        assert_eq!(outcome.response, "fresh");

        let cached = cache.get(
            "model",
            &[
                ChatMessage::new(Role::System, "system"),
                ChatMessage::new(Role::User, "hello"),
            ],
        );
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().text, "fresh");
    }
}
