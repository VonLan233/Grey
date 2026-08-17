//! Conservative context budgeting used by every agent request.
//!
//! P2 extension: token budget allocation, tool-output truncation, rolling
//! summary, and drop-oldest fallback per spec §8.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, ContextConfig, Role, SummaryEngine, TokenCounter, ToolCall};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBudget {
    pub system: u64,
    pub history: u64,
    pub tools: u64,
    pub input: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAudit {
    pub original_chars: usize,
    pub retained_chars: usize,
    pub dropped_messages: usize,
    pub retained_tokens: u64,
    pub summary_created: bool,
    pub tool_outputs_truncated: usize,
}

pub struct ContextManager {
    config: ContextConfig,
    counter: Arc<dyn TokenCounter>,
    summarizer: Option<SummaryEngine>,
    model: String,
}

impl ContextManager {
    pub fn new(max_chars: usize) -> Self {
        let config = ContextConfig {
            max_tokens: (max_chars as f64 / 4.0) as u64,
            ..Default::default()
        };
        Self {
            config,
            counter: Arc::new(crate::CharApproxCounter),
            summarizer: None,
            model: String::new(),
        }
    }

    pub fn with_budget(
        config: ContextConfig,
        counter: Arc<dyn TokenCounter>,
        summarizer: Option<SummaryEngine>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            config,
            counter,
            summarizer,
            model: model.into(),
        }
    }

    pub async fn prepare(&self, messages: &[ChatMessage]) -> (Vec<ChatMessage>, ContextAudit) {
        let original_chars: usize = messages.iter().map(message_size).sum();
        let original_tokens: u64 = self.count_tokens(messages);

        if original_tokens <= self.config.max_tokens {
            return (
                messages.to_vec(),
                ContextAudit {
                    original_chars,
                    retained_chars: original_chars,
                    dropped_messages: 0,
                    retained_tokens: original_tokens,
                    summary_created: false,
                    tool_outputs_truncated: 0,
                },
            );
        }

        let system = messages.iter().find(|m| m.role == Role::System).cloned();

        let mut working: Vec<ChatMessage> = messages.to_vec();

        let truncated = self.truncate_tool_outputs(&mut working);

        let after_trim_tokens = self.count_tokens(&working);
        if after_trim_tokens <= self.config.max_tokens {
            let retained_chars: usize = working.iter().map(message_size).sum();
            let dropped = messages.len().saturating_sub(working.len());
            return (
                working,
                ContextAudit {
                    original_chars,
                    retained_chars,
                    dropped_messages: dropped,
                    retained_tokens: after_trim_tokens,
                    summary_created: false,
                    tool_outputs_truncated: truncated,
                },
            );
        }

        let summary_created = self.try_summarize(&mut working).await;

        let after_summary_tokens = self.count_tokens(&working);
        if after_summary_tokens <= self.config.max_tokens {
            let retained_chars: usize = working.iter().map(message_size).sum();
            let dropped = messages.len().saturating_sub(working.len());
            return (
                working,
                ContextAudit {
                    original_chars,
                    retained_chars,
                    dropped_messages: dropped,
                    retained_tokens: after_summary_tokens,
                    summary_created,
                    tool_outputs_truncated: truncated,
                },
            );
        }

        self.drop_oldest(&mut working, &system);
        Self::strip_leading_tool_messages(&mut working);
        if let Some(sys) = &system {
            if working.is_empty() || working[0].role != Role::System {
                working.insert(0, sys.clone());
            }
        }

        let retained_tokens = self.count_tokens(&working);
        let retained_chars: usize = working.iter().map(message_size).sum();
        let dropped = messages.len().saturating_sub(working.len());

        (
            working,
            ContextAudit {
                original_chars,
                retained_chars,
                dropped_messages: dropped,
                retained_tokens,
                summary_created,
                tool_outputs_truncated: truncated,
            },
        )
    }

    fn count_tokens(&self, messages: &[ChatMessage]) -> u64 {
        self.counter.count_messages(messages, &self.model)
    }

    fn truncate_tool_outputs(&self, messages: &mut [ChatMessage]) -> usize {
        let budget = self.config.tool_output_budget as usize;
        let mut truncated = 0;
        for msg in messages.iter_mut() {
            if msg.role == Role::Tool && msg.content.chars().count() > budget {
                let chars: Vec<char> = msg.content.chars().collect();
                let head: String = chars.iter().take(budget / 2).collect();
                let tail: String = chars
                    .iter()
                    .rev()
                    .take(budget / 2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let n = chars.len() - budget;
                msg.content = format!("{head}\n...[truncated {n} chars]...\n{tail}");
                truncated += 1;
            }
        }
        truncated
    }

    async fn try_summarize(&self, messages: &mut Vec<ChatMessage>) -> bool {
        let Some(ref summarizer) = self.summarizer else {
            return false;
        };
        if messages.len() <= self.config.summary_threshold {
            return false;
        }

        let system_idx = messages.iter().position(|m| m.role == Role::System);
        let keep_recent = self.config.summary_max_messages.min(messages.len());

        let split_point = messages.len().saturating_sub(keep_recent);
        if split_point == 0 {
            return false;
        }

        let start = system_idx.map(|i| i + 1).unwrap_or(0);
        if start >= split_point {
            return false;
        }

        let to_summarize: Vec<ChatMessage> = messages[start..split_point].to_vec();
        if to_summarize.is_empty() {
            return false;
        }

        match summarizer.summarize(&to_summarize).await {
            Ok(summary) => {
                let mut new_messages = Vec::new();
                if let Some(idx) = system_idx {
                    new_messages.push(messages[idx].clone());
                }
                new_messages.push(summary);
                new_messages.extend(messages[split_point..].iter().cloned());
                *messages = new_messages;
                true
            }
            Err(_) => false,
        }
    }

    fn drop_oldest(&self, messages: &mut Vec<ChatMessage>, system: &Option<ChatMessage>) {
        let target = self.config.max_tokens;
        let mut current = self.count_tokens(messages);

        let start_idx =
            if system.is_some() && messages.first().is_some_and(|m| m.role == Role::System) {
                1
            } else {
                0
            };

        let mut keep = vec![true; messages.len()];
        for (i, msg) in messages.iter().enumerate() {
            if i < start_idx {
                continue;
            }
            if current <= target {
                break;
            }
            let size = self.counter.count(&msg.content, &self.model);
            current = current.saturating_sub(size);
            keep[i] = false;
        }

        let mut new_messages = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            if keep[i] {
                new_messages.push(msg.clone());
            }
        }
        *messages = new_messages;
    }

    fn strip_leading_tool_messages(messages: &mut Vec<ChatMessage>) -> bool {
        let mut removed = false;
        while messages.first().is_some_and(|m| m.role == Role::Tool) {
            messages.remove(0);
            removed = true;
        }
        removed
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new(120_000)
    }
}

