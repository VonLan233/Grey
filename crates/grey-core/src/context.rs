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
    pub tool_outputs_deduplicated: usize,
    pub tool_outputs_truncated: usize,
    pub budget: TokenBudget,
}

pub struct ContextManager {
    config: ContextConfig,
    budget: TokenBudget,
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
            budget: TokenBudget {
                system: config.system_budget,
                history: config.history_budget,
                tools: config.tool_output_budget,
                input: config.input_budget,
                total: config.max_tokens,
            },
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
            budget: TokenBudget {
                system: config.system_budget,
                history: config.history_budget,
                tools: config.tool_output_budget,
                input: config.input_budget,
                total: config.max_tokens,
            },
            config,
            counter,
            summarizer,
            model: model.into(),
        }
    }

    pub async fn prepare(&self, messages: &[ChatMessage]) -> (Vec<ChatMessage>, ContextAudit) {
        let original_chars: usize = messages.iter().map(message_size).sum();
        let mut working = messages.to_vec();
        let tool_outputs_deduplicated = self.deduplicate_semantic_tool_messages(&mut working);
        let partition_truncated = self.apply_partition_budgets(&mut working);
        let partition_tokens = self.count_tokens(&working);

        if partition_tokens <= self.config.max_tokens {
            return (
                working.clone(),
                ContextAudit {
                    original_chars,
                    retained_chars: working.iter().map(message_size).sum(),
                    dropped_messages: messages.len().saturating_sub(working.len()),
                    retained_tokens: partition_tokens,
                    summary_created: false,
                    tool_outputs_deduplicated,
                    tool_outputs_truncated: partition_truncated,
                    budget: self.budget,
                },
            );
        }

        let system = messages.iter().find(|m| m.role == Role::System).cloned();
        let truncated = partition_truncated + self.truncate_tool_outputs(&mut working);

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
                    tool_outputs_deduplicated,
                    tool_outputs_truncated: truncated,
                    budget: self.budget,
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
                    tool_outputs_deduplicated,
                    tool_outputs_truncated: truncated,
                    budget: self.budget,
                },
            );
        }

        let retained_system = working
            .iter()
            .find(|message| message.role == Role::System)
            .cloned()
            .or(system);
        self.drop_oldest(&mut working, &retained_system);
        Self::normalize_tool_messages(&mut working);
        if let Some(sys) = &retained_system {
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
                tool_outputs_deduplicated,
                tool_outputs_truncated: truncated,
                budget: self.budget,
            },
        )
    }

    fn count_tokens(&self, messages: &[ChatMessage]) -> u64 {
        self.counter.count_messages(messages, &self.model)
    }

    fn truncate_tool_outputs(&self, messages: &mut [ChatMessage]) -> usize {
        let budget = self.config.tool_output_budget.saturating_mul(4) as usize;
        let mut truncated = 0;
        for msg in messages.iter_mut() {
            if msg.role == Role::Tool && msg.content.chars().count() > budget {
                if budget == 0 {
                    msg.content.clear();
                    truncated += 1;
                    continue;
                }
                let chars: Vec<char> = msg.content.chars().collect();
                let marker = format!("\n...[truncated {} chars]...\n", chars.len() - budget);
                let available = budget.saturating_sub(marker.chars().count());
                let head_len = available.div_ceil(2);
                let tail_len = available / 2;
                let head: String = chars.iter().take(head_len).collect();
                let tail: String = chars
                    .iter()
                    .rev()
                    .take(tail_len)
                    .copied()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                msg.content = format!("{head}{marker}{tail}");
                truncated += 1;
            }
        }
        truncated
    }

    fn apply_partition_budgets(&self, messages: &mut [ChatMessage]) -> usize {
        let mut truncated = 0;
        let system_budget = self.budget.system.saturating_mul(4) as usize;
        let input_budget = self.budget.input.saturating_mul(4) as usize;
        for message in messages.iter_mut() {
            if message.role == Role::System && truncate_content(&mut message.content, system_budget)
            {
                truncated += 1;
            }
        }
        if let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == Role::User)
        {
            if truncate_content(&mut message.content, input_budget) {
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

    fn normalize_tool_messages(messages: &mut Vec<ChatMessage>) {
        let call_ids: std::collections::HashSet<String> = messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .flat_map(|message| message.tool_calls.iter().map(|call| call.id.clone()))
            .collect();
        messages.retain(|message| {
            message.role != Role::Tool
                || message
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| call_ids.contains(id))
        });
        let result_ids: std::collections::HashSet<String> = messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| message.tool_call_id.clone())
            .collect();
        for message in messages.iter_mut().filter(|m| m.role == Role::Assistant) {
            message
                .tool_calls
                .retain(|call| result_ids.contains(&call.id));
        }
        Self::strip_leading_tool_messages(messages);
    }

    fn deduplicate_semantic_tool_messages(&self, messages: &mut Vec<ChatMessage>) -> usize {
        let mut seen = std::collections::HashSet::<(String, String)>::new();
        let mut removed = 0;
        let mut index = messages.len();
        while index > 0 {
            index -= 1;
            if messages[index].role != Role::System {
                continue;
            }
            let Some((tool, path)) = parse_semantic_tool_key(&messages[index].content) else {
                continue;
            };
            if seen.insert((tool, path)) {
                continue;
            }
            messages.remove(index);
            removed += 1;
        }
        removed
    }
}

fn parse_semantic_tool_key(content: &str) -> Option<(String, String)> {
    let prefix = "[semantic-view]";
    if !content.starts_with(prefix) {
        return None;
    }
    let mut parts = content[prefix.len()..].split_whitespace();
    let tool = parts.next()?.trim().to_string();
    let path = parts
        .find_map(|part| part.strip_prefix("path="))
        .map(str::to_string)?;
    Some((tool, path))
}

fn truncate_content(content: &mut String, budget_chars: usize) -> bool {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= budget_chars {
        return false;
    }
    if budget_chars == 0 {
        content.clear();
        return true;
    }
    *content = chars.into_iter().take(budget_chars).collect();
    true
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
    async fn deduplicates_lsp_semantic_views_by_path() {
        let cm = ContextManager::new(10_000);
        let messages = vec![
            ChatMessage::new(
                Role::System,
                "[semantic-view] lsp_diagnostics path=src/main.rs shown=1 total=1",
            ),
            ChatMessage::new(
                Role::System,
                "[semantic-view] lsp_diagnostics path=src/main.rs shown=1 total=1",
            ),
            ChatMessage::new(
                Role::System,
                "[semantic-view] lsp_symbols path=src/lib.rs shown=2 total=2",
            ),
            ChatMessage::new(Role::User, "ask"),
        ];
        let (prepared, audit) = cm.prepare(&messages).await;
        assert_eq!(
            prepared
                .iter()
                .filter(|message| message.content.starts_with("[semantic-view]"))
                .count(),
            2
        );
        assert_eq!(audit.tool_outputs_deduplicated, 1);
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
