//! Bounded provider/tool loop shared by the headless CLI and TUI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    checked_utf8_bytes, CachedResponse, ChatMessage, ChatRequest, ContextAudit, ContextManager,
    Provider, ProviderEvent, ProviderFailure, ProviderFailureKind, ProviderModelRef, RequestCache,
    Role, RuntimeConfig, ToolCall, ToolExecutor, ToolResult, Usage, UsageTracker,
};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub model: String,
    pub max_steps: usize,
    pub retries: usize,
    pub retry_delay: Duration,
    pub response_max_bytes: usize,
}

impl AgentOptions {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_steps: 12,
            retries: 2,
            retry_delay: Duration::from_millis(250),
            response_max_bytes: RuntimeConfig::default().response_max_bytes,
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
        provider: String,
        model: String,
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

/// A concrete provider/model pair that can be attempted after the primary.
///
/// The core crate owns the failover algorithm, while the composition root
/// resolves provider identifiers into these concrete handles.
#[derive(Clone)]
pub struct ProviderCandidate {
    pub provider: Arc<dyn Provider>,
    pub provider_id: String,
    pub model: String,
    health: Option<Arc<dyn ProviderHealth>>,
}

pub trait ProviderHealth: Send + Sync {
    fn is_healthy(&self, reference: &ProviderModelRef) -> bool;
    fn mark_failed(&self, reference: &ProviderModelRef, failure: &ProviderFailure);
    fn mark_success(&self, reference: &ProviderModelRef);
}

impl ProviderCandidate {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        let provider_id = provider.id().to_string();
        Self::new_with_id(provider, provider_id, model)
    }

