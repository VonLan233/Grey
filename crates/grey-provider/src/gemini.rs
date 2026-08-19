//! Gemini API streaming adapter (generativelanguage.googleapis.com).

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use grey_core::{
    ChatRequest, Provider, ProviderEvent, ProviderFailure, ProviderFailureKind, ToolCall, Usage,
};
use serde_json::{json, Value};

use crate::sse::SseDecoder;

pub struct GeminiProvider {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("building Gemini HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
        })
    }

    fn build_url(&self, model: &str) -> Result<String> {
        let mut url = reqwest::Url::parse(&format!("{}/models", self.base_url))
            .context("invalid Gemini base URL or model")?;
        let model_path = format!("{model}:streamGenerateContent");
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Gemini base URL cannot accept path segments"))?
            .push(&model_path);
        url.query_pairs_mut().append_pair("alt", "sse");
        Ok(url.to_string())
    }

    fn build_request(&self, request: &ChatRequest) -> Result<reqwest::Request> {
        let url = self.build_url(&request.model)?;
        let body = request_body(request)?;
        let mut builder = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                builder = builder.header("x-goog-api-key", key);
            }
        }
        builder.build().context("building Gemini request")
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        "gemini"
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<futures_util::stream::BoxStream<'a, ProviderEvent>> {
        let http_request = self.build_request(request)?;
        let response = crate::send_http(&self.client, http_request, "Gemini provider").await?;

        let mut chunks = response.bytes_stream();
        let output = async_stream::stream! {
            let mut decoder = SseDecoder::default();
            let mut protocol = GeminiStreamState::default();

            while let Some(chunk) = chunks.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(event) = protocol.fail_failure(ProviderFailure::with_source(
                            ProviderFailureKind::Transport,
                            "Gemini stream transport failed",
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
                            "Gemini SSE framing is malformed",
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
                    "Gemini SSE stream ended with an incomplete event",
                    error,
                )) {
                    yield event;
                }
                return;
            }
            if let Some(event) = protocol.fail("stream ended before usage metadata".to_string()) {
                yield event;
            }
        };
        Ok(Box::pin(output))
    }
}

#[derive(Default)]
struct GeminiStreamState {
    done: bool,
    next_call_id: u64,
}

impl GeminiStreamState {
    fn fail(&mut self, error: String) -> Option<ProviderEvent> {
        self.fail_failure(ProviderFailure::new(ProviderFailureKind::Protocol, error))
    }

    fn fail_failure(&mut self, failure: ProviderFailure) -> Option<ProviderEvent> {
        if self.done {
            None
        } else {
            self.done = true;
            Some(ProviderEvent::Error(failure))
        }
    }

