//! OpenAI-compatible Chat Completions streaming adapter.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use grey_core::{
    checked_utf8_bytes, ChatMessage, ChatRequest, GreyConfig, Provider, ProviderEvent,
    ProviderFailure, ProviderFailureKind, Role, RuntimeConfig, ToolCall, Usage,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sse::SseDecoder;

pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: Option<String>,
    include_usage: bool,
    response_max_bytes: usize,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    pub fn from_config(cfg: &GreyConfig) -> Result<Self> {
        Self::new_with_usage(
            cfg.openai.base_url.clone(),
            (!cfg.openai.api_key.is_empty()).then(|| cfg.openai.api_key.clone()),
            cfg.openai.include_usage,
        )
        .map(|provider| provider.with_response_max_bytes(cfg.runtime.response_max_bytes))
    }

    pub fn new(base_url: String, api_key: Option<String>) -> Result<Self> {
        Self::new_with_usage(base_url, api_key, true)
    }

    pub fn new_with_usage(
        base_url: String,
        api_key: Option<String>,
        include_usage: bool,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("building OpenAI HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            include_usage,
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
            .post(format!("{}/chat/completions", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request_body(request, self.include_usage)?);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        builder.build().context("building OpenAI request")
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<futures_util::stream::BoxStream<'a, ProviderEvent>> {
        let http_request = self.build_request(request)?;
        let response = crate::send_http(&self.client, http_request, "OpenAI provider").await?;

        let mut chunks = response.bytes_stream();
        let output = async_stream::stream! {
            let mut decoder = SseDecoder::default();
            let mut protocol = OpenAiStreamState::with_limit(self.response_max_bytes);

            while let Some(chunk) = chunks.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(event) = protocol.fail(ProviderFailure::with_source(
                            ProviderFailureKind::Transport,
                            "OpenAI stream transport failed",
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
                        if let Some(event) = protocol.fail(ProviderFailure::with_source(
                            ProviderFailureKind::Protocol,
                            "OpenAI SSE framing is malformed",
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
                if let Some(event) = protocol.fail(ProviderFailure::with_source(
                    ProviderFailureKind::Protocol,
                    "OpenAI SSE stream ended with an incomplete event",
                    error,
                )) {
                    yield event;
                }
                return;
            }
            if let Some(event) = protocol.fail(ProviderFailure::new(
                ProviderFailureKind::Protocol,
                "stream ended before OpenAI [DONE] marker",
            )) {
                yield event;
            }
        };
        Ok(Box::pin(output))
    }
}

fn request_body(request: &ChatRequest, include_usage: bool) -> Result<Value> {
    let messages = request
        .messages
        .iter()
        .map(openai_message)
        .collect::<Result<Vec<_>>>()?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
    });
    if include_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    Ok(body)
}

fn openai_message(message: &ChatMessage) -> Result<Value> {
    match message.role {
        Role::System => Ok(json!({"role": "system", "content": message.content})),
        Role::User => Ok(json!({"role": "user", "content": message.content})),
        Role::Assistant => {
            let tool_calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        }
                    })
                })
                .collect::<Vec<_>>();
            let mut value = json!({"role": "assistant", "content": message.content});
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(tool_calls);
            }
            Ok(value)
        }
        Role::Tool => {
            let call_id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .context("OpenAI tool message is missing tool_call_id")?;
            Ok(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": message.content,
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<UsageShape>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: StreamFunction,
}

#[derive(Debug, Default, Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageShape {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Debug, Default)]
struct ToolCallParts {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct OpenAiStreamState {
    calls: BTreeMap<usize, ToolCallParts>,
    tool_data_bytes: usize,
    text_bytes: usize,
    response_max_bytes: usize,
    usage: Usage,
    terminal: bool,
}

impl OpenAiStreamState {
    fn with_limit(response_max_bytes: usize) -> Self {
        Self {
            calls: BTreeMap::new(),
            tool_data_bytes: 0,
            text_bytes: 0,
            response_max_bytes,
            usage: Usage::default(),
            terminal: false,
        }
    }

    fn consume(&mut self, payload: &str) -> Vec<ProviderEvent> {
        if self.terminal {
            return Vec::new();
        }
        if payload == "[DONE]" {
            return self.complete();
        }

        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return self
                    .fail(ProviderFailure::with_source(
                        ProviderFailureKind::Protocol,
                        "malformed OpenAI SSE payload",
                        error,
                    ))
                    .into_iter()
                    .collect()
            }
        };
        if let Some(error) = value.get("error") {
            return self
                .fail(ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    format!("OpenAI stream error: {error}"),
                ))
                .into_iter()
                .collect();
        }

        let chunk: StreamChunk = match serde_json::from_value(value) {
            Ok(chunk) => chunk,
            Err(error) => {
                return self
                    .fail(ProviderFailure::with_source(
                        ProviderFailureKind::Protocol,
                        "unexpected OpenAI stream payload",
                        error,
                    ))
                    .into_iter()
                    .collect()
            }
        };
        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    let Some(next) =
                        checked_utf8_bytes(self.text_bytes, &content, self.response_max_bytes)
                    else {
                        return self
                            .fail(ProviderFailure::new(
                                ProviderFailureKind::Protocol,
                                "OpenAI response text exceeds configured byte limit",
                            ))
                            .into_iter()
                            .collect();
                    };
                    self.text_bytes = next;
                    events.push(ProviderEvent::Delta(content));
                }
            }
            for delta in choice.delta.tool_calls {
                if !self.calls.contains_key(&delta.index)
                    && self.calls.len() >= crate::MAX_TOOL_CALLS
                {
                    return self
                        .fail(ProviderFailure::new(
                            ProviderFailureKind::Protocol,
                            format!("OpenAI stream exceeds {} tool calls", crate::MAX_TOOL_CALLS),
                        ))
                        .into_iter()
                        .collect();
                }
                let mut next = self.tool_data_bytes;
                for value in [
                    delta.id.as_deref(),
                    delta.function.name.as_deref(),
                    delta.function.arguments.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    let Some(checked) = checked_utf8_bytes(next, value, self.response_max_bytes)
                    else {
                        return self
                            .fail(ProviderFailure::new(
                                ProviderFailureKind::Protocol,
                                "OpenAI tool call exceeds configured byte limit",
                            ))
                            .into_iter()
                            .collect();
                    };
                    next = checked;
                }
                let call = self.calls.entry(delta.index).or_default();
                if let Some(id) = delta.id {
                    call.id.push_str(&id);
                }
                if let Some(name) = delta.function.name {
                    call.name.push_str(&name);
                }
                if let Some(arguments) = delta.function.arguments {
                    call.arguments.push_str(&arguments);
                }
                self.tool_data_bytes = next;
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            };
        }
        events
    }

    fn complete(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        for (index, parts) in std::mem::take(&mut self.calls) {
            if parts.id.is_empty() || parts.name.is_empty() {
                return self
                    .fail(ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        format!("incomplete OpenAI tool call at index {index}"),
                    ))
                    .into_iter()
                    .collect();
            }
            let arguments = match serde_json::from_str(&parts.arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return self
                        .fail(ProviderFailure::with_source(
                            ProviderFailureKind::Protocol,
                            format!("malformed OpenAI tool arguments at index {index}"),
                            error,
                        ))
                        .into_iter()
                        .collect()
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

    fn fail(&mut self, failure: ProviderFailure) -> Option<ProviderEvent> {
        if self.terminal {
            None
        } else {
            self.terminal = true;
            Some(ProviderEvent::Error(failure))
        }
    }
}

impl Default for OpenAiStreamState {
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
    fn assembles_fragmented_text_tool_call_and_usage_once() {
        let payloads = [
            json!({"choices":[{"delta":{"content":"hel"}}]}).to_string(),
            json!({"choices":[{"delta":{"content":"lo","tool_calls":[{"index":0,"id":"call_","function":{"name":"read_","arguments":"{\"path\":\""}}]}}]}).to_string(),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"1","function":{"name":"file","arguments":"src/lib.rs\"}"}}]}}]}).to_string(),
            json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}).to_string(),
            "[DONE]".to_string(),
        ];
        let wire = payloads
            .iter()
            .map(|payload| format!("data: {payload}\r\n\r\n"))
            .collect::<String>();
        let mut decoder = SseDecoder::default();
        let mut state = OpenAiStreamState::default();
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            for payload in decoder.feed(std::slice::from_ref(byte)).unwrap() {
                events.extend(state.consume(&payload));
            }
        }
        decoder.finish().unwrap();

        assert!(matches!(&events[0], ProviderEvent::Delta(text) if text == "hel"));
        assert!(matches!(&events[1], ProviderEvent::Delta(text) if text == "lo"));
        assert!(matches!(&events[2], ProviderEvent::ToolCall(call)
            if call.id == "call_1"
                && call.name == "read_file"
                && call.arguments == json!({"path":"src/lib.rs"})));
        assert!(matches!(&events[3], ProviderEvent::Done(usage)
            if usage.input_tokens == 11 && usage.output_tokens == 7));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::Done(_)))
                .count(),
            1
        );
        assert!(state.consume("[DONE]").is_empty());
    }

    #[test]
    fn malformed_payload_and_incomplete_tool_arguments_terminate_with_error() {
        let mut malformed = OpenAiStreamState::default();
        assert!(matches!(
            malformed.consume("not-json").as_slice(),
            [ProviderEvent::Error(message)] if message.to_string().contains("malformed")
        ));
        assert!(malformed.consume("[DONE]").is_empty());

        let mut incomplete = OpenAiStreamState::default();
        let delta = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"call-1","function":{"name":"grep","arguments":"{\"pattern\":"}
        }]}}]})
        .to_string();
        assert!(incomplete.consume(&delta).is_empty());
        assert!(matches!(
            incomplete.consume("[DONE]").as_slice(),
            [ProviderEvent::Error(message)] if message.to_string().contains("arguments")
        ));
    }

    #[test]
    fn malformed_stream_failure_is_protocol() {
        let mut state = OpenAiStreamState::default();
        let events = state.consume("not-json");
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == grey_core::ProviderFailureKind::Protocol));
    }

    #[test]
    fn transport_errors_are_emitted_once() {
        let mut state = OpenAiStreamState::default();
        assert!(matches!(
            state.fail(ProviderFailure::new(
                ProviderFailureKind::Transport,
                "connection reset",
            )),
            Some(ProviderEvent::Error(_))
        ));
        assert!(state
            .fail(ProviderFailure::new(
                ProviderFailureKind::Transport,
                "second error",
            ))
            .is_none());
    }

    #[test]
    fn serializes_tool_history_definitions_and_authorization() {
        let provider = OpenAiCompatibleProvider::new(
            "https://example.test/v1/".into(),
            Some("sk-secret".into()),
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
                ChatMessage::assistant("checking", vec![call.clone()]),
                ChatMessage::tool_result(&call, "contents"),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "Read one file".into(),
                input_schema: json!({"type":"object"}),
                risk: ToolRisk::ReadOnly,
            }],
            temperature: None,
        };
        let http = provider.build_request(&request).unwrap();
        assert_eq!(
            http.url().as_str(),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            http.headers()[reqwest::header::AUTHORIZATION],
            "Bearer sk-secret"
        );
        let body: Value = serde_json::from_slice(http.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"Cargo.toml"}"#
        );
        assert_eq!(body["messages"][1]["tool_call_id"], "call-1");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type":"object"})
        );
        assert!(body.get("temperature").is_none());

        let provider =
            OpenAiCompatibleProvider::new_with_usage("https://example.test/v1".into(), None, false)
                .unwrap();
        let http = provider.build_request(&request).unwrap();
        let body: Value = serde_json::from_slice(http.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn rejects_unbounded_tool_call_accumulation() {
        let mut state = OpenAiStreamState::default();
        let tool_calls = (0..=crate::MAX_TOOL_CALLS)
            .map(|index| {
                json!({
                    "index": index,
                    "id": format!("call-{index}"),
                    "function": {"name": "grep", "arguments": "{}"}
                })
            })
            .collect::<Vec<_>>();
        let payload = json!({"choices":[{"delta":{"tool_calls":tool_calls}}]}).to_string();

        assert!(
            matches!(state.consume(&payload).last(), Some(ProviderEvent::Error(message))
            if message.to_string().contains("tool calls"))
        );
        assert!(state.consume("[DONE]").is_empty());
    }

    #[test]
    fn limit_rejects_openai_text_and_tool_arguments_once() {
        let mut text = OpenAiStreamState::with_limit(3);
        let payload = json!({"choices":[{"delta":{"content":"甲乙"}}]}).to_string();
        assert!(
            matches!(text.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(text.consume("[DONE]").is_empty());

        let mut tool = OpenAiStreamState::with_limit(1);
        let payload = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"call-1","function":{"name":"grep","arguments":"{}"}
        }]}}]})
        .to_string();
        assert!(
            matches!(tool.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(tool.consume("[DONE]").is_empty());
    }

    #[test]
    fn limit_rejects_openai_tool_metadata_before_retaining_it() {
        let mut state = OpenAiStreamState::with_limit(2);
        let payload = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"oversized-id","function":{"name":"x","arguments":"{}"}
        }]}}]})
        .to_string();

        assert!(
            matches!(state.consume(&payload).as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == ProviderFailureKind::Protocol)
        );
        assert!(state.calls.is_empty());
        assert_eq!(state.tool_data_bytes, 0);
        assert!(state.consume("[DONE]").is_empty());
    }

    #[tokio::test]
    async fn http_stream_emits_one_done_and_reports_missing_done() {
        let complete_body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}})
        )
        .into_bytes();
        let (base_url, server) = crate::test_support::serve_one_sse(complete_body, None).await;
        let provider = OpenAiCompatibleProvider::new(base_url, None).unwrap();
        let request = ChatRequest::new("test", vec![ChatMessage::new(Role::User, "hello")]);
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(matches!(events.as_slice(), [ProviderEvent::Done(usage)]
            if usage.input_tokens == 3 && usage.output_tokens == 2));

        let incomplete_body = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"partial"}}]})
        )
        .into_bytes();
        let (base_url, server) = crate::test_support::serve_one_sse(incomplete_body, None).await;
        let provider = OpenAiCompatibleProvider::new(base_url, None).unwrap();
        let events = provider
            .stream_chat(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert!(
            matches!(events.as_slice(), [ProviderEvent::Delta(_), ProviderEvent::Error(message)]
            if message.to_string().contains("[DONE]"))
        );
    }

    #[tokio::test]
    async fn truncated_http_body_propagates_transport_error() {
        let body = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"partial"}}]})
        )
        .into_bytes();
        let declared_length = body.len() + 20;
        let (base_url, server) =
            crate::test_support::serve_one_sse(body, Some(declared_length)).await;
        let provider = OpenAiCompatibleProvider::new(base_url, None).unwrap();
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
    }

    #[tokio::test]
    async fn non_success_http_body_is_bounded() {
        let body = vec![b'x'; crate::MAX_ERROR_BODY_BYTES * 2];
        let (base_url, server) =
            crate::test_support::serve_one_response(body, None, "500 Internal Server Error").await;
        let provider = OpenAiCompatibleProvider::new(base_url, None).unwrap();
        let request = ChatRequest::new("test", vec![ChatMessage::new(Role::User, "hello")]);
        let error = match provider.stream_chat(&request).await {
            Ok(_) => panic!("non-success response unexpectedly produced a stream"),
            Err(error) => error,
        };
        server.await.unwrap();

        assert!(error.to_string().contains("500 Internal Server Error"));
        assert!(error.to_string().contains("truncated"));
        assert!(error.to_string().len() < crate::MAX_ERROR_BODY_BYTES + 200);
    }
}
