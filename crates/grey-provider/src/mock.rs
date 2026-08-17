//! Mock provider: a deterministic, self-contained backend for developing the
//! whole pipeline without network or API keys. Emits the full event shape
//! (deltas, a tool call, usage) so every downstream consumer is exercised.

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use grey_core::{ChatRequest, Provider, ProviderEvent, Role, ToolCall, Usage};
use serde_json::json;
use std::time::Duration;

pub struct MockProvider {
    model: String,
}

impl MockProvider {
    pub fn new(model: String) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn stream_chat<'a>(
        &'a self,
        req: &'a ChatRequest,
    ) -> Result<stream::BoxStream<'a, ProviderEvent>> {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let reply = if last_user.trim().is_empty() {
            "（mock）请说点什么。".to_string()
        } else {
            let model = if req.model.trim().is_empty() {
                &self.model
            } else {
                &req.model
            };
            format!(
                "（mock {}）收到你的消息：{last_user}\n\n这是 Grey 模拟流式输出。真实 Provider 接入后，这里会出现模型回复。",
                model
            )
        };

        let chunks: Vec<String> = split_chunks(&reply, 4);
        let mut events = Vec::new();
        for c in chunks {
            events.push(ProviderEvent::Delta(c));
        }
        // The no-tools spike demonstrates the complete event shape. Real agent
        // requests include definitions; returning a synthetic call there would
        // create an endless tool loop instead of a deterministic mock reply.
        if req.tools.is_empty() {
            events.push(ProviderEvent::ToolCall(ToolCall {
                id: "mock-call-1".into(),
                name: "grep".into(),
                arguments: json!({ "pattern": "TODO", "path": "." }),
            }));
        }
        events.push(ProviderEvent::Done(Usage {
            input_tokens: req
                .messages
                .iter()
                .map(|m| m.content.len() as u64 / 4)
                .sum(),
            output_tokens: reply.len() as u64 / 4,
        }));

        let delay = Duration::from_millis(18);
        let stream = stream::iter(events).then(move |ev| async move {
            tokio::time::sleep(delay).await;
            ev
        });

        Ok(Box::pin(stream))
    }
}

fn split_chunks(s: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(max)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use grey_core::{collect, ChatMessage, ToolDefinition, ToolRisk};

    #[tokio::test]
    async fn mock_emits_full_event_shape() {
        let p = MockProvider::new("m".into());
        let req = ChatRequest::new("m", vec![ChatMessage::new(Role::User, "你好 Grey")]);
        let stream = p.stream_chat(&req).await.unwrap();
        let (text, calls, usage) = collect(stream).await.unwrap();
        assert!(text.contains("你好 Grey"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep");
        assert!(usage.output_tokens > 0);
    }

    #[tokio::test]
    async fn mock_agent_request_completes_without_a_synthetic_tool_loop() {
        let p = MockProvider::new("configured-model".into());
        let req = ChatRequest::new(
            "override-model",
            vec![ChatMessage::new(Role::User, "hello")],
        )
        .with_tools(vec![ToolDefinition {
            name: "grep".into(),
            description: "Search files".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
        }]);
        let stream = p.stream_chat(&req).await.unwrap();
        let (text, calls, _) = collect(stream).await.unwrap();

        assert_eq!(
            text,
            "（mock override-model）收到你的消息：hello\n\n这是 Grey 模拟流式输出。真实 Provider 接入后，这里会出现模型回复。"
        );
        assert!(calls.is_empty());
    }
}
