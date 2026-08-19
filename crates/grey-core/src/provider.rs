//! Vendor-neutral provider contracts and normalized conversation messages.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::ToolDefinition;

/// Returns the new UTF-8 byte count when a complete string fragment fits.
pub fn checked_utf8_bytes(current: usize, addition: &str, limit: usize) -> Option<usize> {
    current
        .checked_add(addition.len())
        .filter(|next| *next <= limit)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderModelRef {
    pub provider: String,
    pub model: String,
}

impl ProviderModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

impl std::fmt::Display for ProviderModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

impl std::str::FromStr for ProviderModelRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (provider, model) = s
            .split_once('/')
            .ok_or_else(|| format!("expected `provider/model`, got `{s}`"))?;
        if provider.is_empty() || model.is_empty() {
            return Err(format!("provider and model must be non-empty in `{s}`"));
        }
        Ok(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call.id.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            temperature: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    Auth,
    Authorization,
    RateLimit,
    Transport,
    Server,
    Protocol,
}

impl ProviderFailureKind {
    pub fn allows_retry_or_fallback(self) -> bool {
        matches!(self, Self::RateLimit | Self::Transport | Self::Server)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderFailure {
    kind: ProviderFailureKind,
    message: String,
    source: Option<Arc<ProviderFailureSource>>,
}

#[derive(Debug)]
struct ProviderFailureSource(String);

impl fmt::Display for ProviderFailureSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ProviderFailureSource {}

impl ProviderFailure {
    pub fn new(kind: ProviderFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: redact_provider_secrets(&message.into()),
            source: None,
        }
    }

    pub fn with_source(
        kind: ProviderFailureKind,
        message: impl Into<String>,
        source: impl fmt::Display,
    ) -> Self {
        Self {
            kind,
            message: redact_provider_secrets(&message.into()),
            source: Some(Arc::new(ProviderFailureSource(redact_provider_secrets(
                &source.to_string(),
            )))),
        }
    }

    pub fn from_error(error: anyhow::Error) -> Self {
        match error.downcast::<Self>() {
            Ok(failure) => failure,
            Err(error) => Self::with_source(
                ProviderFailureKind::Protocol,
                "provider returned an unclassified error",
                format!("{error:#}"),
            ),
        }
    }

    pub fn kind(&self) -> ProviderFailureKind {
        self.kind
    }

    pub fn allows_retry_or_fallback(&self) -> bool {
        self.kind.allows_retry_or_fallback()
    }
}

impl PartialEq for ProviderFailure {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.message == other.message
    }
}

impl Eq for ProviderFailure {}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ProviderFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

pub fn redact_provider_secrets(input: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<Value>(input) {
        redact_provider_json(&mut value);
        return serde_json::to_string(&value).unwrap_or_else(|_| "***".to_string());
    }

    redact_provider_text(input)
}

fn redact_provider_json(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(redact_provider_json),
        Value::Object(values) => values.iter_mut().for_each(|(name, value)| {
            if is_provider_secret_key(name) {
                *value = Value::String("***".into());
            } else {
                redact_provider_json(value);
            }
        }),
        Value::String(value) => *value = redact_provider_text(value),
        _ => {}
    }
}

fn is_provider_secret_key(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "api_key"
            | "api-key"
            | "apikey"
            | "x-api-key"
            | "x-goog-api-key"
            | "token"
            | "access_token"
            | "access-token"
            | "accesstoken"
            | "secret"
            | "password"
    )
}

fn redact_provider_text(input: &str) -> String {
    let mut redacted = input.to_string();
    for key in [
        "authorization",
        "x-goog-api-key",
        "x-api-key",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "accesstoken",
        "token",
        "secret",
        "password",
    ] {
        redacted = redact_assignments(&redacted, key);
    }
    redact_bearer_tokens(&redacted)
}

fn redact_assignments(input: &str, key: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    loop {
        let lowercase = remaining.to_ascii_lowercase();
        let Some(relative_key_start) = lowercase.find(key) else {
            output.push_str(remaining);
            return output;
        };
        let key_end = relative_key_start + key.len();
        if remaining[..relative_key_start]
            .chars()
            .next_back()
            .is_some_and(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            output.push_str(&remaining[..key_end]);
            remaining = &remaining[key_end..];
            continue;
        }
        let suffix = &remaining[key_end..];
        let separator_offset = suffix
            .char_indices()
            .find(|(_, character)| {
                !character.is_ascii_whitespace() && *character != '"' && *character != '\''
            })
            .map(|(index, _)| index)
            .unwrap_or(suffix.len());
        let Some(separator) = suffix[separator_offset..].chars().next() else {
            output.push_str(remaining);
            return output;
        };
        if separator != ':' && separator != '=' {
            let advance = key_end;
            output.push_str(&remaining[..advance]);
            remaining = &remaining[advance..];
            continue;
        }

        let after_separator = key_end + separator_offset + separator.len_utf8();
        let value_suffix = &remaining[after_separator..];
        let value_offset = value_suffix
            .char_indices()
            .find(|(_, character)| !character.is_ascii_whitespace())
            .map(|(index, _)| index)
            .unwrap_or(value_suffix.len());
        let value_start = after_separator + value_offset;
        let quote = remaining[value_start..]
            .chars()
            .next()
            .filter(|character| *character == '"' || *character == '\'');
        let secret_start = value_start + quote.map_or(0, char::len_utf8);
        let secret_end = if let Some(quote) = quote {
            remaining[secret_start..]
                .find(quote)
                .map(|index| secret_start + index)
                .unwrap_or(remaining.len())
        } else if key == "authorization" {
            remaining[secret_start..]
                .find(['\r', '\n', ',', ';'])
                .map(|index| secret_start + index)
                .unwrap_or(remaining.len())
        } else {
            remaining[secret_start..]
                .find(|character: char| {
                    character.is_ascii_whitespace()
                        || matches!(character, '&' | ',' | ';' | '}' | ']')
                })
                .map(|index| secret_start + index)
                .unwrap_or(remaining.len())
        };

        output.push_str(&remaining[..secret_start]);
        let secret = &remaining[secret_start..secret_end];
        if !secret.is_empty() && secret.bytes().all(|byte| byte == b'*') {
            output.push_str(secret);
        } else {
            output.push_str("***");
        }
        remaining = &remaining[secret_end..];
    }
}

