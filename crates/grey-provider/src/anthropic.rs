//! Anthropic Messages API streaming adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use grey_core::{
    checked_utf8_bytes, ChatMessage, ChatRequest, GreyConfig, Provider, ProviderEvent,
    ProviderFailure, ProviderFailureKind, Role, RuntimeConfig, ToolCall, Usage,
};
use serde_json::{json, Value};

use crate::sse::SseDecoder;

pub struct AnthropicProvider {
    base_url: String,
    api_key: Option<String>,
    version: String,
    max_tokens: u32,
    response_max_bytes: usize,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn from_config(cfg: &GreyConfig) -> Result<Self> {
        Self::new(
            cfg.anthropic.base_url.clone(),
            (!cfg.anthropic.api_key.is_empty()).then(|| cfg.anthropic.api_key.clone()),
            cfg.anthropic.version.clone(),
            cfg.anthropic.max_tokens,
        )
        .map(|provider| provider.with_response_max_bytes(cfg.runtime.response_max_bytes))
    }

    pub fn new(
        base_url: String,
        api_key: Option<String>,
        version: String,
        max_tokens: u32,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("building Anthropic HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            version,
            max_tokens,
            response_max_bytes: RuntimeConfig::default().response_max_bytes,
            client,
        })
    }

    pub fn with_response_max_bytes(mut self, response_max_bytes: usize) -> Self {
        self.response_max_bytes = response_max_bytes;
        self
    }

    fn build_request(&self, request: &ChatRequest) -> Result<reqwest::Request> {
        let mut builder = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", &self.version)
            .json(&request_body(request, self.max_tokens)?);
        if let Some(key) = &self.api_key {
            builder = builder.header("x-api-key", key);
        }
        builder.build().context("building Anthropic request")
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<futures_util::stream::BoxStream<'a, ProviderEvent>> {
        let http_request = self.build_request(request)?;
        let response = crate::send_http(&self.client, http_request, "Anthropic provider").await?;

        let mut chunks = response.bytes_stream();
        let output = async_stream::stream! {
            let mut decoder = SseDecoder::default();
            let mut protocol = AnthropicStreamState::with_limit(self.response_max_bytes);

            while let Some(chunk) = chunks.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(event) = protocol.fail_failure(ProviderFailure::with_source(
                            ProviderFailureKind::Transport,
                            "Anthropic stream transport failed",
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
                        if let Some(event) = protocol.fail_failure(ProviderFailure::with_source(
                            ProviderFailureKind::Protocol,
                            "Anthropic SSE framing is malformed",
                            error,
                        )) {
                            yield event;
                        }
                        return;
                    }
                };
                for payload in payloads {
                    for event in protocol.consume(&payload) {
                        let terminal = matches!(event, ProviderEvent::Done(_) | ProviderEvent::Error(_));
                        yield event;
                        if terminal {
                            return;
                        }
                    }
                }
            }

            if let Err(error) = decoder.finish() {
                if let Some(event) = protocol.fail_failure(ProviderFailure::with_source(
                    ProviderFailureKind::Protocol,
                    "Anthropic SSE stream ended with an incomplete event",
                    error,
                )) {
                    yield event;
                }
                return;
            }
            if let Some(event) = protocol.fail("stream ended before Anthropic message_stop") {
                yield event;
            }
        };
        Ok(Box::pin(output))
    }
}

fn request_body(request: &ChatRequest, max_tokens: u32) -> Result<Value> {
    if max_tokens == 0 {
        bail!("Anthropic max_tokens must be greater than zero");
    }
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    let mut saw_conversation_message = false;
    for message in &request.messages {
        if message.role == Role::System {
            if saw_conversation_message {
                bail!("Anthropic system messages must precede conversation messages");
            }
            system_parts.push(message.content.as_str());
        } else {
            saw_conversation_message = true;
            messages.push(anthropic_message(message)?);
        }
    }
    let system = system_parts.join("\n\n");
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = Value::String(system);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    Ok(body)
}

fn anthropic_message(message: &ChatMessage) -> Result<Value> {
    match message.role {
        Role::System => bail!("Anthropic system messages belong in the top-level system field"),
        Role::User => Ok(json!({"role": "user", "content": message.content})),
        Role::Assistant => {
            if message.tool_calls.is_empty() {
                return Ok(json!({"role": "assistant", "content": message.content}));
            }
            let mut content = Vec::new();
            if !message.content.is_empty() {
                content.push(json!({"type": "text", "text": message.content}));
            }
            content.extend(message.tool_calls.iter().map(|call| {
                json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                })
            }));
            Ok(json!({"role": "assistant", "content": content}))
        }
        Role::Tool => {
            let call_id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .context("Anthropic tool message is missing tool_call_id")?;
            Ok(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": message.content,
                }]
            }))
        }
    }
}