    pub fn new_with_id(
        provider: Arc<dyn Provider>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            provider,
            model: model.into(),
            health: None,
        }
    }

    pub fn with_health(mut self, health: Arc<dyn ProviderHealth>) -> Self {
        self.health = Some(health);
        self
    }
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
    fallback_providers: Vec<ProviderCandidate>,
    fallback_health: Option<Arc<dyn ProviderHealth>>,
    usage_session_id: String,
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
            fallback_providers: Vec::new(),
            fallback_health: None,
            usage_session_id: "default".to_string(),
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

    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }

    pub fn with_fallback_chain(mut self, chain: Vec<ProviderModelRef>) -> Self {
        self.fallback_chain = chain;
        self
    }

    pub fn with_fallback_providers(mut self, providers: Vec<ProviderCandidate>) -> Self {
        self.fallback_providers = providers;
        self
    }

    pub fn with_fallback_health(mut self, health: Arc<dyn ProviderHealth>) -> Self {
        self.fallback_health = Some(health);
        self
    }

    pub fn with_usage_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.usage_session_id = session_id.into();
        self
    }

    pub fn usage_tracker(&self) -> Option<Arc<UsageTracker>> {
        self.usage.clone()
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn model(&self) -> &str {
        &self.options.model
    }

    pub async fn run_new(
        &self,
        system_prompt: impl Into<String>,
        prompt: impl Into<String>,
        events: Option<&mpsc::Sender<AgentEvent>>,
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
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Result<AgentOutcome> {
        anyhow::ensure!(
            self.options.max_steps > 0,
            "agent max_steps must be greater than zero"
        );
        messages.push(ChatMessage::new(Role::User, prompt));
        let definitions = self.tools.definitions();
        let mut total_usage = Usage::default();
        let mut cached = false;
        let mut semantic_view_cache: HashMap<(String, String), String> = HashMap::new();

        for step in 1..=self.options.max_steps {
            let (prepared, audit) = self.context.prepare(&messages).await;
            messages = prepared;
            if audit.dropped_messages > 0
                || audit.tool_outputs_truncated > 0
                || audit.summary_created
            {
                send_event(events, AgentEvent::ContextTrimmed(audit)).await?;
            }
            let request = ChatRequest::new(self.options.model.clone(), messages.clone())
                .with_tools(definitions.clone());

            if let Some(cache) = &self.cache {
                if let Some(cached_resp) = cache
                    .get_for_provider(&self.provider_id, &request.model, &request.messages)
                    .or_else(|| cache.get(&request.model, &request.messages))
                {
                    validate_response_limit(
                        &cached_resp.text,
                        &cached_resp.tool_calls,
                        self.options.response_max_bytes,
                    )?;
                    cached = true;
                    send_event(
                        events,
                        AgentEvent::CacheHit {
                            model: self.options.model.clone(),
                        },
                    )
                    .await?;
                    let turn = CompletedTurn {
                        text: cached_resp.text,
                        calls: cached_resp.tool_calls,
                        usage: cached_resp.usage,
                        provider_id: self.provider_id.clone(),
                        model: request.model.clone(),
                    };
                    total_usage.add_assign(&turn.usage);
                    if let Some(usage_tracker) = &self.usage {
                        let timestamp = unix_timestamp();
                        usage_tracker.record(
                            &self.usage_session_id,
                            crate::TurnUsage {
                                provider: turn.provider_id.clone(),
                                model: turn.model.clone(),
                                input_tokens: turn.usage.input_tokens,
                                output_tokens: turn.usage.output_tokens,
                                cost_usd: 0.0,
                                cached: true,
                                timestamp,
                            },
                        );
                    }
                    messages.push(ChatMessage::assistant(
                        turn.text.clone(),
                        turn.calls.clone(),
                    ));

                    if turn.calls.is_empty() {
                        let (messages, _) = self.context.prepare(&messages).await;
                        return Ok(AgentOutcome {
                            messages,
                            response: turn.text,
                            usage: total_usage,
                            steps: step,
                            cached,
                            provider_id: self.provider_id.clone(),
                            model: request.model.clone(),
                        });
                    }
                    if step == self.options.max_steps {
                        bail!(
                            "agent reached the maximum of {} provider steps",
                            self.options.max_steps
                        );
                    }
                    for call in turn.calls {
                        send_event(events, AgentEvent::ToolStarted(call.clone())).await?;
                        let result = self.tools.execute(&call).await;
                        send_event(events, AgentEvent::ToolFinished(result.clone())).await?;
                        self.push_tool_result_messages(
                            &call,
                            result,
                            &mut messages,
                            &mut semantic_view_cache,
                        );
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
                    provider: turn.provider_id.clone(),
                    model: turn.model.clone(),
                    input_tokens: turn.usage.input_tokens,
                    output_tokens: turn.usage.output_tokens,
                    cost_usd: 0.0,
                    cached: false,
                    timestamp,
                };
                usage_tracker.record(&self.usage_session_id, turn_usage);
            }

            if let Some(cache) = &self.cache {
                let cached_resp = CachedResponse {
                    text: turn.text.clone(),
                    tool_calls: turn.calls.clone(),
                    usage: turn.usage.clone(),
                    cached_at: 0,
                };
                let _ = cache.put_for_provider(
                    &turn.provider_id,
                    &turn.model,
                    &request.messages,
                    &cached_resp,
                );
            }

            messages.push(ChatMessage::assistant(
                turn.text.clone(),
                turn.calls.clone(),
            ));

            if turn.calls.is_empty() {
                let (messages, _) = self.context.prepare(&messages).await;
                return Ok(AgentOutcome {
                    messages,
                    response: turn.text,
                    usage: total_usage,
                    steps: step,
                    cached,
                    provider_id: turn.provider_id,
                    model: turn.model,
                });
            }
            if step == self.options.max_steps {
                bail!(
                    "agent reached the maximum of {} provider steps",
                    self.options.max_steps
                );
            }

            for call in turn.calls {
                send_event(events, AgentEvent::ToolStarted(call.clone())).await?;
                let result = self.tools.execute(&call).await;
                send_event(events, AgentEvent::ToolFinished(result.clone())).await?;
                self.push_tool_result_messages(
                    &call,
                    result,
                    &mut messages,
                    &mut semantic_view_cache,
                );
            }
        }

        unreachable!("the bounded loop always returns or errors")
    }

    async fn stream_turn(
        &self,
        request: &ChatRequest,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Result<CompletedTurn> {
        let mut candidates = vec![ProviderCandidate {
            provider: self.provider.clone(),
            provider_id: self.provider_id.clone(),
            model: request.model.clone(),
            health: self.fallback_health.clone(),
        }];
        candidates.extend(
            self.fallback_providers
                .iter()
                .filter(|candidate| {
                    candidate.provider_id != self.provider_id || candidate.model != request.model
                })
                .cloned(),
        );

        let mut last_error = None;
        for (index, candidate) in candidates.iter().enumerate() {
            let reference = ProviderModelRef::new(&candidate.provider_id, &candidate.model);
            if let Some(health) = &candidate.health {
                if !health.is_healthy(&reference) {
                    continue;
                }
            }
            let candidate_request = if candidate.model == request.model {
                request.clone()
            } else {
                let mut request = request.clone();
                request.model.clone_from(&candidate.model);
                request
            };
            match self
                .stream_candidate(candidate, &candidate_request, events)
                .await
            {
                Ok(mut turn) => {
                    if let Some(health) = &candidate.health {
                        health.mark_success(&reference);
                    }
                    turn.provider_id.clone_from(&candidate.provider_id);
                    turn.model.clone_from(&candidate.model);
                    return Ok(turn);
                }
                Err(failure)
                    if !failure.visible_output
                        && failure.failure.allows_retry_or_fallback()
                        && index + 1 < candidates.len() =>
                {
                    if let Some(health) = &candidate.health {
                        health.mark_failed(&reference, &failure.failure);
                    }
                    last_error = Some(failure.failure.clone());
                    send_event(
                        events,
                        AgentEvent::ProviderSwitched {
                            from: candidate.provider_id.clone(),
                            to: candidates[index + 1].provider_id.clone(),
                            reason: failure.failure.to_string(),
                        },
                    )
                    .await?;
                }
                Err(failure) => {
                    if failure.failure.allows_retry_or_fallback() {
                        if let Some(health) = &candidate.health {
                            health.mark_failed(&reference, &failure.failure);
                        }
                    }
                    return Err(failure.failure.into());
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    "all provider candidates failed",
                )
            })
            .into())
    }

    async fn stream_candidate(
        &self,
        candidate: &ProviderCandidate,
        request: &ChatRequest,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> std::result::Result<CompletedTurn, AttemptFailure> {
        'attempts: for attempt in 0..=self.options.retries {
            let mut stream = match candidate.provider.stream_chat(request).await {
                Ok(stream) => stream,
                Err(error) => {
                    let failure = ProviderFailure::from_error(error);
                    if failure.allows_retry_or_fallback() && attempt < self.options.retries {
                        self.retry(attempt, &failure, events).await?;
                        continue;
                    }
                    return Err(AttemptFailure {
                        failure,
                        visible_output: false,
                    });
                }
            };
            let mut text = String::new();
            let mut calls = Vec::new();
            let mut usage = None;
            let mut visible_output = false;
            let mut tool_argument_bytes = 0;

            while let Some(event) = stream.next().await {
                match event {
                    ProviderEvent::Delta(delta) => {
                        if checked_utf8_bytes(text.len(), &delta, self.options.response_max_bytes)
                            .is_none()
                        {
                            return Err(AttemptFailure::protocol(
                                "provider response exceeds configured byte limit",
                                visible_output,
                            ));
                        }
                        visible_output = true;
                        text.push_str(&delta);
                        send_event(events, AgentEvent::Delta(delta))
                            .await
                            .map_err(|error| {
                                AttemptFailure::event_channel(error, visible_output)
                            })?;
                    }
                    ProviderEvent::ToolCall(call) => {
                        let arguments =
                            serde_json::to_string(&call.arguments).map_err(|error| {
                                AttemptFailure::protocol_with_source(
                                    "provider tool arguments are not serializable",
                                    error,
                                    visible_output,
                                )
                            })?;
                        tool_argument_bytes = checked_utf8_bytes(
                            tool_argument_bytes,
                            &arguments,
                            self.options.response_max_bytes,
                        )
                        .ok_or_else(|| {
                            AttemptFailure::protocol(
                                "provider tool arguments exceed configured byte limit",
                                visible_output,
                            )
                        })?;
                        visible_output = true;
                        calls.push(call);
                    }
                    ProviderEvent::Done(done_usage) => usage = Some(done_usage),
                    ProviderEvent::Error(failure)
                        if !visible_output
                            && failure.allows_retry_or_fallback()
                            && attempt < self.options.retries =>
                    {
                        self.retry(attempt, &failure, events).await?;
                        continue 'attempts;
                    }
                    ProviderEvent::Error(failure) => {
                        return Err(AttemptFailure {
                            failure,
                            visible_output,
                        })
                    }
                }
            }

            if usage.is_none() {
                return Err(AttemptFailure {
                    failure: ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        "provider stream ended before completion",
                    ),
                    visible_output,
                });
            }

            return Ok(CompletedTurn {
                text,
                calls,
                usage: usage.expect("checked above"),
                provider_id: candidate.provider_id.clone(),
                model: candidate.model.clone(),
            });
        }

        unreachable!("the retry loop always returns or errors")
    }

    async fn retry(
        &self,
        zero_based_attempt: usize,
        failure: &ProviderFailure,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> std::result::Result<(), AttemptFailure> {
        send_event(
            events,
            AgentEvent::Retry {
                attempt: zero_based_attempt + 2,
                error: failure.to_string(),
            },
        )
        .await
        .map_err(|error| AttemptFailure::event_channel(error, false))?;
        tokio::time::sleep(self.options.retry_delay).await;
        Ok(())
    }

    fn push_tool_result_messages(
        &self,
        call: &ToolCall,
        result: ToolResult,
        messages: &mut Vec<ChatMessage>,
        semantic_view_cache: &mut HashMap<(String, String), String>,
    ) {
        messages.push(ChatMessage::tool_result(call, result.model_content()));
        let Some(summary) = Self::semantic_view_summary(call, &result) else {
            return;
        };
        let key = (summary.tool.clone(), summary.path.clone());
        let should_emit = semantic_view_cache
            .get(&key)
            .is_none_or(|cached| cached != &summary.message);
        if should_emit {
            semantic_view_cache.insert(key, summary.message.clone());
            messages.push(ChatMessage::new(Role::System, summary.message));
        }
    }

    fn semantic_view_summary(
        call: &ToolCall,
        result: &ToolResult,
    ) -> Option<LspSemanticViewSummary> {
        if !call.name.starts_with("lsp_") || !result.success {
            return None;
        }

        #[derive(serde::Deserialize)]
        struct CompactToolOutput {
            tool: String,
            path: String,
            count: usize,
            shown: usize,
            truncated: bool,
            compact: Value,
        }

        let parsed = serde_json::from_str::<CompactToolOutput>(&result.output).ok()?;
        if parsed.tool != call.name {
            return None;
        }

        let message = compact_lsp_summary(
            &parsed.tool,
            &parsed.path,
            parsed.count,
            parsed.shown,
            parsed.truncated,
            parsed.compact,
        );
        Some(LspSemanticViewSummary {
            tool: parsed.tool,
            path: parsed.path,
            message,
        })
    }
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

struct LspSemanticViewSummary {
    tool: String,
    path: String,
    message: String,
}

fn compact_lsp_summary(
    tool: &str,
    path: &str,
    count: usize,
    shown: usize,
    truncated: bool,
    compact: Value,
) -> String {
    let mut message = format!("[semantic-view] {tool} path={path} shown={shown} total={count}");
    if truncated {
        message.push_str(" truncated");
    }
    for line in compact_preview(tool, &compact) {
        message.push('\n');
        message.push_str(&line);
    }
    message
}

fn compact_preview(tool: &str, compact: &Value) -> Vec<String> {
    compact
        .as_array()
        .map(|items| {
            items
                .iter()
                .take(3)
                .filter_map(|item| compact_preview_line(tool, item))
                .collect()
        })
        .unwrap_or_default()
}

fn compact_preview_line(tool: &str, item: &Value) -> Option<String> {
    match tool {
        "lsp_diagnostics" => {
            let message = item.get("message")?.as_str()?;
            let line = item
                .get("line")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let severity = item
                .get("severity")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            Some(format!(
                "- line {line} [{severity}] {}",
                compact_text(message, 120)
            ))
        }
        "lsp_definition" | "lsp_references" => {
            let path = item
                .get("path")
                .or_else(|| item.get("uri"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let line = item
                .get("start_line")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            Some(format!("- {path}:{line}"))
        }
        "lsp_symbols" => {
            let name = item.get("name").and_then(|value| value.as_str())?;
            let kind = item
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(format!("- {name} {kind}"))
        }
        "lsp_hover" | "lsp_rename" => {
            let text = item.get("text")?.as_str()?;
            Some(format!("- {}", compact_text(text, 120)))
        }
        _ => None,
    }
}

fn compact_text(message: &str, max_chars: usize) -> String {
    let normalized = message.replace('\n', " ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let keep = max_chars.saturating_sub(1);
    let mut compact: String = normalized.chars().take(keep).collect();
    compact.push('…');
    compact
}

struct CompletedTurn {
    text: String,
    calls: Vec<ToolCall>,
    usage: Usage,
    provider_id: String,
    model: String,
}

fn validate_response_limit(
    text: &str,
    calls: &[ToolCall],
    response_max_bytes: usize,
) -> std::result::Result<(), ProviderFailure> {
    if checked_utf8_bytes(0, text, response_max_bytes).is_none() {
        return Err(ProviderFailure::new(
            ProviderFailureKind::Protocol,
            "provider response exceeds configured byte limit",
        ));
    }
    let mut bytes = 0;
    for call in calls {
        let arguments = serde_json::to_string(&call.arguments).map_err(|error| {
            ProviderFailure::with_source(
                ProviderFailureKind::Protocol,
                "provider tool arguments are not serializable",
                error,
            )
        })?;
        bytes = checked_utf8_bytes(bytes, &arguments, response_max_bytes).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureKind::Protocol,
                "provider tool arguments exceed configured byte limit",
            )
        })?;
    }
    Ok(())
}

struct AttemptFailure {
    failure: ProviderFailure,
    visible_output: bool,
}

impl AttemptFailure {
    fn protocol(message: impl Into<String>, visible_output: bool) -> Self {
        Self {
            failure: ProviderFailure::new(ProviderFailureKind::Protocol, message),
            visible_output,
        }
    }

    fn protocol_with_source(
        message: impl Into<String>,
        source: impl std::fmt::Display,
        visible_output: bool,
    ) -> Self {
        Self {
            failure: ProviderFailure::with_source(ProviderFailureKind::Protocol, message, source),
            visible_output,
        }
    }

    fn event_channel(error: anyhow::Error, visible_output: bool) -> Self {
        Self::protocol_with_source("agent event receiver closed", error, visible_output)
    }
}

async fn send_event(events: Option<&mpsc::Sender<AgentEvent>>, event: AgentEvent) -> Result<()> {
    if let Some(events) = events {
        events
            .send(event)
            .await
            .map_err(|_| anyhow::anyhow!("agent event receiver closed"))?;
    }
    Ok(())
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

    struct ReusableTools {
        output: String,
    }

    #[async_trait]
    impl ToolExecutor for ReusableTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "lsp_diagnostics".into(),
                description: "lsp".into(),
                input_schema: serde_json::json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            }]
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            if call.name != "lsp_diagnostics" {
                ToolResult::failure(call, "unsupported tool")
            } else {
                ToolResult::success(call, &self.output)
            }
        }
    }

    struct FixedProvider {
        id: &'static str,
        events: Vec<ProviderEvent>,
    }

    #[async_trait]
    impl Provider for FixedProvider {
        fn id(&self) -> &str {
            self.id
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            Ok(Box::pin(stream::iter(self.events.clone())))
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
    async fn injects_semantic_view_for_repeated_lsp_diagnostics_tool_calls() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "lsp_diagnostics".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        let call_revisit = ToolCall {
            id: "call-2".into(),
            name: "lsp_diagnostics".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        let compact = serde_json::json!({
            "tool": "lsp_diagnostics",
            "path": "src/main.rs",
            "count": 1,
            "shown": 1,
            "truncated": false,
            "compact": [
                {"message": "unused variable", "line": 12, "severity": "warning"}
            ]
        })
        .to_string();
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([
                vec![
                    ProviderEvent::ToolCall(call.clone()),
                    ProviderEvent::Done(Usage::default()),
                ],
                vec![
                    ProviderEvent::ToolCall(call_revisit.clone()),
                    ProviderEvent::Done(Usage::default()),
                ],
                vec![
                    ProviderEvent::Delta("ok".into()),
                    ProviderEvent::Done(Usage::default()),
                ],
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut options = AgentOptions::new("model");
        options.max_steps = 3;
        options.retries = 0;
        let outcome = Agent::new(
            provider,
            Arc::new(ReusableTools { output: compact }),
            ContextManager::default(),
            options,
        )
        .run_new("system", "please inspect", None)
        .await
        .unwrap();

        let system_views = outcome
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .filter(|message| message.content.starts_with("[semantic-view]"));
        assert_eq!(system_views.count(), 1);
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
                ProviderEvent::Error(ProviderFailure::new(
                    ProviderFailureKind::Transport,
                    "disconnect",
                )),
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
    async fn bounded_event_queue_backpressures_without_losing_deltas() {
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([vec![
                ProviderEvent::Delta("甲".into()),
                ProviderEvent::Delta("乙".into()),
                ProviderEvent::Done(Usage::default()),
            ]])),
            requests: Mutex::new(Vec::new()),
        });
        let agent = agent(provider, 1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let run =
            tokio::spawn(async move { agent.run_new("system", "hello", Some(&events_tx)).await });

        tokio::task::yield_now().await;
        assert!(!run.is_finished(), "producer must wait for capacity");
        assert!(matches!(events_rx.recv().await, Some(AgentEvent::Delta(text)) if text == "甲"));
        assert!(matches!(events_rx.recv().await, Some(AgentEvent::Delta(text)) if text == "乙"));
        let outcome = run.await.unwrap().unwrap();
        assert_eq!(outcome.response, "甲乙");
    }

    #[tokio::test]
    async fn bounded_agent_response_and_tool_arguments_fail_as_protocol() {
        for events in [
            vec![
                ProviderEvent::Delta("甲乙".into()),
                ProviderEvent::Done(Usage::default()),
            ],
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "too-long"}),
                }),
                ProviderEvent::Done(Usage::default()),
            ],
        ] {
            let provider = Arc::new(ScriptedProvider {
                turns: Mutex::new(VecDeque::from([events])),
                requests: Mutex::new(Vec::new()),
            });
            let mut options = AgentOptions::new("model");
            options.retries = 0;
            options.response_max_bytes = 3;
            let agent = Agent::new(
                provider,
                Arc::new(ScriptedTools),
                ContextManager::default(),
                options,
            );

            let error = agent.run_new("system", "hello", None).await.unwrap_err();
            let failure = error.downcast_ref::<ProviderFailure>().unwrap();
            assert_eq!(failure.kind(), ProviderFailureKind::Protocol);
        }
    }

    #[tokio::test]
    async fn bounded_outcome_retains_only_context_prepared_history() {
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::from([
                vec![
                    ProviderEvent::Delta("first".into()),
                    ProviderEvent::Done(Usage::default()),
                ],
                vec![
                    ProviderEvent::Delta("second".into()),
                    ProviderEvent::Done(Usage::default()),
                ],
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let config = crate::ContextConfig {
            max_tokens: 24,
            system_budget: 4,
            history_budget: 12,
            tool_output_budget: 4,
            input_budget: 4,
            summary_threshold: usize::MAX,
            summary_max_messages: 0,
        };
        let mut options = AgentOptions::new("model");
        options.retries = 0;
        let agent = Agent::new(
            provider,
            Arc::new(ScriptedTools),
            ContextManager::with_budget(config, Arc::new(crate::CharApproxCounter), None, "model"),
            options,
        );
        let mut history = vec![ChatMessage::new(Role::System, "system")];
        for index in 0..20 {
            history.push(ChatMessage::new(Role::User, format!("old-message-{index}")));
            history.push(ChatMessage::new(Role::Assistant, "old-response"));
        }
        let original_len = history.len();

        let first = agent
            .continue_messages(history, "new prompt", None)
            .await
            .unwrap();
        assert!(first.messages.len() < original_len);
        let first_len = first.messages.len();
        let second = agent
            .continue_messages(first.messages, "next prompt", None)
            .await
            .unwrap();
        assert!(second.messages.len() <= first_len + 1);
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
    async fn bounded_cache_hit_is_rejected_by_shared_response_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            RequestCache::open(
                &dir.path().join("cache.db"),
                crate::cache::RequestCacheConfig::default(),
            )
            .unwrap(),
        );
        let messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::new(Role::User, "hello"),
        ];
        cache
            .put(
                "model",
                &messages,
                &CachedResponse {
                    text: "甲乙".into(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    cached_at: 0,
                },
            )
            .unwrap();
        let provider = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });
        let mut options = AgentOptions::new("model");
        options.response_max_bytes = 3;
        let agent = Agent::new(
            provider,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_cache(cache);

        let error = agent.run_new("system", "hello", None).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProviderFailure>().unwrap().kind(),
            ProviderFailureKind::Protocol
        );
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

        let cached = cache.get_for_provider(
            "scripted",
            "model",
            &[
                ChatMessage::new(Role::System, "system"),
                ChatMessage::new(Role::User, "hello"),
            ],
        );
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().text, "fresh");
    }

    #[tokio::test]
    async fn switches_to_fallback_provider_before_visible_output() {
        let primary = Arc::new(FixedProvider {
            id: "primary",
            events: vec![ProviderEvent::Error(ProviderFailure::new(
                ProviderFailureKind::Server,
                "primary unavailable",
            ))],
        });
        let fallback = Arc::new(FixedProvider {
            id: "fallback",
            events: vec![
                ProviderEvent::Delta("fallback response".into()),
                ProviderEvent::Done(Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                }),
            ],
        });
        let mut options = AgentOptions::new("primary-model");
        options.retries = 0;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_providers(vec![ProviderCandidate::new(fallback, "fallback-model")]);
        let (events_tx, mut events_rx) = mpsc::channel(8);

        let outcome = agent
            .run_new("system", "hello", Some(&events_tx))
            .await
            .unwrap();
        assert_eq!(outcome.response, "fallback response");
        assert_eq!(outcome.provider_id, "fallback");
        assert_eq!(outcome.model, "fallback-model");
        let mut switched = false;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::ProviderSwitched { from, to, .. } = event {
                switched = from == "primary" && to == "fallback";
            }
        }
        assert!(switched);
    }

    struct CountingFailureProvider {
        id: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        events: Vec<ProviderEvent>,
    }

    struct StartupFailureProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        failure: ProviderFailure,
    }

    #[derive(Default)]
    struct ThresholdHealth {
        failures: std::sync::atomic::AtomicUsize,
    }

    impl ProviderHealth for ThresholdHealth {
        fn is_healthy(&self, _reference: &ProviderModelRef) -> bool {
            self.failures.load(std::sync::atomic::Ordering::SeqCst) < 3
        }

        fn mark_failed(&self, _reference: &ProviderModelRef, _failure: &ProviderFailure) {
            self.failures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn mark_success(&self, _reference: &ProviderModelRef) {
            self.failures.store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Provider for StartupFailureProvider {
        fn id(&self) -> &str {
            "startup-failure"
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(self.failure.clone().into())
        }
    }

    #[async_trait]
    impl Provider for CountingFailureProvider {
        fn id(&self) -> &str {
            self.id
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(stream::iter(self.events.clone())))
        }
    }

    fn failure(kind: ProviderFailureKind, message: &str) -> ProviderEvent {
        ProviderEvent::Error(ProviderFailure::new(kind, message))
    }

    #[tokio::test]
    async fn auth_failure_does_not_retry_or_fallback() {
        assert_no_retry_or_fallback(ProviderFailureKind::Auth).await;
    }

    #[tokio::test]
    async fn authorization_failure_does_not_retry_or_fallback() {
        assert_no_retry_or_fallback(ProviderFailureKind::Authorization).await;
    }

    #[tokio::test]
    async fn startup_auth_failure_does_not_retry_or_fallback() {
        let primary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary = Arc::new(StartupFailureProvider {
            calls: primary_calls.clone(),
            failure: ProviderFailure::new(ProviderFailureKind::Auth, "login required"),
        });
        let fallback = Arc::new(CountingFailureProvider {
            id: "fallback",
            calls: fallback_calls.clone(),
            events: vec![ProviderEvent::Done(Usage::default())],
        });
        let mut options = AgentOptions::new("model");
        options.retries = 2;
        options.retry_delay = Duration::ZERO;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_providers(vec![ProviderCandidate::new(fallback, "fallback-model")]);

        let error = agent.run_new("system", "hello", None).await.unwrap_err();

        assert!(error.to_string().contains("login required"));
        assert_eq!(primary_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn repeated_auth_failures_never_poison_health_or_fallback() {
        let primary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let health = Arc::new(ThresholdHealth::default());
        let primary = Arc::new(CountingFailureProvider {
            id: "primary",
            calls: primary_calls.clone(),
            events: vec![failure(ProviderFailureKind::Auth, "login required")],
        });
        let fallback = Arc::new(CountingFailureProvider {
            id: "fallback",
            calls: fallback_calls.clone(),
            events: vec![ProviderEvent::Done(Usage::default())],
        });
        let mut options = AgentOptions::new("model");
        options.retries = 0;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_health(health.clone())
        .with_fallback_providers(vec![
            ProviderCandidate::new(fallback, "fallback-model").with_health(health)
        ]);

        for _ in 0..4 {
            assert!(agent
                .run_new("system", "secret prompt", None)
                .await
                .is_err());
        }

        assert_eq!(primary_calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn protocol_failure_does_not_retry_or_fallback() {
        assert_no_retry_or_fallback(ProviderFailureKind::Protocol).await;
    }

    async fn assert_no_retry_or_fallback(kind: ProviderFailureKind) {
        let primary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary = Arc::new(CountingFailureProvider {
            id: "primary",
            calls: primary_calls.clone(),
            events: vec![failure(kind, "classified failure")],
        });
        let fallback = Arc::new(CountingFailureProvider {
            id: "fallback",
            calls: fallback_calls.clone(),
            events: vec![ProviderEvent::Done(Usage::default())],
        });
        let mut options = AgentOptions::new("model");
        options.retries = 2;
        options.retry_delay = Duration::ZERO;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_providers(vec![ProviderCandidate::new(fallback, "fallback-model")]);

        let error = agent.run_new("system", "hello", None).await.unwrap_err();

        assert!(error.to_string().contains("classified failure"));
        assert_eq!(primary_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn transient_failures_retry_then_fallback_before_visible_output() {
        for kind in [
            ProviderFailureKind::RateLimit,
            ProviderFailureKind::Transport,
            ProviderFailureKind::Server,
        ] {
            assert_transient_failure_retries_then_fallback(kind).await;
        }
    }

    async fn assert_transient_failure_retries_then_fallback(kind: ProviderFailureKind) {
        let primary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary = Arc::new(CountingFailureProvider {
            id: "primary",
            calls: primary_calls.clone(),
            events: vec![failure(kind, "transient failure")],
        });
        let fallback = Arc::new(CountingFailureProvider {
            id: "fallback",
            calls: fallback_calls.clone(),
            events: vec![
                ProviderEvent::Delta("fallback".into()),
                ProviderEvent::Done(Usage::default()),
            ],
        });
        let mut options = AgentOptions::new("model");
        options.retries = 2;
        options.retry_delay = Duration::ZERO;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_providers(vec![ProviderCandidate::new(fallback, "fallback-model")]);

        let outcome = agent.run_new("system", "hello", None).await.unwrap();

        assert_eq!(outcome.response, "fallback");
        assert_eq!(primary_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn visible_output_failure_never_fallback() {
        let primary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary = Arc::new(CountingFailureProvider {
            id: "primary",
            calls: primary_calls.clone(),
            events: vec![
                ProviderEvent::Delta("partial".into()),
                failure(ProviderFailureKind::Server, "server failed"),
            ],
        });
        let fallback = Arc::new(CountingFailureProvider {
            id: "fallback",
            calls: fallback_calls.clone(),
            events: vec![ProviderEvent::Done(Usage::default())],
        });
        let mut options = AgentOptions::new("model");
        options.retries = 2;
        options.retry_delay = Duration::ZERO;
        let agent = Agent::new(
            primary,
            Arc::new(ScriptedTools),
            ContextManager::default(),
            options,
        )
        .with_fallback_providers(vec![ProviderCandidate::new(fallback, "fallback-model")]);

        let error = agent.run_new("system", "hello", None).await.unwrap_err();

        assert!(error.to_string().contains("server failed"));
        assert_eq!(primary_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
