//! Token counting: tiktoken-rs for OpenAI models, char-approx for others.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ChatMessage;

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str, model: &str) -> u64;
    fn count_messages(&self, messages: &[ChatMessage], model: &str) -> u64 {
        messages
            .iter()
            .map(|message| {
                let tool_calls = message
                    .tool_calls
                    .iter()
                    .map(|call| self.count(&format!("{}{}", call.name, call.arguments), model))
                    .sum::<u64>();
                self.count(&message.content, model) + 4 + tool_calls
            })
            .sum()
    }
}

pub struct TiktokenCounter {
    encoders: Mutex<HashMap<String, Arc<tiktoken_rs::CoreBPE>>>,
}

impl TiktokenCounter {
    pub fn new() -> Self {
        Self {
            encoders: Mutex::new(HashMap::new()),
        }
    }

    fn encoder_for(&self, model: &str) -> Option<Arc<tiktoken_rs::CoreBPE>> {
        let mut cache = self.encoders.lock().unwrap();
        if let Some(enc) = cache.get(model) {
            return Some(Arc::clone(enc));
        }
        let enc = if model.starts_with("gpt-4o") {
            tiktoken_rs::o200k_base().ok()
        } else if model.starts_with("gpt-4") || model.starts_with("gpt-3.5") {
            tiktoken_rs::cl100k_base().ok()
        } else {
            None
        };
        enc.map(|e| {
            let shared = Arc::new(e);
            cache.insert(model.to_string(), Arc::clone(&shared));
            shared
        })
    }
}

impl Default for TiktokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenCounter for TiktokenCounter {
    fn count(&self, text: &str, model: &str) -> u64 {
        match self.encoder_for(model) {
            Some(enc) => enc.encode_with_special_tokens(text).len() as u64,
            None => CharApproxCounter.count(text, model),
        }
    }
}

pub struct CharApproxCounter;

impl TokenCounter for CharApproxCounter {
    fn count(&self, text: &str, _model: &str) -> u64 {
        (text.len() as u64).div_ceil(4)
    }
}

/// Select the appropriate counter for a given model name.
pub fn counter_for_model(model: &str) -> Box<dyn TokenCounter> {
    if model.starts_with("gpt-") {
        Box::new(TiktokenCounter::new())
    } else {
        Box::new(CharApproxCounter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiktoken_counts_known_openai_string() {
        let c = TiktokenCounter::new();
        let n = c.count("hello world", "gpt-4o");
        assert!(n > 0 && n <= 3, "got {n}");
    }

    #[test]
    fn char_approx_is_len_div_4() {
        assert_eq!(CharApproxCounter.count("hello world!", "x"), 3);
    }

    #[test]
    fn tiktoken_falls_back_for_unknown_model() {
        let c = TiktokenCounter::new();
        assert_eq!(c.count("hello world", "claude-sonnet-4-5"), 3);
    }

    #[test]
    fn counter_for_openai_model_returns_tiktoken() {
        let c = counter_for_model("gpt-4o");
        let n = c.count("hello", "gpt-4o");
        assert!(n > 0);
    }

    #[test]
    fn counter_for_non_openai_returns_char_approx() {
        let c = counter_for_model("glm-5.2");
        assert_eq!(c.count("hello world!", "glm-5.2"), 3);
    }
}