#[derive(Debug)]
struct AnthropicToolParts {
    id: String,
    name: String,
    initial_input: Value,
    input_json: String,
}

#[derive(Debug)]
struct AnthropicStreamState {
    calls: BTreeMap<usize, AnthropicToolParts>,
    active_blocks: BTreeSet<usize>,
    seen_blocks: BTreeSet<usize>,
    tool_data_bytes: usize,
    text_bytes: usize,
    response_max_bytes: usize,
    usage: Usage,
    started: bool,
    terminal: bool,
}

impl AnthropicStreamState {
    fn with_limit(response_max_bytes: usize) -> Self {
        Self {
            calls: BTreeMap::new(),
            active_blocks: BTreeSet::new(),
            seen_blocks: BTreeSet::new(),
            tool_data_bytes: 0,
            text_bytes: 0,
            response_max_bytes,
            usage: Usage::default(),
            started: false,
            terminal: false,
        }
    }

    fn consume(&mut self, payload: &str) -> Vec<ProviderEvent> {
        if self.terminal {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return self
                    .fail(format!("malformed Anthropic SSE payload: {error}"))
                    .into_iter()
                    .collect()
            }
        };
        let event_type = match value.get("type").and_then(Value::as_str) {
            Some(event_type) => event_type,
            None => {
                return self
                    .fail("Anthropic SSE payload is missing type")
                    .into_iter()
                    .collect()
            }
        };
        if !self.started && !matches!(event_type, "message_start" | "ping" | "error") {
            return self
                .fail(format!(
                    "Anthropic {event_type} arrived before message_start"
                ))
                .into_iter()
                .collect();
        }
        match event_type {
            "message_start" => {
                if self.started {
                    return self
                        .fail("Anthropic stream contains duplicate message_start")
                        .into_iter()
                        .collect();
                }
                let Some(input) = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                else {
                    return self
                        .fail("Anthropic message_start is missing usage.input_tokens")
                        .into_iter()
                        .collect();
                };
                let Some(output) = value
                    .pointer("/message/usage/output_tokens")
                    .and_then(Value::as_u64)
                else {
                    return self
                        .fail("Anthropic message_start is missing usage.output_tokens")
                        .into_iter()
                        .collect();
                };
                self.usage.input_tokens = input;
                self.usage.output_tokens = output;
                self.started = true;
                Vec::new()
            }
            "content_block_start" => self.start_content_block(&value),
            "content_block_delta" => self.apply_content_delta(&value),
            "content_block_stop" => self.stop_content_block(&value),
            "message_delta" => {
                if !self.active_blocks.is_empty() {
                    return self
                        .fail("Anthropic message_delta arrived before content_block_stop")
                        .into_iter()
                        .collect();
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = output;
                }
                Vec::new()
            }
            "message_stop" => self.complete(),
            "error" => self
                .fail(format!(
                    "Anthropic stream error: {}",
                    value.get("error").unwrap_or(&value)
                ))
                .into_iter()
                .collect(),
            "ping" => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn start_content_block(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return self
                .fail("Anthropic content_block_start is missing index")
                .into_iter()
                .collect();
        };
        let Ok(index) = usize::try_from(index) else {
            return self
                .fail("Anthropic content block index does not fit usize")
                .into_iter()
                .collect();
        };
        if self.seen_blocks.contains(&index) {
            return self
                .fail(format!(
                    "Anthropic content block index {index} started more than once"
                ))
                .into_iter()
                .collect();
        }
        let Some(block) = value.get("content_block") else {
            return self
                .fail("Anthropic content_block_start is missing content_block")
                .into_iter()
                .collect();
        };
        if block.get("type").and_then(Value::as_str) == Some("tool_use")
            && self.calls.len() >= crate::MAX_TOOL_CALLS
        {
            return self
                .fail(format!(
                    "Anthropic stream exceeds {} tool calls",
                    crate::MAX_TOOL_CALLS
                ))
                .into_iter()
                .collect();
        }
        if self.seen_blocks.len() >= crate::MAX_TOOL_CALLS {
            return self
                .fail(format!(
                    "Anthropic stream exceeds {} content blocks",
                    crate::MAX_TOOL_CALLS
                ))
                .into_iter()
                .collect();
        }
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    return self
                        .fail("Anthropic text block is missing text")
                        .into_iter()
                        .collect();
                };
                if text.is_empty() {
                    self.seen_blocks.insert(index);
                    self.active_blocks.insert(index);
                    Vec::new()
                } else {
                    let Some(next) =
                        checked_utf8_bytes(self.text_bytes, text, self.response_max_bytes)
                    else {
                        return self
                            .fail("Anthropic response text exceeds configured byte limit")
                            .into_iter()
                            .collect();
                    };
                    self.seen_blocks.insert(index);
                    self.active_blocks.insert(index);
                    self.text_bytes = next;
                    vec![ProviderEvent::Delta(text.to_string())]
                }
            }
            Some("tool_use") => {
                let Some(id) = block.get("id").and_then(Value::as_str) else {
                    return self
                        .fail("Anthropic tool_use block is missing id")
                        .into_iter()
                        .collect();
                };
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    return self
                        .fail("Anthropic tool_use block is missing name")
                        .into_iter()
                        .collect();
                };
                let initial_input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let initial_json = initial_input.to_string();
                let mut next = self.tool_data_bytes;
                for value in [id, name, &initial_json] {
                    let Some(checked) = checked_utf8_bytes(next, value, self.response_max_bytes)
                    else {
                        return self
                            .fail("Anthropic tool call exceeds configured byte limit")
                            .into_iter()
                            .collect();
                    };
                    next = checked;
                }
                self.seen_blocks.insert(index);
                self.tool_data_bytes = next;
                self.calls.insert(
                    index,
                    AnthropicToolParts {
                        id: id.to_string(),
                        name: name.to_string(),
                        initial_input,
                        input_json: String::new(),
                    },
                );
                self.active_blocks.insert(index);
                Vec::new()
            }
            Some(_) => {
                self.seen_blocks.insert(index);
                self.active_blocks.insert(index);
                Vec::new()
            }
            None => self
                .fail("Anthropic content block is missing type")
                .into_iter()
                .collect(),
        }
    }

    fn apply_content_delta(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return self
                .fail("Anthropic content_block_delta is missing index")
                .into_iter()
                .collect();
        };
        let Ok(index) = usize::try_from(index) else {
            return self
                .fail("Anthropic content block index does not fit usize")
                .into_iter()
                .collect();
        };
        if !self.active_blocks.contains(&index) {
            return self
                .fail(format!(
                    "Anthropic content_block_delta references inactive index {index}"
                ))
                .into_iter()
                .collect();
        }
        let Some(delta) = value.get("delta") else {
            return self
                .fail("Anthropic content_block_delta is missing delta")
                .into_iter()
                .collect();
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => match delta.get("text").and_then(Value::as_str) {
                Some(text) if !text.is_empty() => {
                    let Some(next) =
                        checked_utf8_bytes(self.text_bytes, text, self.response_max_bytes)
                    else {
                        return self
                            .fail("Anthropic response text exceeds configured byte limit")
                            .into_iter()
                            .collect();
                    };
                    self.text_bytes = next;
                    vec![ProviderEvent::Delta(text.to_string())]
                }
                Some(_) => Vec::new(),
                None => self
                    .fail("Anthropic text_delta is missing text")
                    .into_iter()
                    .collect(),
            },
            Some("input_json_delta") => {
                let Some(partial) = delta.get("partial_json").and_then(Value::as_str) else {
                    return self
                        .fail("Anthropic input_json_delta is missing partial_json")
                        .into_iter()
                        .collect();
                };
                let Some(next) =
                    checked_utf8_bytes(self.tool_data_bytes, partial, self.response_max_bytes)
                else {
                    return self
                        .fail("Anthropic tool arguments exceed configured byte limit")
                        .into_iter()
                        .collect();
                };
                let Some(call) = self.calls.get_mut(&index) else {
                    return self
                        .fail(format!(
                            "Anthropic input_json_delta references unknown index {index}"
                        ))
                        .into_iter()
                        .collect();
                };
                call.input_json.push_str(partial);
                self.tool_data_bytes = next;
                Vec::new()
            }
            Some(_) => Vec::new(),
            None => self
                .fail("Anthropic content delta is missing type")
                .into_iter()
                .collect(),
        }
    }

    fn stop_content_block(&mut self, value: &Value) -> Vec<ProviderEvent> {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return self
                .fail("Anthropic content_block_stop is missing index")
                .into_iter()
                .collect();
        };
        let Ok(index) = usize::try_from(index) else {
            return self
                .fail("Anthropic content block index does not fit usize")
                .into_iter()
                .collect();
        };
        if !self.active_blocks.remove(&index) {
            return self
                .fail(format!(
                    "Anthropic content_block_stop references inactive index {index}"
                ))
                .into_iter()
                .collect();
        }
        Vec::new()
    }

    fn complete(&mut self) -> Vec<ProviderEvent> {
        if !self.active_blocks.is_empty() {
            return self
                .fail("Anthropic message_stop arrived before content_block_stop")
                .into_iter()
                .collect();
        }
        let mut events = Vec::new();
        for (index, parts) in std::mem::take(&mut self.calls) {
            let arguments = if parts.input_json.is_empty() {
                parts.initial_input
            } else {
                match serde_json::from_str(&parts.input_json) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        return self
                            .fail(format!(
                                "malformed Anthropic tool arguments at index {index}: {error}"
                            ))
                            .into_iter()
                            .collect()
                    }
                }
            };
            events.push(ProviderEvent::ToolCall(ToolCall {
                id: parts.id,
                name: parts.name,
                arguments,
            }));
        }
        self.terminal = true;
        events.push(ProviderEvent::Done(self.usage.clone()));
        events
    }

    fn fail(&mut self, message: impl Into<String>) -> Option<ProviderEvent> {
        self.fail_failure(ProviderFailure::new(ProviderFailureKind::Protocol, message))
    }

    fn fail_failure(&mut self, failure: ProviderFailure) -> Option<ProviderEvent> {
        if self.terminal {
            None
        } else {
            self.terminal = true;
            Some(ProviderEvent::Error(failure))
        }
    }
}

