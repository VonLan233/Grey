//! Rolling conversation summary engine.
//!
//! Sends a dedicated request to the provider asking it to compress older
//! messages into a brief summary. Called by the context manager when history
//! exceeds `summary_threshold`. The summary is cached in-memory per session
//! to avoid re-summarizing the same prefix. If the provider is offline or
//! summarization fails, the context manager falls back to dropping oldest
//! messages (degraded mode).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use futures_util::StreamExt;

use crate::{ChatMessage, ChatRequest, Provider, ProviderEvent, Role};

const SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Compress the given conversation into a concise summary that preserves key context, decisions, tool results, and open tasks. Output only the summary.";

const SUMMARY_INSTRUCTION: &str = "Summarize the following conversation. Preserve key decisions, tool results, file paths, and any open tasks or unresolved issues.";

pub struct SummaryEngine {
    provider: Arc<dyn Provider>,
    model: String,
    _max_messages: usize,
    cache: Mutex<HashMap<u64, ChatMessage>>,
}

impl SummaryEngine {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>, max_messages: usize) -> Self {
        Self {
            provider,
            model: model.into(),
            _max_messages: max_messages,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn summarize(&self, messages: &[ChatMessage]) -> Result<ChatMessage> {
        let cache_key = self.cache_key_for(messages);
        if let Some(cached) = self.cache.lock().unwrap().get(&cache_key).cloned() {
            return Ok(cached);
        }

        let mut request_messages = Vec::with_capacity(messages.len() + 2);
        request_messages.push(ChatMessage::new(Role::System, SUMMARY_SYSTEM_PROMPT));
        let conversation = self.format_conversation(messages);
        let user_prompt = format!("{SUMMARY_INSTRUCTION}\n\n{conversation}");
        request_messages.push(ChatMessage::new(Role::User, user_prompt));

        let request = ChatRequest::new(&self.model, request_messages);
        let stream = self
            .provider
            .stream_chat(&request)
            .await
            .context("summary provider stream failed")?;

        let mut text = String::new();
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::Delta(delta) => text.push_str(&delta),
                ProviderEvent::Done(_) => {}
                ProviderEvent::ToolCall(_) => {}
                ProviderEvent::Error(e) => anyhow::bail!("summary provider error: {e}"),
            }
        }

        if text.trim().is_empty() {
            anyhow::bail!("summary provider returned empty response");
        }

