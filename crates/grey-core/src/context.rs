//! Conservative context budgeting used by every agent request.

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, Role};

#[derive(Debug, Clone)]
pub struct ContextManager {
    max_chars: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAudit {
    pub original_chars: usize,
    pub retained_chars: usize,
    pub dropped_messages: usize,
}

impl ContextManager {
    pub fn new(max_chars: usize) -> Self {
        Self {
            max_chars: max_chars.max(1_024),
        }
    }

    pub fn prepare(&self, messages: &[ChatMessage]) -> (Vec<ChatMessage>, ContextAudit) {
        let original_chars = messages.iter().map(message_size).sum();
        if original_chars <= self.max_chars {
            return (
                messages.to_vec(),
                ContextAudit {
                    original_chars,
                    retained_chars: original_chars,
                    dropped_messages: 0,
                },
            );
        }

        let system = messages
            .iter()
            .find(|message| message.role == Role::System)
            .cloned();
        let system_chars = system.as_ref().map(message_size).unwrap_or(0);
        let mut retained_reversed = Vec::new();
        let mut retained_chars = system_chars;

        for message in messages.iter().rev() {
            if message.role == Role::System {
                continue;
            }
            let size = message_size(message);
            if !retained_reversed.is_empty() && retained_chars.saturating_add(size) > self.max_chars
            {
                break;
            }
            retained_chars = retained_chars.saturating_add(size);
            retained_reversed.push(message.clone());
        }
        retained_reversed.reverse();

        while retained_reversed
            .first()
            .is_some_and(|message| message.role == Role::Tool)
        {
            let removed = retained_reversed.remove(0);
            retained_chars = retained_chars.saturating_sub(message_size(&removed));
        }

        let mut retained = Vec::new();
        if let Some(system) = system {
            retained.push(system);
        }
        retained.extend(retained_reversed);
        let dropped_messages = messages.len().saturating_sub(retained.len());

        (
            retained,
            ContextAudit {
                original_chars,
                retained_chars,
                dropped_messages,
            },
        )
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new(120_000)
    }
}

fn message_size(message: &ChatMessage) -> usize {
    message.content.chars().count()
        + message
            .tool_calls
            .iter()
            .map(|call| call.name.len() + call.arguments.to_string().len())
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_system_and_recent_messages() {
        let messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::new(Role::User, "a".repeat(700)),
            ChatMessage::new(Role::Assistant, "b".repeat(700)),
            ChatMessage::new(Role::User, "latest"),
        ];
        let (prepared, audit) = ContextManager::new(1_024).prepare(&messages);
        assert_eq!(prepared.first().unwrap().role, Role::System);
        assert_eq!(prepared.last().unwrap().content, "latest");
        assert!(audit.dropped_messages > 0);
    }

    #[test]
    fn never_starts_with_an_orphan_tool_result() {
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
        let (prepared, _) = ContextManager::new(1_024).prepare(&messages);
        assert!(!prepared
            .iter()
            .find(|message| message.role != Role::System)
            .is_some_and(|message| message.role == Role::Tool));
    }
}
