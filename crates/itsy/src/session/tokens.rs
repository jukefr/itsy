//! Token & cost tracking.
//!
//! Extracts usage from OpenAI-compatible API responses, tracks cumulative
//! token + cost per session, and exposes a tiny `format_short` helper used
//! by the status bar. Adapted from OpenCode's `getUsage` pattern.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-call usage parsed from an API response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Per-million-token pricing in USD.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
}

static MODEL_PRICING: Lazy<HashMap<&'static str, Pricing>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("default", Pricing { input: 0.0, output: 0.0 });
    m.insert("claude-sonnet-4-5", Pricing { input: 3.0, output: 15.0 });
    m.insert("claude-sonnet-4-6", Pricing { input: 3.0, output: 15.0 });
    m.insert("claude-haiku-4-5", Pricing { input: 0.25, output: 1.25 });
    m.insert("gpt-5.4-mini", Pricing { input: 0.4, output: 1.6 });
    m.insert("gpt-5.4-nano", Pricing { input: 0.1, output: 0.4 });
    m.insert("deepseek-v4", Pricing { input: 0.14, output: 0.28 });
    m.insert("deepseek-v4-pro", Pricing { input: 0.5, output: 1.0 });
    m.insert("deepseek-v4-flash", Pricing { input: 0.07, output: 0.14 });
    m
});

/// Look up pricing for a model. Falls back to the free/local default.
pub fn get_pricing(model: &str) -> Pricing {
    MODEL_PRICING
        .get(model)
        .copied()
        .unwrap_or_else(|| MODEL_PRICING.get("default").copied().unwrap_or_default())
}

/// Extract token usage from an OpenAI-compatible response body.
pub fn extract_usage(response: &Value) -> Usage {
    let prompt = response.pointer("/usage/prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion =
        response.pointer("/usage/completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let total = response
        .pointer("/usage/total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt + completion);
    Usage { input_tokens: prompt, output_tokens: completion, total_tokens: total }
}

/// Very rough char-based estimator (≈ 4 chars per token).
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    ((text.chars().count() as f64) / 4.0).ceil() as u64
}

/// Cost in USD given a usage + pricing pair.
pub fn calculate_cost(usage: Usage, pricing: Pricing) -> f64 {
    let input = (usage.input_tokens as f64) * pricing.input / 1_000_000.0;
    let output = (usage.output_tokens as f64) * pricing.output / 1_000_000.0;
    input + output
}

/// Cumulative usage stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenStats {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
    pub cost: f64,
    pub calls: u64,
}

/// Session-level tracker.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenTracker {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub calls: u64,
    pub by_model: HashMap<String, u64>,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record usage from a response body. `model` is used for cost lookup
    /// and per-model aggregation.
    pub fn record(&mut self, data: &Value, model: &str) {
        let usage = extract_usage(data);
        self.prompt_tokens += usage.input_tokens;
        self.completion_tokens += usage.output_tokens;
        self.total_tokens += usage.total_tokens;
        self.cost_usd += calculate_cost(usage, get_pricing(model));
        self.calls += 1;
        *self.by_model.entry(model.to_string()).or_insert(0) += usage.total_tokens;
    }

    pub fn stats(&self) -> TokenStats {
        TokenStats {
            prompt: self.prompt_tokens,
            completion: self.completion_tokens,
            total: self.total_tokens,
            cost: self.cost_usd,
            calls: self.calls,
        }
    }

    /// Compact "1.2k tokens $0.0034" string for the status bar.
    pub fn format_short(&self) -> String {
        if self.total_tokens == 0 {
            return String::new();
        }
        let n = self.total_tokens as f64;
        let tokens_str = if n >= 1000.0 {
            format!("{:.1}k", n / 1000.0)
        } else {
            format!("{}", self.total_tokens)
        };
        if self.cost_usd > 0.0 {
            format!("{} tokens ${:.4}", tokens_str, self.cost_usd)
        } else {
            format!("{} tokens", tokens_str)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_usage() {
        let r = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 20}});
        let u = extract_usage(&r);
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.total_tokens, 30);
    }

    #[test]
    fn records_and_costs() {
        let mut t = TokenTracker::new();
        t.record(
            &json!({"usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}}),
            "claude-sonnet-4-5",
        );
        assert_eq!(t.stats().prompt, 1_000_000);
        assert!((t.cost_usd - 3.0).abs() < 1e-9);
        assert_eq!(t.calls, 1);
    }

    #[test]
    fn format_short_is_compact() {
        let mut t = TokenTracker::new();
        t.record(&json!({"usage": {"prompt_tokens": 1500, "completion_tokens": 0}}), "default");
        assert_eq!(t.format_short(), "1.5k tokens");
    }
}
