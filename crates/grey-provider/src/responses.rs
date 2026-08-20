//! OpenAI Responses streaming adapter for API-key compatible endpoints.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use grey_core::{
    checked_utf8_bytes, ChatMessage, ChatRequest, Provider, ProviderEvent, ProviderFailure,
    ProviderFailureKind, Role, RuntimeConfig, ToolCall, Usage,
};
use serde_json::{json, Value};

use crate::sse::SseDecoder;

pub struct ResponsesProvider {
    base_url: String,
    api_key: Option<String>,
    response_max_bytes: usize,
    client: reqwest::Client,
}

impl ResponsesProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("building OpenAI Responses HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            response_max_bytes: RuntimeConfig::default().response_max_bytes,
            client,
        })
    }

    pub fn with_response_max_bytes(mut self, response_max_bytes: usize) -> Self {
        self.response_max_bytes = response_max_bytes;
        self
    }

    fn build_request(&self, request: &ChatRequest) -> Result<reqwest::Request> {
        let body = request_body(request).map_err(anyhow::Error::new)?;
        let base = self.base_url.trim_end_matches('/');
        let url = if base.ends_with("/responses") {
            base.to_string()
        } else {
            format!("{base}/responses")
        };
        let mut builder = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        builder.build().map_err(|error| {
            ProviderFailure::with_source(
                ProviderFailureKind::Protocol,
                "building OpenAI Responses request failed",
                error,
            )
            .into()
        })
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn id(&self) -> &str {
        "openai_responses"
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<futures_util::stream::BoxStream<'a, ProviderEvent>> {
        let http_request = self.build_request(request)?;
        let response =
            crate::send_http(&self.client, http_request, "OpenAI Responses provider").await?;
        let mut chunks = response.bytes_stream();
        let output = async_stream::stream! {
            let mut decoder = SseDecoder::default();
            let mut state = ResponsesStreamState::with_limit(self.response_max_bytes);
            while let Some(chunk) = chunks.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(event) = state.fail(ProviderFailure::with_source(
                            ProviderFailureKind::Transport,
                            "OpenAI Responses stream transport failed",
                            error,
                        )) {
                            yield event;
                        }
                        return;
                    }
                };
                let payloads = match decoder.feed(&chunk) {
                    Ok(payloads) => payloads,
                    Err(error) => {
                        if let Some(event) = state.fail(ProviderFailure::with_source(
                            ProviderFailureKind::Protocol,
                            "OpenAI Responses SSE framing is malformed",
                            error,
                        )) {
                            yield event;
                        }
                        return;
                    }
                };
                let events = consume_batch(&mut state, payloads);
                let terminal = events.iter().any(is_terminal_event);
                for event in events {
                    yield event;
                }
                if terminal {
                    return;
                }
            }
            if let Err(error) = decoder.finish() {
                if let Some(event) = state.fail(ProviderFailure::with_source(
                    ProviderFailureKind::Protocol,
                    "OpenAI Responses SSE stream ended with an incomplete event",
                    error,
                )) {
                    yield event;
                }
                return;
            }
            for event in state.finish() {
                yield event;
            }
        };
        Ok(Box::pin(output))
    }
}

fn request_body(request: &ChatRequest) -> std::result::Result<Value, ProviderFailure> {
    let mut input = Vec::new();
    for message in &request.messages {
        input.extend(response_items(message)?);
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    Ok(body)
}

fn response_items(message: &ChatMessage) -> std::result::Result<Vec<Value>, ProviderFailure> {
    match message.role {
        Role::System | Role::User => Ok(vec![json!({
            "type": "message",
            "role": match message.role { Role::System => "system", _ => "user" },
            "content": message.content,
        })]),
        Role::Assistant => {
            let mut items = vec![json!({
                "type": "message",
                "role": "assistant",
                "content": message.content,
            })];
            for call in &message.tool_calls {
                if call.id.trim().is_empty() || call.name.trim().is_empty() {
                    return Err(ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        "OpenAI Responses assistant function call is missing id or name",
                    ));
                }
                let arguments = serde_json::to_string(&call.arguments).map_err(|error| {
                    ProviderFailure::with_source(
                        ProviderFailureKind::Protocol,
                        "serializing OpenAI Responses function call arguments failed",
                        error,
                    )
                })?;
                items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": arguments,
                }));
            }
            Ok(items)
        }
        Role::Tool => {
            let call_id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        "OpenAI Responses tool message is missing tool_call_id",
                    )
                })?;
            Ok(vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": message.content,
            })])
        }
    }
}