impl Default for AnthropicStreamState {
    fn default() -> Self {
        Self::with_limit(RuntimeConfig::default().response_max_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use grey_core::{ToolDefinition, ToolRisk};

    #[test]
    fn decodes_text_tool_input_and_usage_from_fragmented_sse() {
        let payloads = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":13,"output_tokens":0}}}).to_string(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string(),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}).to_string(),
            json!({"type":"content_block_stop","index":0}).to_string(),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool-1","name":"grep","input":{}}}).to_string(),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\""}}).to_string(),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"TODO\"}"}}).to_string(),
            json!({"type":"content_block_stop","index":1}).to_string(),
            json!({"type":"message_delta","usage":{"output_tokens":8}}).to_string(),
            json!({"type":"message_stop"}).to_string(),
        ];
        let wire = payloads
            .iter()
            .map(|payload| format!("event: ignored\r\ndata: {payload}\r\n\r\n"))
            .collect::<String>();
        let mut decoder = SseDecoder::default();
        let mut state = AnthropicStreamState::default();
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            for payload in decoder.feed(std::slice::from_ref(byte)).unwrap() {
                events.extend(state.consume(&payload));
            }
        }
        decoder.finish().unwrap();

        assert!(matches!(&events[0], ProviderEvent::Delta(text) if text == "hello"));
        assert!(matches!(&events[1], ProviderEvent::ToolCall(call)
            if call.id == "tool-1"
                && call.name == "grep"
                && call.arguments == json!({"pattern":"TODO"})));
        assert!(matches!(&events[2], ProviderEvent::Done(usage)
            if usage.input_tokens == 13 && usage.output_tokens == 8));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::Done(_)))
                .count(),
            1
        );
        assert!(state
            .consume(&json!({"type":"message_stop"}).to_string())
            .is_empty());
    }

    #[test]
    fn malformed_payload_and_vendor_error_terminate_once() {
        let mut malformed = AnthropicStreamState::default();
        assert!(matches!(
            malformed.consume("{").as_slice(),
            [ProviderEvent::Error(message)] if message.to_string().contains("malformed")
        ));
        assert!(malformed.fail("again").is_none());

        let mut vendor = AnthropicStreamState::default();
        let payload = json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})
            .to_string();
        assert!(matches!(
            vendor.consume(&payload).as_slice(),
            [ProviderEvent::Error(message)] if message.to_string().contains("busy")
        ));
    }

    #[test]
    fn malformed_stream_failure_is_protocol() {
        let mut state = AnthropicStreamState::default();
        let events = state.consume("{");
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == grey_core::ProviderFailureKind::Protocol));
    }

    #[test]
    fn rejects_out_of_order_and_incomplete_lifecycle_events() {
        let mut stop_only = AnthropicStreamState::default();
        let stop = json!({"type":"message_stop"}).to_string();
        assert!(
            matches!(stop_only.consume(&stop).as_slice(), [ProviderEvent::Error(message)]
            if message.to_string().contains("before message_start"))
        );

        let mut missing_usage = AnthropicStreamState::default();
        let start = json!({"type":"message_start","message":{}}).to_string();
        assert!(
            matches!(missing_usage.consume(&start).as_slice(), [ProviderEvent::Error(message)]
            if message.to_string().contains("input_tokens"))
        );

        let mut active_block = AnthropicStreamState::default();
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}).to_string();
        let block = json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string();
        assert!(active_block.consume(&start).is_empty());
        assert!(active_block.consume(&block).is_empty());
        assert!(
            matches!(active_block.consume(&stop).as_slice(), [ProviderEvent::Error(message)]
            if message.to_string().contains("content_block_stop"))
        );
    }

    #[test]
    fn rejects_unbounded_tool_call_accumulation() {
        let mut state = AnthropicStreamState::default();
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}).to_string();
        assert!(state.consume(&start).is_empty());
        for index in 0..crate::MAX_TOOL_CALLS {
            let block = json!({"type":"content_block_start","index":index,"content_block":{
                "type":"tool_use","id":format!("tool-{index}"),"name":"grep","input":{}
            }})
            .to_string();
            assert!(state.consume(&block).is_empty());
            let stop = json!({"type":"content_block_stop","index":index}).to_string();
            assert!(state.consume(&stop).is_empty());
        }
        let overflow =
            json!({"type":"content_block_start","index":crate::MAX_TOOL_CALLS,"content_block":{
                "type":"tool_use","id":"overflow","name":"grep","input":{}
            }})
            .to_string();
        assert!(
            matches!(state.consume(&overflow).as_slice(), [ProviderEvent::Error(message)]
            if message.to_string().contains("tool calls"))
        );
    }

    #[test]
    fn limit_rejects_anthropic_text_and_tool_arguments_once() {
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}).to_string();
        let mut text = AnthropicStreamState::with_limit(3);
        assert!(text.consume(&start).is_empty());
        let payload = json!({"type":"content_block_start","index":0,"content_block":{
            "type":"text","text":"甲乙"
        }})
        .to_string();
        assert!(
            matches!(text.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(text
            .consume(&json!({"type":"message_stop"}).to_string())
            .is_empty());

        let mut tool = AnthropicStreamState::with_limit(3);
        assert!(tool.consume(&start).is_empty());
        let payload = json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"tool-1","name":"grep","input":{"x":"long"}
        }})
        .to_string();
        assert!(
            matches!(tool.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(tool
            .consume(&json!({"type":"message_stop"}).to_string())
            .is_empty());
    }

    #[test]
    fn limit_rejects_anthropic_tool_metadata_before_retaining_it() {
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}).to_string();
        let mut state = AnthropicStreamState::with_limit(2);
        assert!(state.consume(&start).is_empty());
        let payload = json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"oversized-id","name":"x","input":{}
        }})
        .to_string();

        assert!(
            matches!(state.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(state.calls.is_empty());
        assert!(state.seen_blocks.is_empty());
        assert!(state.active_blocks.is_empty());
        assert_eq!(state.tool_data_bytes, 0);
        assert!(state
            .consume(&json!({"type":"message_stop"}).to_string())
            .is_empty());
    }

    #[test]
    fn limit_rejects_unbounded_empty_and_unknown_anthropic_blocks_once() {
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}).to_string();
        for block in [json!({"type":"text","text":""}), json!({"type":"unknown"})] {
            let mut state = AnthropicStreamState::default();
            assert!(state.consume(&start).is_empty());
            for index in 0..crate::MAX_TOOL_CALLS {
                let start_block = json!({
                    "type":"content_block_start","index":index,"content_block":block
                })
                .to_string();
                assert!(state.consume(&start_block).is_empty());
                let stop_block = json!({"type":"content_block_stop","index":index}).to_string();
                assert!(state.consume(&stop_block).is_empty());
            }
            let overflow = json!({
                "type":"content_block_start",
                "index":crate::MAX_TOOL_CALLS,
                "content_block":block
            })
            .to_string();
            assert!(
                matches!(state.consume(&overflow).as_slice(), [ProviderEvent::Error(failure)]
                if failure.kind() == ProviderFailureKind::Protocol)
            );
            assert!(state.consume(&overflow).is_empty());
        }
    }

    #[test]
    fn serializes_messages_tools_and_anthropic_headers() {
        let provider = AnthropicProvider::new(
            "https://api.example.test/v1/".into(),
            Some("anthropic-secret".into()),
            "2023-06-01".into(),
            2048,
        )
        .unwrap();
        let call = ToolCall {
            id: "tool-1".into(),
            name: "grep".into(),
            arguments: json!({"pattern":"TODO"}),
        };
        let request = ChatRequest {
            model: "claude-test".into(),
            messages: vec![
                ChatMessage::new(Role::System, "Be precise."),
                ChatMessage::new(Role::User, "inspect"),
                ChatMessage::assistant("I will inspect.", vec![call.clone()]),
                ChatMessage::tool_result(&call, "one match"),
            ],
            tools: vec![ToolDefinition {
                name: "grep".into(),
                description: "Search text".into(),
                input_schema: json!({"type":"object"}),
                risk: ToolRisk::ReadOnly,
            }],
            temperature: Some(0.25),
        };
        let http = provider.build_request(&request).unwrap();
        assert_eq!(http.url().as_str(), "https://api.example.test/v1/messages");
        assert_eq!(http.headers()["x-api-key"], "anthropic-secret");
        assert_eq!(http.headers()["anthropic-version"], "2023-06-01");
        let body: Value = serde_json::from_slice(http.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["system"], "Be precise.");
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));

        let reordered = ChatRequest::new(
            "claude-test",
            vec![
                ChatMessage::new(Role::User, "hello"),
                ChatMessage::new(Role::System, "too late"),
            ],
        );
        assert!(request_body(&reordered, 128)
            .unwrap_err()
            .to_string()
            .contains("must precede"));
    }

    #[tokio::test]
    async fn http_stream_emits_one_done_and_reports_missing_message_stop() {
        let complete_body = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"message_delta","usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ]
        .into_iter()
        .map(|payload| format!("data: {payload}\n\n"))
        .collect::<String>()
        .into_bytes();
        let (base_url, server) = crate::test_support::serve_one_sse(complete_body, None).await;
        let provider = AnthropicProvider::new(base_url, None, "2023-06-01".into(), 128).unwrap();
        let request = ChatRequest::new("test", vec![ChatMessage::new(Role::User, "hello")]);
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(matches!(events.as_slice(), [ProviderEvent::Done(usage)]
            if usage.input_tokens == 5 && usage.output_tokens == 4));

        let incomplete_body = format!(
            "data: {}\n\n",
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}})
        )
        .into_bytes();
        let (base_url, server) = crate::test_support::serve_one_sse(incomplete_body, None).await;
        let provider = AnthropicProvider::new(base_url, None, "2023-06-01".into(), 128).unwrap();
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(message)]
            if message.to_string().contains("message_stop")));
    }

    #[tokio::test]
    async fn truncated_and_non_success_http_responses_propagate_bounded_errors() {
        let body = format!(
            "data: {}\n\n",
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}})
        )
        .into_bytes();
        let declared_length = body.len() + 20;
        let (base_url, server) =
            crate::test_support::serve_one_sse(body, Some(declared_length)).await;
        let provider = AnthropicProvider::new(base_url, None, "2023-06-01".into(), 128).unwrap();
        let request = ChatRequest::new("test", vec![ChatMessage::new(Role::User, "hello")]);
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(matches!(events.last(), Some(ProviderEvent::Error(message))
            if message.kind() == ProviderFailureKind::Transport));
        assert!(!events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Done(_))));

        let body = vec![b'x'; crate::MAX_ERROR_BODY_BYTES * 2];
        let (base_url, server) =
            crate::test_support::serve_one_response(body, None, "429 Too Many Requests").await;
        let provider = AnthropicProvider::new(base_url, None, "2023-06-01".into(), 128).unwrap();
        let error = match provider.stream_chat(&request).await {
            Ok(_) => panic!("non-success response unexpectedly produced a stream"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert!(error.to_string().contains("429 Too Many Requests"));
        assert!(error.to_string().contains("truncated"));
        assert!(error.to_string().len() < crate::MAX_ERROR_BODY_BYTES + 200);
    }
}