    fn consume(&mut self, payload: &str) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                return vec![ProviderEvent::Error(ProviderFailure::with_source(
                    ProviderFailureKind::Protocol,
                    "invalid Gemini JSON",
                    e,
                ))];
            }
        };

        let mut events = Vec::new();

        if let Some(candidates) = value.get("candidates").and_then(|c| c.as_array()) {
            for candidate in candidates {
                if let Some(content) = candidate.get("content") {
                    if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                events.push(ProviderEvent::Delta(text.to_string()));
                            }
                            if let Some(fc) = part.get("functionCall") {
                                if let Some(name) = fc.get("name").and_then(|n| n.as_str()) {
                                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                                    self.next_call_id += 1;
                                    events.push(ProviderEvent::ToolCall(ToolCall {
                                        id: format!("gemini-call-{name}--{}", self.next_call_id),
                                        name: name.to_string(),
                                        arguments: args,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(usage) = value.get("usageMetadata") {
            let input = usage
                .get("promptTokenCount")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("candidatesTokenCount")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            self.done = true;
            events.push(ProviderEvent::Done(Usage {
                input_tokens: input,
                output_tokens: output,
            }));
        }

        events
    }
}

fn request_body(request: &ChatRequest) -> Result<Value> {
    use grey_core::Role;

    let mut system_instruction: Option<String> = None;
    let mut contents: Vec<Value> = Vec::new();

    for message in &request.messages {
        match message.role {
            Role::System => {
                system_instruction = Some(message.content.clone());
            }
            Role::User => {
                contents.push(json!({"role": "user", "parts": [{"text": message.content}]}));
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for call in &message.tool_calls {
                    parts
                        .push(json!({"functionCall": {"name": call.name, "args": call.arguments}}));
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::Tool => {
                let call_id = message
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .unwrap_or("unknown");
                let function_name = call_id
                    .strip_prefix("gemini-call-")
                    .and_then(|id| id.split_once("--"))
                    .map(|(name, _)| name)
                    .unwrap_or(call_id);
                contents.push(json!({
                    "role": "function",
                    "parts": [{"functionResponse": {"name": function_name, "response": {"content": message.content}}}]
                }));
            }
        }
    }

    let mut body = json!({"contents": contents});
    if let Some(sys) = system_instruction {
        body["systemInstruction"] = json!({"parts": [{"text": sys}]});
    }
    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| json!({"name": tool.name, "description": tool.description, "parameters": tool.input_schema}))
            .collect();
        body["tools"] = json!({"functionDeclarations": declarations});
    }
    if let Some(temp) = request.temperature {
        body["generationConfig"] = json!({"temperature": temp});
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grey_core::{collect, ChatMessage, ChatRequest, Role, ToolCall, ToolDefinition, ToolRisk};

    #[test]
    fn builds_request_body_with_system_and_user() {
        let req = ChatRequest::new(
            "gemini-2.0-flash",
            vec![
                ChatMessage::new(Role::System, "be helpful"),
                ChatMessage::new(Role::User, "hello"),
            ],
        );
        let body = request_body(&req).unwrap();
        assert!(body.get("systemInstruction").is_some());
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        assert_eq!(body["contents"][0]["role"], "user");
    }

    #[test]
    fn builds_request_body_with_tool_declarations() {
        let req = ChatRequest::new("m", vec![ChatMessage::new(Role::User, "hi")]).with_tools(vec![
            ToolDefinition {
                name: "grep".into(),
                description: "search".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
        ]);
        let body = request_body(&req).unwrap();
        assert_eq!(body["tools"]["functionDeclarations"][0]["name"], "grep");
    }

    #[test]
    fn build_url_includes_alt_sse_and_model_path() {
        let provider = GeminiProvider::new(
            "https://generativelanguage.googleapis.com/v1beta/".into(),
            None,
        )
        .unwrap();
        let url = provider.build_url("gemini-2.5-flash").unwrap();
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn build_request_adds_x_goog_api_key_when_present() {
        let provider =
            GeminiProvider::new("http://localhost:11434/v1".into(), Some("k-123".into())).unwrap();
        let request =
            ChatRequest::new("gemini-2.0-flash", vec![ChatMessage::new(Role::User, "hi")]);
        let http_request = provider.build_request(&request).unwrap();

        let api_key = http_request
            .headers()
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert_eq!(api_key, "k-123");
    }

    #[test]
    fn malformed_stream_failure_is_protocol() {
        let mut state = GeminiStreamState::default();
        let events = state.consume("not-json");
        assert!(matches!(events.as_slice(), [ProviderEvent::Error(failure)]
            if failure.kind() == grey_core::ProviderFailureKind::Protocol));
    }

    #[test]
    fn request_body_maps_gemini_function_response_name_from_call_id_prefix() {
        let call = ToolCall {
            id: "gemini-call-grep--1".into(),
            name: "grep".into(),
            arguments: serde_json::json!({"pattern":"todo"}),
        };
        let req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![
                ChatMessage::assistant("", vec![call.clone()]),
                ChatMessage::tool_result(&call, "tool output"),
            ],
        );
        let body = request_body(&req).unwrap();
        let maybe_function = body["contents"].as_array().and_then(|contents| {
            contents
                .iter()
                .find(|entry| entry["role"] == "function")
                .cloned()
        });
        let function_response = maybe_function.unwrap_or_else(|| panic!("missing function role"));
        assert_eq!(
            function_response["parts"][0]["functionResponse"]["name"],
            "grep"
        );
        assert_eq!(
            function_response["parts"][0]["functionResponse"]["response"]["content"],
            "tool output"
        );
    }

    #[test]
    fn parses_text_delta_from_sse() {
        let payload = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        let mut state = GeminiStreamState::default();
        let events = state.consume(payload);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::Delta(t) => assert_eq!(t, "hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn parses_tool_call_from_sse() {
        let payload = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"grep","args":{"pattern":"todo"}}}]}}]}"#;
        let mut state = GeminiStreamState::default();
        let events = state.consume(payload);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::ToolCall(call) => {
                assert_eq!(call.name, "grep");
                assert_eq!(call.arguments["pattern"], "todo");
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn parses_usage_and_marks_done() {
        let payload = r#"{"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}"#;
        let mut state = GeminiStreamState::default();
        let events = state.consume(payload);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::Done(usage) => {
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 3);
            }
            _ => panic!("expected Done"),
        }
        assert!(state.done);
    }

    #[tokio::test]
    async fn gemini_provider_streaming_with_mock_sse() {
        use crate::test_support::serve_one_sse;

        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]}}]}\n\n",
            "data: {\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":2}}\n\n"
        );
        let (url, _task) = serve_one_sse(sse_body.as_bytes().to_vec(), None).await;

        let provider = GeminiProvider::new(url, None).unwrap();
        let req = ChatRequest::new("gemini-2.0-flash", vec![ChatMessage::new(Role::User, "hi")]);
        let stream = provider.stream_chat(&req).await.unwrap();
        let (text, calls, usage) = collect(stream).await.unwrap();

        assert_eq!(text, "hello");
        assert!(calls.is_empty());
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 2);
    }
}
