//! Vendor-neutral provider contracts and normalized conversation messages.

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::ToolDefinition;

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

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    Delta(String),
    ToolCall(ToolCall),
    Done(Usage),
    Error(String),
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
            ProviderEvent::Error(error) => anyhow::bail!("provider error: {error}"),
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
}