pub fn message_size(message: &ChatMessage) -> usize {
    message.content.chars().count()
        + message
            .tool_calls
            .iter()
            .map(|call: &ToolCall| call.name.len() + call.arguments.to_string().len())
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn under_budget_returns_as_is() {
        let cm = ContextManager::new(100_000);
        let messages = vec![
            ChatMessage::new(Role::System, "sys"),
            ChatMessage::new(Role::User, "hello"),
        ];
        let (prepared, audit) = cm.prepare(&messages).await;
        assert_eq!(prepared.len(), 2);
        assert_eq!(audit.dropped_messages, 0);
        assert!(!audit.summary_created);
    }

    #[tokio::test]
    async fn retains_system_and_recent_messages() {
        let messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::new(Role::User, "a".repeat(700)),
            ChatMessage::new(Role::Assistant, "b".repeat(700)),
            ChatMessage::new(Role::User, "latest"),
        ];
        let (prepared, audit) = ContextManager::new(1_024).prepare(&messages).await;
        assert_eq!(prepared.first().unwrap().role, Role::System);
        assert_eq!(prepared.last().unwrap().content, "latest");
        assert!(audit.dropped_messages > 0);
    }

    #[tokio::test]
    async fn never_starts_with_an_orphan_tool_result() {
        let call = crate::ToolCall {
            id: "one".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        };
        let messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::new(Role::User, "x".repeat(2_000)),
            ChatMessage::assistant("", vec![call.clone()]),
            ChatMessage::tool_result(&call, "y".repeat(900)),
            ChatMessage::new(Role::Assistant, "done"),
        ];
        let (prepared, _) = ContextManager::new(1_024).prepare(&messages).await;
        assert!(!prepared
            .iter()
            .find(|message| message.role != Role::System)
            .is_some_and(|message| message.role == Role::Tool));
    }

    #[tokio::test]
    async fn over_budget_triggers_dropping() {
        let cm = ContextManager::new(200);
        let messages: Vec<ChatMessage> = (0..50)
            .map(|i| ChatMessage::new(Role::User, format!("message {i} with padding")))
            .collect();
        let (prepared, audit) = cm.prepare(&messages).await;
        assert!(prepared.len() < messages.len());
        assert!(audit.dropped_messages > 0);
    }

    #[tokio::test]
    async fn tool_outputs_truncated_when_over_budget() {
        let cm = ContextManager::with_budget(
            ContextConfig {
                max_tokens: 100,
                tool_output_budget: 20,
                summary_threshold: 100,
                summary_max_messages: 5,
                ..Default::default()
            },
            Arc::new(crate::CharApproxCounter),
            None,
            "test",
        );
        let call = crate::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        };
        let messages = vec![
            ChatMessage::new(Role::System, "sys"),
            ChatMessage::assistant("", vec![call.clone()]),
            ChatMessage::tool_result(&call, "x".repeat(500)),
            ChatMessage::new(Role::User, "ok"),
        ];
        let (prepared, audit) = cm.prepare(&messages).await;
        assert_eq!(audit.tool_outputs_truncated, 1);
        let tool_msg = prepared.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content.contains("[truncated"));
    }

    #[tokio::test]
    async fn summary_not_created_without_summarizer() {
        let cm = ContextManager::with_budget(
            ContextConfig {
                max_tokens: 10,
                summary_threshold: 3,
                summary_max_messages: 2,
                ..Default::default()
            },
            Arc::new(crate::CharApproxCounter),
            None,
            "test",
        );
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage::new(Role::User, format!("msg {i} padded")))
            .collect();
        let (_, audit) = cm.prepare(&messages).await;
        assert!(!audit.summary_created);
        assert!(audit.dropped_messages > 0);
    }

    #[tokio::test]
    async fn summary_created_with_summarizer() {
        use crate::{ChatRequest, Provider, ProviderEvent, Usage};
        use anyhow::Result;
        use async_trait::async_trait;
        use futures_util::stream;
        use futures_util::stream::BoxStream;

        struct MockSummaryProvider;
        #[async_trait]
        impl Provider for MockSummaryProvider {
            fn id(&self) -> &str {
                "mock"
            }
            async fn stream_chat<'a>(
                &'a self,
                _request: &'a ChatRequest,
            ) -> Result<BoxStream<'a, ProviderEvent>> {
                Ok(Box::pin(stream::iter(vec![
                    ProviderEvent::Delta("Summary of conversation".into()),
                    ProviderEvent::Done(Usage::default()),
                ])))
            }
        }

        let summarizer =
            SummaryEngine::new(std::sync::Arc::new(MockSummaryProvider), "test-model", 100);
        let cm = ContextManager::with_budget(
            ContextConfig {
                max_tokens: 20,
                summary_threshold: 3,
                summary_max_messages: 2,
                ..Default::default()
            },
            Arc::new(crate::CharApproxCounter),
            Some(summarizer),
            "test-model",
        );
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage::new(Role::User, format!("msg {i} padded")))
            .collect();
        let (prepared, audit) = cm.prepare(&messages).await;
        assert!(audit.summary_created);
        assert!(prepared
            .iter()
            .any(|m| m.content.contains("Summary of conversation")));
    }

    #[tokio::test]
    async fn retained_tokens_never_exceeds_max_tokens() {
        let cm = ContextManager::with_budget(
            ContextConfig {
                max_tokens: 100,
                summary_threshold: 100,
                summary_max_messages: 5,
                ..Default::default()
            },
            Arc::new(crate::CharApproxCounter),
            None,
            "test",
        );
        let messages: Vec<ChatMessage> = (0..30)
            .map(|i| ChatMessage::new(Role::User, format!("message {i} with enough padding")))
            .collect();
        let (_, audit) = cm.prepare(&messages).await;
        assert!(
            audit.retained_tokens <= 100,
            "retained_tokens={} > max_tokens=100",
            audit.retained_tokens
        );
    }

    #[tokio::test]
    async fn audit_records_original_and_retained_chars() {
        let cm = ContextManager::new(100_000);
        let messages = vec![
            ChatMessage::new(Role::System, "sys"),
            ChatMessage::new(Role::User, "hello world"),
        ];
        let (_, audit) = cm.prepare(&messages).await;
        assert_eq!(audit.original_chars, audit.retained_chars);
        assert!(audit.original_chars > 0);
    }
}