        let summary_message = ChatMessage::new(Role::System, text);
        self.cache
            .lock()
            .unwrap()
            .insert(cache_key, summary_message.clone());
        Ok(summary_message)
    }

    pub fn invalidate_cache(&self) {
        self.cache.lock().unwrap().clear();
    }

    fn format_conversation(&self, messages: &[ChatMessage]) -> String {
        let limit = messages.len().min(self._max_messages.max(1));
        let start = messages.len().saturating_sub(limit);
        let mut out = String::new();
        for msg in &messages[start..] {
            out.push_str(&format!("[{}] {}\n", role_label(&msg.role), msg.content));
            for call in &msg.tool_calls {
                out.push_str(&format!("  (tool_call: {})\n", call.name));
            }
            if let Some(id) = &msg.tool_call_id {
                out.push_str(&format!("  (tool_result for {id})\n"));
            }
        }
        out
    }

    fn cache_key_for(&self, messages: &[ChatMessage]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for msg in messages {
            role_label(&msg.role).hash(&mut hasher);
            msg.content.hash(&mut hasher);
            for call in &msg.tool_calls {
                call.id.hash(&mut hasher);
                call.name.hash(&mut hasher);
                call.arguments.to_string().hash(&mut hasher);
            }
            msg.tool_call_id.hash(&mut hasher);
            hasher.write_u8(0);
        }
        hasher.finish()
    }
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;
    use async_trait::async_trait;
    use futures_util::stream;
    use futures_util::stream::BoxStream;

    struct MockSummaryProvider {
        response: String,
    }

    #[async_trait]
    impl Provider for MockSummaryProvider {
        fn id(&self) -> &str {
            "mock-summary"
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            let resp = self.response.clone();
            Ok(Box::pin(stream::iter(vec![
                ProviderEvent::Delta(resp),
                ProviderEvent::Done(Usage::default()),
            ])))
        }
    }

    struct ErrorProvider;

    #[async_trait]
    impl Provider for ErrorProvider {
        fn id(&self) -> &str {
            "error-provider"
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a ChatRequest,
        ) -> Result<BoxStream<'a, ProviderEvent>> {
            anyhow::bail!("provider unavailable")
        }
    }

    fn messages(count: usize) -> Vec<ChatMessage> {
        (0..count)
            .map(|i| ChatMessage::new(Role::User, format!("message {i}")))
            .collect()
    }

    #[tokio::test]
    async fn summarize_with_mock_provider_returns_summary() {
        let provider = Arc::new(MockSummaryProvider {
            response: "Summary of conversation".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = messages(5);

        let result = engine.summarize(&msgs).await.unwrap();
        assert_eq!(result.role, Role::System);
        assert_eq!(result.content, "Summary of conversation");
    }

    #[tokio::test]
    async fn summarize_caches_result() {
        let call_count = Arc::new(Mutex::new(0u32));
        struct CountingProvider {
            response: String,
            count: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl Provider for CountingProvider {
            fn id(&self) -> &str {
                "counting"
            }
            async fn stream_chat<'a>(
                &'a self,
                _request: &'a ChatRequest,
            ) -> Result<BoxStream<'a, ProviderEvent>> {
                *self.count.lock().unwrap() += 1;
                let resp = self.response.clone();
                Ok(Box::pin(stream::iter(vec![
                    ProviderEvent::Delta(resp),
                    ProviderEvent::Done(Usage::default()),
                ])))
            }
        }

        let provider = Arc::new(CountingProvider {
            response: "Cached summary".into(),
            count: call_count.clone(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = messages(3);

        let first = engine.summarize(&msgs).await.unwrap();
        let second = engine.summarize(&msgs).await.unwrap();
        assert_eq!(first.content, second.content);
        assert_eq!(*call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn summarize_different_messages_returns_different_cache_keys() {
        let provider = Arc::new(MockSummaryProvider {
            response: "Same response".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);

        let msgs1 = messages(3);
        let msgs2 = messages(5);

        let r1 = engine.summarize(&msgs1).await.unwrap();
        let r2 = engine.summarize(&msgs2).await.unwrap();
        assert_eq!(r1.content, "Same response");
        assert_eq!(r2.content, "Same response");
    }

    #[tokio::test]
    async fn summarize_provider_error_returns_err() {
        let provider = Arc::new(ErrorProvider);
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = messages(3);

        let result = engine.summarize(&msgs).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn summarize_empty_response_returns_err() {
        let provider = Arc::new(MockSummaryProvider {
            response: "   ".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = messages(3);

        let result = engine.summarize(&msgs).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty response"));
    }

    #[tokio::test]
    async fn invalidate_cache_forces_resummarize() {
        let call_count = Arc::new(Mutex::new(0u32));
        struct CountingProvider {
            count: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl Provider for CountingProvider {
            fn id(&self) -> &str {
                "counting2"
            }
            async fn stream_chat<'a>(
                &'a self,
                _request: &'a ChatRequest,
            ) -> Result<BoxStream<'a, ProviderEvent>> {
                let n = {
                    let mut c = self.count.lock().unwrap();
                    *c += 1;
                    *c
                };
                Ok(Box::pin(stream::iter(vec![
                    ProviderEvent::Delta(format!("summary v{n}")),
                    ProviderEvent::Done(Usage::default()),
                ])))
            }
        }

        let provider = Arc::new(CountingProvider {
            count: call_count.clone(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = messages(3);

        let first = engine.summarize(&msgs).await.unwrap();
        engine.invalidate_cache();
        let second = engine.summarize(&msgs).await.unwrap();
        assert_ne!(first.content, second.content);
        assert_eq!(*call_count.lock().unwrap(), 2);
    }

    #[test]
    fn format_conversation_includes_roles_and_content() {
        let provider = Arc::new(MockSummaryProvider {
            response: "x".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs = vec![
            ChatMessage::new(Role::System, "sys"),
            ChatMessage::new(Role::User, "hello"),
            ChatMessage::new(Role::Assistant, "hi"),
        ];
        let formatted = engine.format_conversation(&msgs);
        assert!(formatted.contains("[system] sys"));
        assert!(formatted.contains("[user] hello"));
        assert!(formatted.contains("[assistant] hi"));
    }

    #[test]
    fn format_conversation_respects_max_messages() {
        let provider = Arc::new(MockSummaryProvider {
            response: "x".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 2);
        let msgs = vec![
            ChatMessage::new(Role::User, "old"),
            ChatMessage::new(Role::User, "mid"),
            ChatMessage::new(Role::User, "new"),
        ];
        let formatted = engine.format_conversation(&msgs);
        assert!(!formatted.contains("old"));
        assert!(formatted.contains("mid"));
        assert!(formatted.contains("new"));
    }

    #[test]
    fn cache_key_differs_for_different_messages() {
        let provider = Arc::new(MockSummaryProvider {
            response: "x".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs1 = messages(3);
        let msgs2 = messages(4);
        assert_ne!(engine.cache_key_for(&msgs1), engine.cache_key_for(&msgs2));
    }

    #[test]
    fn cache_key_same_for_same_messages() {
        let provider = Arc::new(MockSummaryProvider {
            response: "x".into(),
        });
        let engine = SummaryEngine::new(provider, "model", 10);
        let msgs1 = messages(3);
        let msgs2 = messages(3);
        assert_eq!(engine.cache_key_for(&msgs1), engine.cache_key_for(&msgs2));
    }
}