#[derive(Debug)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
    done: bool,
}

#[derive(Debug)]
struct ResponsesStreamState {
    pending_by_item_id: HashMap<String, PendingCall>,
    seen_call_ids: HashSet<String>,
    tool_bytes: usize,
    text_bytes: usize,
    response_max_bytes: usize,
    terminal: bool,
    duplicate_reported: bool,
}

impl ResponsesStreamState {
    fn with_limit(response_max_bytes: usize) -> Self {
        Self {
            pending_by_item_id: HashMap::new(),
            seen_call_ids: HashSet::new(),
            tool_bytes: 0,
            text_bytes: 0,
            response_max_bytes,
            terminal: false,
            duplicate_reported: false,
        }
    }

    fn consume(&mut self, payload: &str) -> Vec<ProviderEvent> {
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return self
                    .protocol_error_with_source("malformed OpenAI Responses SSE payload", error)
            }
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses event is missing type");
        };
        if self.terminal {
            return if is_semantic_event(event_type) && !self.duplicate_reported {
                self.duplicate_reported = true;
                vec![ProviderEvent::Error(ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    format!("OpenAI Responses event `{event_type}` arrived after terminal"),
                ))]
            } else {
                Vec::new()
            };
        }

        match event_type {
            "response.output_item.added" => self.add_call(&value),
            "response.function_call_arguments.delta" => self.add_arguments(&value),
            "response.function_call_arguments.done" => self.finish_call(&value),
            "response.output_text.delta" => self.add_text(&value),
            "response.completed" => self.complete(&value),
            "response.failed" => self.failed(&value),
            "response.incomplete" => self.incomplete(&value),
            "error" => self.top_level_error(&value),
            _ => Vec::new(),
        }
    }

    fn add_call(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(item) = value.get("item").and_then(Value::as_object) else {
            return self.protocol_error("OpenAI Responses output item is malformed");
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Vec::new();
        }
        let fields = (
            item.get("id").and_then(Value::as_str),
            item.get("call_id").and_then(Value::as_str),
            item.get("name").and_then(Value::as_str),
        );
        let (Some(item_id), Some(call_id), Some(name)) = fields else {
            return self.protocol_error("OpenAI Responses function call metadata is incomplete");
        };
        if item_id.trim().is_empty() || call_id.trim().is_empty() || name.trim().is_empty() {
            return self.protocol_error("OpenAI Responses function call metadata is empty");
        }
        let arguments = match item.get("arguments") {
            None => "",
            Some(Value::String(arguments)) => arguments,
            Some(_) => return self.protocol_error("OpenAI Responses arguments must be a string"),
        };
        if self.pending_by_item_id.contains_key(item_id) || self.seen_call_ids.contains(call_id) {
            return self.protocol_error("OpenAI Responses function call id is duplicated");
        }
        if self.pending_by_item_id.len() >= crate::MAX_TOOL_CALLS {
            return self.protocol_error(format!(
                "OpenAI Responses stream exceeds {} tool calls",
                crate::MAX_TOOL_CALLS
            ));
        }
        let mut next = self.tool_bytes;
        // call_id is retained both in the pending call and the duplicate-id set.
        for part in [item_id, call_id, call_id, name, arguments] {
            let Some(checked) = checked_utf8_bytes(next, part, self.response_max_bytes) else {
                return self
                    .protocol_error("OpenAI Responses tool call exceeds configured byte limit");
            };
            next = checked;
        }
        self.pending_by_item_id.insert(
            item_id.to_string(),
            PendingCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
                done: false,
            },
        );
        self.seen_call_ids.insert(call_id.to_string());
        self.tool_bytes = next;
        Vec::new()
    }

    fn add_arguments(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(item_id) = value.get("item_id").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses arguments delta is missing item_id");
        };
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses arguments delta is missing delta");
        };
        let Some(call) = self.pending_by_item_id.get(item_id) else {
            return self.protocol_error("OpenAI Responses arguments delta has unknown item_id");
        };
        if call.done {
            return self.protocol_error("OpenAI Responses arguments delta arrived after done");
        }
        let Some(next) = checked_utf8_bytes(self.tool_bytes, delta, self.response_max_bytes) else {
            return self.protocol_error("OpenAI Responses tool call exceeds configured byte limit");
        };
        self.pending_by_item_id
            .get_mut(item_id)
            .expect("pending call was checked")
            .arguments
            .push_str(delta);
        self.tool_bytes = next;
        Vec::new()
    }

    fn finish_call(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(item_id) = value.get("item_id").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses arguments done is missing item_id");
        };
        let Some(final_arguments) = value.get("arguments").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses arguments done is missing arguments");
        };
        let Some(call) = self.pending_by_item_id.get(item_id) else {
            return self.protocol_error("OpenAI Responses arguments done has unknown item_id");
        };
        if call.done {
            return self.protocol_error("OpenAI Responses function call completed twice");
        }
        if let Some(name) = value.get("name") {
            let Some(name) = name.as_str() else {
                return self.protocol_error("OpenAI Responses done name is malformed");
            };
            if name != call.name {
                return self.protocol_error("OpenAI Responses function call metadata changed");
            }
        }
        if let Some(call_id) = value.get("call_id") {
            let Some(call_id) = call_id.as_str() else {
                return self.protocol_error("OpenAI Responses done call_id is malformed");
            };
            if call_id != call.call_id {
                return self.protocol_error("OpenAI Responses function call metadata changed");
            }
        }
        let append_final = call.arguments.is_empty() && !final_arguments.is_empty();
        if !append_final && call.arguments != final_arguments {
            return self.protocol_error("OpenAI Responses final arguments do not match deltas");
        }
        if append_final
            && checked_utf8_bytes(self.tool_bytes, final_arguments, self.response_max_bytes)
                .is_none()
        {
            return self.protocol_error("OpenAI Responses tool call exceeds configured byte limit");
        }
        let arguments: Value = match serde_json::from_str(final_arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return self.protocol_error_with_source(
                    "OpenAI Responses function arguments are malformed",
                    error,
                )
            }
        };
        let call = self
            .pending_by_item_id
            .get_mut(item_id)
            .expect("pending call was checked");
        if append_final {
            call.arguments.push_str(final_arguments);
            self.tool_bytes += final_arguments.len();
        }
        call.done = true;
        let event = ProviderEvent::ToolCall(ToolCall {
            id: call.call_id.clone(),
            name: call.name.clone(),
            arguments,
        });
        call.arguments = String::new();
        vec![event]
    }

    fn add_text(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return self.protocol_error("OpenAI Responses text delta is missing delta");
        };
        if delta.is_empty() {
            return Vec::new();
        }
        let Some(next) = checked_utf8_bytes(self.text_bytes, delta, self.response_max_bytes) else {
            return self.protocol_error("OpenAI Responses text exceeds configured byte limit");
        };
        self.text_bytes = next;
        vec![ProviderEvent::Delta(delta.to_string())]
    }

    fn complete(&mut self, value: &Value) -> Vec<ProviderEvent> {
        if self.pending_by_item_id.values().any(|call| !call.done) {
            return self.protocol_error("OpenAI Responses completed with unfinished tool calls");
        }
        let Some(usage) = value
            .get("response")
            .and_then(|response| response.get("usage"))
        else {
            return self.protocol_error("OpenAI Responses completed event is missing usage");
        };
        let (Some(input_tokens), Some(output_tokens)) = (
            usage.get("input_tokens").and_then(Value::as_u64),
            usage.get("output_tokens").and_then(Value::as_u64),
        ) else {
            return self.protocol_error("OpenAI Responses usage is malformed");
        };
        self.terminal = true;
        vec![ProviderEvent::Done(Usage {
            input_tokens,
            output_tokens,
        })]
    }

    fn failed(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(error) = value
            .get("response")
            .and_then(|response| response.get("error"))
        else {
            return self.protocol_error("OpenAI Responses failed event is missing error");
        };
        self.structured_error(error)
    }

    fn incomplete(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(reason) = value
            .get("response")
            .and_then(|response| response.get("incomplete_details"))
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        else {
            return self.protocol_error("OpenAI Responses incomplete event is malformed");
        };
        self.terminal = true;
        vec![ProviderEvent::Error(bounded_wire_failure(
            ProviderFailureKind::Protocol,
            "incomplete",
            reason,
            self.response_max_bytes,
        ))]
    }

    fn top_level_error(&mut self, value: &Value) -> Vec<ProviderEvent> {
        self.structured_error(value)
    }

    fn structured_error(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let (Some(code), Some(message)) = (
            value.get("code").and_then(Value::as_str),
            value.get("message").and_then(Value::as_str),
        ) else {
            return self.protocol_error("OpenAI Responses error event is malformed");
        };
        if code.trim().is_empty() || message.trim().is_empty() {
            return self.protocol_error("OpenAI Responses error code or message is empty");
        }
        self.terminal = true;
        vec![ProviderEvent::Error(bounded_wire_failure(
            classify_error_code(code),
            code,
            message,
            self.response_max_bytes,
        ))]
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.terminal {
            Vec::new()
        } else {
            self.protocol_error("stream ended before OpenAI Responses terminal event")
        }
    }

    fn fail(&mut self, failure: ProviderFailure) -> Option<ProviderEvent> {
        if self.terminal {
            None
        } else {
            self.terminal = true;
            Some(ProviderEvent::Error(failure))
        }
    }

    fn protocol_error(&mut self, message: impl Into<String>) -> Vec<ProviderEvent> {
        self.fail(ProviderFailure::new(ProviderFailureKind::Protocol, message))
            .into_iter()
            .collect()
    }

    fn protocol_error_with_source(
        &mut self,
        message: impl Into<String>,
        source: impl std::fmt::Display,
    ) -> Vec<ProviderEvent> {
        self.fail(ProviderFailure::with_source(
            ProviderFailureKind::Protocol,
            message,
            source,
        ))
        .into_iter()
        .collect()
    }
}

