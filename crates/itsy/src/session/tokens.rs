//! Token usage tracker.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenTracker {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub by_model: std::collections::HashMap<String, u64>,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, data: &Value, model: &str) {
        let prompt = data.pointer("/usage/prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let completion = data.pointer("/usage/completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        self.total_tokens += prompt + completion;
        *self.by_model.entry(model.to_string()).or_insert(0) += prompt + completion;
    }

    pub fn stats(&self) -> TokenStats {
        TokenStats {
            prompt: self.prompt_tokens,
            completion: self.completion_tokens,
            total: self.total_tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenStats {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
}
