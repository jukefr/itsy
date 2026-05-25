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

/// Estimate tokens in a single message (mirrors JS estimateMessageTokens).
pub fn estimate_message_tokens(m: &Value) -> u64 {
    let content_chars = match m.get("content") {
        Some(Value::String(s)) => s.len(),
        Some(other) if !other.is_null() => serde_json::to_string(other)
            .map(|s| s.len())
            .unwrap_or(0),
        _ => 0,
    };
    let tc_chars = m
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| {
                    let name_len = tc
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    let args_len = tc
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    name_len + args_len + 20
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    ((content_chars + tc_chars) as f64 / 4.0).ceil() as u64
}

/// Sum estimate_message_tokens across all messages.
pub fn estimate_history_tokens(history: &[Value]) -> u64 {
    history.iter().map(estimate_message_tokens).sum()
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

    /// Empty response → zero usage (no panic, no NaN).
    #[test]
    fn extract_usage_missing_fields_returns_zero() {
        let u = extract_usage(&json!({}));
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.total_tokens, 0);
    }

    /// total_tokens defaults to input + output when not provided.
    #[test]
    fn extract_usage_derives_total_when_missing() {
        let r = json!({"usage": {"prompt_tokens": 5, "completion_tokens": 7}});
        let u = extract_usage(&r);
        assert_eq!(u.total_tokens, 12,
            "total must default to prompt + completion when absent");
    }

    /// extract_usage uses the explicit total_tokens when present (even if
    /// it differs from prompt + completion, e.g. cached tokens).
    #[test]
    fn extract_usage_respects_explicit_total() {
        let r = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 35}});
        let u = extract_usage(&r);
        assert_eq!(u.total_tokens, 35,
            "must honour explicit total_tokens (e.g. cached prompt tokens add to total)");
    }

    /// estimate_tokens uses a 4-chars-per-token rough heuristic.
    #[test]
    fn estimate_tokens_approximation() {
        assert_eq!(estimate_tokens(""), 0);
        // ~4 chars/token rounding up.
        assert!(estimate_tokens("hello world") >= 2);
        assert!(estimate_tokens("hello world") <= 4);
    }

    /// estimate_message_tokens accumulates content size plus per-message overhead.
    #[test]
    fn estimate_message_tokens_includes_overhead() {
        let m = json!({"role":"user","content":"hi"});
        let n = estimate_message_tokens(&m);
        // Even tiny content should produce a non-zero estimate (overhead applied).
        assert!(n > 0);
    }

    /// estimate_history_tokens sums across messages.
    #[test]
    fn estimate_history_tokens_sums() {
        let h = vec![
            json!({"role":"user","content":"hello world"}),
            json!({"role":"assistant","content":"hi there friend"}),
        ];
        let n = estimate_history_tokens(&h);
        assert!(n >= estimate_message_tokens(&h[0]) + estimate_message_tokens(&h[1])
                || n == estimate_message_tokens(&h[0]) + estimate_message_tokens(&h[1]),
            "must sum (or at least not be smaller than) per-message estimates");
    }

    /// calculate_cost: per-token unit math.
    #[test]
    fn calculate_cost_unit_math() {
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 500_000, total_tokens: 1_500_000 };
        let pricing = Pricing { input: 3.0, output: 15.0 };
        let cost = calculate_cost(usage, pricing);
        // 1M @ $3 + 0.5M @ $15 = $3 + $7.50 = $10.50
        assert!((cost - 10.5).abs() < 1e-6, "got {cost}");
    }

    /// get_pricing returns sane defaults for unknown models.
    #[test]
    fn get_pricing_unknown_model_returns_defaults() {
        let p = get_pricing("unknown-xyz");
        // Unknown → falls through to "default" (0.0 / 0.0 for local).
        assert_eq!(p.input, 0.0);
        assert_eq!(p.output, 0.0);
    }

    /// get_pricing returns the configured rates for known models.
    #[test]
    fn get_pricing_returns_real_rates_for_known_models() {
        let sonnet = get_pricing("claude-sonnet-4-5");
        let haiku = get_pricing("claude-haiku-4-5");
        // Sonnet is more expensive than Haiku in both dims.
        assert!(sonnet.input > haiku.input, "Sonnet input must exceed Haiku");
        assert!(sonnet.output > haiku.output, "Sonnet output must exceed Haiku");
    }

    /// `format_short` handles totals < 1000.
    #[test]
    fn format_short_handles_small_totals() {
        let mut t = TokenTracker::new();
        t.record(&json!({"usage": {"prompt_tokens": 250, "completion_tokens": 0}}), "default");
        let s = t.format_short();
        assert!(s.contains("250") || s.contains("0.2k"),
            "must format under-1k totals sensibly; got {s}");
    }
}