fn classify_error_code(code: &str) -> ProviderFailureKind {
    match code {
        "invalid_api_key" | "authentication_error" => ProviderFailureKind::Auth,
        "insufficient_permissions" => ProviderFailureKind::Authorization,
        "rate_limit_exceeded" => ProviderFailureKind::RateLimit,
        "server_error" => ProviderFailureKind::Server,
        _ => ProviderFailureKind::Protocol,
    }
}

fn bounded_wire_failure(
    kind: ProviderFailureKind,
    code: &str,
    message: &str,
    limit: usize,
) -> ProviderFailure {
    const PREFIX: &str = "OpenAI Responses error: ";
    let capacity = PREFIX
        .len()
        .saturating_add(code.len())
        .saturating_add(2)
        .saturating_add(message.len())
        .min(limit);
    let mut diagnostic = String::with_capacity(capacity);
    for part in [PREFIX, code, ": ", message] {
        let remaining = limit.saturating_sub(diagnostic.len());
        let mut end = part.len().min(remaining);
        while !part.is_char_boundary(end) {
            end -= 1;
        }
        diagnostic.push_str(&part[..end]);
        if diagnostic.len() == limit {
            break;
        }
    }
    let diagnostic = grey_core::redact_provider_secrets(&diagnostic);
    let mut final_end = diagnostic.len().min(limit);
    while !diagnostic.is_char_boundary(final_end) {
        final_end -= 1;
    }
    ProviderFailure::new(kind, &diagnostic[..final_end])
}