fn redact_bearer_tokens(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    loop {
        let lowercase = remaining.to_ascii_lowercase();
        let Some(start) = lowercase.find("bearer ") else {
            output.push_str(remaining);
            return output;
        };
        let secret_start = start + "bearer ".len();
        let secret_end = remaining[secret_start..]
            .find(|character: char| {
                character.is_ascii_whitespace()
                    || matches!(character, '"' | '\'' | '&' | ',' | ';' | '}' | ']')
            })
            .map(|index| secret_start + index)
            .unwrap_or(remaining.len());
        output.push_str(&remaining[..secret_start]);
        let secret = &remaining[secret_start..secret_end];
        if !secret.is_empty() && secret.bytes().all(|byte| byte == b'*') {
            output.push_str(secret);
        } else {
            output.push_str("***");
        }
        remaining = &remaining[secret_end..];
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    Delta(String),
    ToolCall(ToolCall),
    Done(Usage),
    Error(ProviderFailure),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<BoxStream<'a, ProviderEvent>>;
}

pub async fn collect(
    stream: BoxStream<'_, ProviderEvent>,
) -> Result<(String, Vec<ToolCall>, Usage)> {
    use futures_util::StreamExt;

    let mut text = String::new();
    let mut calls = Vec::new();
    let mut usage = Usage::default();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::Delta(delta) => text.push_str(&delta),
            ProviderEvent::ToolCall(call) => calls.push(call),
            ProviderEvent::Done(done_usage) => usage = done_usage,
            ProviderEvent::Error(error) => return Err(error.into()),
        }
    }
    Ok((text, calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[test]
    fn tool_messages_round_trip_losslessly() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let messages = vec![
            ChatMessage::assistant("checking", vec![call.clone()]),
            ChatMessage::tool_result(&call, "file contents"),
        ];
        let json = serde_json::to_string(&messages).unwrap();
        let decoded: Vec<ChatMessage> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, messages);
    }

    #[tokio::test]
    async fn collect_aggregates_stream() {
        let stream: BoxStream<'_, ProviderEvent> = Box::pin(stream::iter(vec![
            ProviderEvent::Delta("hel".into()),
            ProviderEvent::Delta("lo".into()),
            ProviderEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "grep".into(),
                arguments: serde_json::json!({"pattern": "x"}),
            }),
            ProviderEvent::Done(Usage {
                input_tokens: 1,
                output_tokens: 2,
            }),
        ]));
        let (text, calls, usage) = collect(stream).await.unwrap();
        assert_eq!(text, "hello");
        assert_eq!(calls.len(), 1);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn provider_failure_preserves_a_redacted_source_chain() {
        let failure = ProviderFailure::with_source(
            ProviderFailureKind::Transport,
            "Authorization: Bearer message-secret",
            "request failed with token=source-secret and api_key=other-secret",
        );
        let error = anyhow::Error::new(failure);
        let diagnostic = format!("{error:#}");

        assert!(diagnostic.contains("***"));
        assert!(diagnostic.contains("request failed"));
        assert!(!diagnostic.contains("message-secret"));
        assert!(!diagnostic.contains("source-secret"));
        assert!(!diagnostic.contains("other-secret"));
    }

    #[test]
    fn provider_failure_redacts_json_secrets_without_hiding_diagnostics() {
        let json = r#"{"Authorization":"authorization-secret","api_key":"underscore-secret","api-key":"hyphen-secret","nested":[{"ApIkEy":"compact-secret"},{"TOKEN":"token-secret"},{"detail":"Bearer bearer-secret"}],"input_tokens":17,"token_count":9,"args":["visible-argument"]}"#;
        let failure = ProviderFailure::with_source(ProviderFailureKind::Protocol, json, json);
        let display = failure.to_string();
        let debug = format!("{failure:?}");
        let source = failure.source().unwrap().to_string();

        for diagnostic in [&display, &debug, &source] {
            for secret in [
                "authorization-secret",
                "underscore-secret",
                "hyphen-secret",
                "compact-secret",
                "token-secret",
                "bearer-secret",
            ] {
                assert!(
                    !diagnostic.contains(secret),
                    "leaked {secret}: {diagnostic}"
                );
            }
            assert!(diagnostic.contains("***"));
            assert!(diagnostic.contains("input_tokens"));
            assert!(diagnostic.contains("17"));
            assert!(diagnostic.contains("token_count"));
            assert!(diagnostic.contains('9'));
            assert!(diagnostic.contains("args"));
            assert!(diagnostic.contains("visible-argument"));
        }
    }
}