fn is_semantic_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.output_item.added"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.output_text.delta"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "error"
    )
}

fn is_terminal_event(event: &ProviderEvent) -> bool {
    matches!(event, ProviderEvent::Done(_) | ProviderEvent::Error(_))
}

fn consume_batch(
    state: &mut ResponsesStreamState,
    payloads: impl IntoIterator<Item = String>,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    for payload in payloads {
        for event in state.consume(&payload) {
            if is_terminal_event(&event) {
                events.retain(|event| !is_terminal_event(event));
            }
            events.push(event);
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use grey_core::{
        ChatMessage, ChatRequest, Provider, ProviderEvent, ProviderFailureKind, Role, ToolCall,
        ToolDefinition, ToolRisk,
    };
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn maps_ordered_history_and_flat_function_tools_without_temperature() {
        let provider = ResponsesProvider::new(
            "https://example.test/v1/responses/".into(),
            Some("sk-fixture".into()),
        )
        .unwrap();
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path":"Cargo.toml"}),
        };
        let request = ChatRequest {
            model: "gpt-test".into(),
            messages: vec![
                ChatMessage::new(Role::System, "system one"),
                ChatMessage::new(Role::User, "question"),
                ChatMessage::assistant("checking", vec![call.clone()]),
                ChatMessage::tool_result(&call, ""),
                ChatMessage::new(Role::System, "later system summary"),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "Read one file".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                risk: ToolRisk::ReadOnly,
            }],
            temperature: Some(0.7),
        };

        let http = provider.build_request(&request).unwrap();
        assert_eq!(http.url().as_str(), "https://example.test/v1/responses");
        assert_eq!(
            http.headers()[reqwest::header::AUTHORIZATION],
            "Bearer sk-fixture"
        );
        let body: Value = serde_json::from_slice(http.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert!(body.get("temperature").is_none());
        assert_eq!(
            body["input"],
            json!([
                {"type":"message","role":"system","content":"system one"},
                {"type":"message","role":"user","content":"question"},
                {"type":"message","role":"assistant","content":"checking"},
                {"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"},
                {"type":"function_call_output","call_id":"call-1","output":""},
                {"type":"message","role":"system","content":"later system summary"}
            ])
        );
        assert_eq!(
            body["tools"][0],
            json!({
                "type":"function",
                "name":"read_file",
                "description":"Read one file",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
            })
        );
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn rejects_missing_tool_history_identifiers_before_request() {
        let provider = ResponsesProvider::new("https://example.test/v1".into(), None).unwrap();
        let mut missing_result_id = ChatMessage::new(Role::Tool, "result");
        missing_result_id.tool_call_id = None;
        assert!(provider
            .build_request(&ChatRequest::new("m", vec![missing_result_id]))
            .unwrap_err()
            .to_string()
            .contains("tool_call_id"));

        for (id, name) in [("", "read_file"), ("call-1", "")] {
            let request = ChatRequest::new(
                "m",
                vec![ChatMessage::assistant(
                    "",
                    vec![ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments: json!({}),
                    }],
                )],
            );
            let error = provider.build_request(&request).unwrap_err();
            assert!(error.to_string().contains("function call"));
        }
    }

    #[test]
    fn correlates_early_function_name_and_emits_one_call_and_usage() {
        let payloads = vec![
            json!({"type":"response.created","response":{"id":"r1"}}).to_string(),
            json!({"type":"response.output_text.delta","delta":"hello"}).to_string(),
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "id":"fc_1","call_id":"call_1","type":"function_call","name":"read_file","arguments":""
            }}).to_string(),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}).to_string(),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"Cargo.toml\"}"}).to_string(),
            json!({"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"Cargo.toml\"}"}).to_string(),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":11,"output_tokens":7}}}).to_string(),
        ];
        let mut state = ResponsesStreamState::with_limit(4096);
        let events = consume_batch(&mut state, payloads);

        assert!(matches!(&events[0], ProviderEvent::Delta(text) if text == "hello"));
        assert!(matches!(&events[1], ProviderEvent::ToolCall(call)
            if call.id == "call_1"
                && call.name == "read_file"
                && call.arguments == json!({"path":"Cargo.toml"})));
        assert!(matches!(&events[2], ProviderEvent::Done(usage)
            if usage.input_tokens == 11 && usage.output_tokens == 7));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::ToolCall(_)))
                .count(),
            1
        );
    }

    #[test]
    fn classifies_failed_incomplete_and_top_level_errors() {
        for (payload, kind) in [
            (
                json!({"type":"response.failed","response":{"error":{"code":"invalid_api_key","message":"bad key"}}}),
                ProviderFailureKind::Auth,
            ),
            (
                json!({"type":"response.failed","response":{"error":{"code":"insufficient_permissions","message":"denied"}}}),
                ProviderFailureKind::Authorization,
            ),
            (
                json!({"type":"error","code":"rate_limit_exceeded","message":"slow down"}),
                ProviderFailureKind::RateLimit,
            ),
            (
                json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"retry"}}}),
                ProviderFailureKind::Server,
            ),
            (
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
                ProviderFailureKind::Protocol,
            ),
            (
                json!({"type":"error","code":"new_error","message":"unknown"}),
                ProviderFailureKind::Protocol,
            ),
        ] {
            let mut state = ResponsesStreamState::with_limit(4096);
            let events = state.consume(&payload.to_string());
            assert!(
                matches!(events.as_slice(), [ProviderEvent::Error(failure)] if failure.kind() == kind)
            );
        }
    }

    #[test]
    fn bounds_and_redacts_the_final_wire_error_diagnostic() {
        let secret = "secret-value-must-not-appear";
        let mut state = ResponsesStreamState::with_limit(64);
        let payload = json!({
            "type":"error",
            "code":"new_error",
            "message":format!("token={secret} {}", "甲".repeat(100))
        })
        .to_string();

        let events = state.consume(&payload);
        let [ProviderEvent::Error(failure)] = events.as_slice() else {
            panic!("expected one typed failure");
        };
        let diagnostic = failure.to_string();
        assert_eq!(failure.kind(), ProviderFailureKind::Protocol);
        assert!(diagnostic.len() <= 64, "{diagnostic:?}");
        assert!(!diagnostic.contains(secret));
        assert!(diagnostic.is_char_boundary(diagnostic.len()));
    }

    #[test]
    fn malformed_shapes_eof_and_duplicate_terminal_are_protocol_errors() {
        for payload in [
            "not-json".to_string(),
            json!({"type":"response.completed"}).to_string(),
            json!({"type":"response.failed","response":{}}).to_string(),
            json!({"type":"error","message":"missing code"}).to_string(),
            json!({"type":"response.function_call_arguments.delta","item_id":"missing","delta":"{}"}).to_string(),
        ] {
            let mut state = ResponsesStreamState::with_limit(4096);
            assert!(matches!(state.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
                if failure.kind() == ProviderFailureKind::Protocol));
        }

        let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}).to_string();
        let mut duplicate = ResponsesStreamState::with_limit(4096);
        assert!(matches!(
            duplicate.consume(&completed).as_slice(),
            [ProviderEvent::Done(_)]
        ));
        assert!(
            matches!(duplicate.consume(&completed).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(duplicate.consume(&completed).is_empty());

        let mut batch = ResponsesStreamState::with_limit(4096);
        let events = consume_batch(
            &mut batch,
            vec![
                completed,
                json!({"type":"response.output_text.delta","delta":"after"}).to_string(),
            ],
        );
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol));

        let mut eof = ResponsesStreamState::with_limit(4096);
        assert!(
            matches!(eof.finish().as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(eof.finish().is_empty());
    }

    #[test]
    fn rejects_wrongly_typed_optional_done_metadata() {
        let added = json!({"type":"response.output_item.added","item":{
            "id":"fc_1","call_id":"call_1","type":"function_call","name":"read_file","arguments":"{}"
        }})
        .to_string();
        for field in ["name", "call_id"] {
            let mut state = ResponsesStreamState::with_limit(4096);
            assert!(state.consume(&added).is_empty());
            let mut done = json!({
                "type":"response.function_call_arguments.done",
                "item_id":"fc_1",
                "arguments":"{}"
            });
            done[field] = json!(7);
            assert!(
                matches!(state.consume(&done.to_string()).as_slice(), [ProviderEvent::Error(failure)]
                if failure.kind() == ProviderFailureKind::Protocol)
            );
        }
    }

    #[test]
    fn enforces_event_call_metadata_argument_and_text_caps_before_retaining() {
        let mut decoder = crate::sse::SseDecoder::default();
        let oversized_event = format!("data: {}", "x".repeat(1024 * 1024 + 1));
        assert!(decoder.feed(oversized_event.as_bytes()).is_err());

        let mut calls = ResponsesStreamState::with_limit(1024 * 1024);
        for index in 0..crate::MAX_TOOL_CALLS {
            let payload = json!({"type":"response.output_item.added","item":{
                "id":format!("fc_{index}"),"call_id":format!("call_{index}"),
                "type":"function_call","name":"f","arguments":""
            }})
            .to_string();
            assert!(calls.consume(&payload).is_empty());
        }
        let too_many = json!({"type":"response.output_item.added","item":{
            "id":"overflow","call_id":"overflow","type":"function_call","name":"f","arguments":""
        }})
        .to_string();
        assert!(matches!(
            calls.consume(&too_many).as_slice(),
            [ProviderEvent::Error(_)]
        ));

        let mut metadata = ResponsesStreamState::with_limit(8);
        let oversized_metadata = json!({"type":"response.output_item.added","item":{
            "id":"fc1","call_id":"c1","type":"function_call","name":"tool","arguments":""
        }})
        .to_string();
        assert!(matches!(
            metadata.consume(&oversized_metadata).as_slice(),
            [ProviderEvent::Error(_)]
        ));
        assert!(metadata.pending_by_item_id.is_empty());
        assert_eq!(metadata.tool_bytes, 0);

        let mut arguments = ResponsesStreamState::with_limit(5);
        let added = json!({"type":"response.output_item.added","item":{
            "id":"i","call_id":"c","type":"function_call","name":"n","arguments":""
        }})
        .to_string();
        assert!(arguments.consume(&added).is_empty());
        let delta =
            json!({"type":"response.function_call_arguments.delta","item_id":"i","delta":"甲"})
                .to_string();
        assert!(matches!(
            arguments.consume(&delta).as_slice(),
            [ProviderEvent::Error(_)]
        ));

        let mut text = ResponsesStreamState::with_limit(5);
        let delta = json!({"type":"response.output_text.delta","delta":"甲乙"}).to_string();
        assert!(matches!(
            text.consume(&delta).as_slice(),
            [ProviderEvent::Error(_)]
        ));
    }

    async fn serve_fragmented(
        status: &'static str,
        body: Vec<u8>,
        declared_length: Option<usize>,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                declared_length.unwrap_or(body.len())
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            for byte in body {
                socket.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            socket.shutdown().await.unwrap();
            request
        });
        (format!("http://{address}/v1/"), task)
    }

    #[tokio::test]
    async fn posts_exact_responses_path_and_auth_over_fragmented_sse() {
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"ok"}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}})
        ).into_bytes();
        let (base_url, server) = serve_fragmented("200 OK", body, None).await;
        let provider = ResponsesProvider::new(base_url, Some("sk-fixture".into())).unwrap();
        let request = ChatRequest::new("gpt-test", vec![ChatMessage::new(Role::User, "hello")]);
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let raw_request = server.await.unwrap();

        assert!(
            matches!(events.as_slice(), [ProviderEvent::Delta(text), ProviderEvent::Done(usage)]
            if text == "ok" && usage.input_tokens == 3 && usage.output_tokens == 2)
        );
        let request_text = String::from_utf8(raw_request).unwrap();
        assert!(request_text.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        assert!(request_text
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-fixture\r\n"));
        let body: Value =
            serde_json::from_str(request_text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["input"][0]["content"], "hello");
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn classifies_http_transport_non_success_and_eof_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let provider = ResponsesProvider::new(format!("http://{address}"), None).unwrap();
        let request = ChatRequest::new("m", vec![]);
        let transport = match provider.stream_chat(&request).await {
            Ok(_) => panic!("closed port unexpectedly produced a stream"),
            Err(error) => error,
        };
        assert_eq!(
            transport
                .downcast_ref::<grey_core::ProviderFailure>()
                .unwrap()
                .kind(),
            ProviderFailureKind::Transport
        );

        let (base_url, server) =
            serve_fragmented("401 Unauthorized", b"bad key".to_vec(), None).await;
        let provider = ResponsesProvider::new(base_url, None).unwrap();
        let auth = match provider.stream_chat(&request).await {
            Ok(_) => panic!("HTTP 401 unexpectedly produced a stream"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(
            auth.downcast_ref::<grey_core::ProviderFailure>()
                .unwrap()
                .kind(),
            ProviderFailureKind::Auth
        );

        let partial = format!(
            "data: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"partial"})
        )
        .into_bytes();
        let (base_url, server) = serve_fragmented("200 OK", partial, None).await;
        let provider = ResponsesProvider::new(base_url, None).unwrap();
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(
            matches!(events.as_slice(), [ProviderEvent::Delta(_), ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );

        let truncated = b"data: {\"type\":\"response.created\"}\n\n".to_vec();
        let declared = truncated.len() + 10;
        let (base_url, server) = serve_fragmented("200 OK", truncated, Some(declared)).await;
        let provider = ResponsesProvider::new(base_url, None).unwrap();
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Transport));
    }
}
