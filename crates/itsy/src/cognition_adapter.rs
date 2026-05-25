//! Cognition adapter — bridges the Rust runtime to the
//! MarrowScript-compiled cognition layer ported under
//! [`crate::runtime::cognition`]. Falls back to the hand-rolled regex
//! classifier (from [`crate::governor::classify_task`]) when the compiled
//! layer is unavailable.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::runtime::cognition::prompts::call_prompt;
use crate::runtime::cognition::router::{
    coding_router_fallback, coding_router_route, RouteDecision,
};

/// Classify a user message into a task type. Calls `classify_task_type`
/// in the compiled cognition layer; on any failure, defers to the
/// caller-supplied regex fallback.
pub async fn classify_task_compiled<F: Fn(&str) -> &'static str>(
    user_message: &str,
    fallback: F,
) -> &'static str {
    let result = call_prompt(
        "classify_task_type",
        json!({ "user_message": user_message }),
    )
    .await;
    let Ok(value) = result else { return fallback(user_message) };
    let Some(text) = value.as_str() else { return fallback(user_message) };
    let cleaned = text
        .trim()
        .to_lowercase()
        .trim_end_matches(['.', ',', '!', '?'])
        .to_string();
    match cleaned.as_str() {
        "coding" => "coding",
        "editing" => "editing",
        "search" => "search",
        "shell" => "shell",
        "explanation" => "explanation",
        "multi_step" => "multi_step",
        "debugging" => "debugging",
        "backend" => "backend",
        _ => fallback(user_message),
    }
}

/// Compress conversation history via the compiled prompt. Returns `None`
/// on any failure.
pub async fn compress_history_compiled(history: &str, max_tokens: u32) -> Option<String> {
    let result = call_prompt(
        "compress_history",
        json!({ "history": history, "max_tokens": max_tokens }),
    )
    .await
    .ok()?;
    Some(result.as_str()?.to_string())
}

/// Route to a model tier given a 0..1 complexity estimate.
pub fn route_to_tier(complexity: f64) -> RouteDecision {
    coding_router_route(&json!({ "complexity": complexity }))
}

/// Fallback router decision when no signal is available.
pub fn route_fallback() -> RouteDecision {
    coding_router_fallback()
}

static STRONG_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)\b(refactor|redesign|architect|rewrite|migrate|convert)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(multi.?file|multiple files|across files|all files)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(system|framework|infrastructure|full.?stack)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(test suite|integration test|e2e)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(and then|step \d|first.*then.*finally)\b").expect("valid regex literal"),
    ]
});

static FAST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)\b(fix typo|rename|add comment|format|lint)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(what is|explain|show me|read)\b").expect("valid regex literal"),
        Regex::new(r"(?i)\b(simple|quick|small|minor)\b").expect("valid regex literal"),
    ]
});

/// Estimate task complexity from a message. Replaces the hand-rolled
/// estimator in `src/model/router.js`.
pub fn estimate_complexity(message: &str) -> f64 {
    if message.is_empty() {
        return 0.5;
    }
    let lc = message.to_lowercase();
    let len = message.len();
    if STRONG_PATTERNS.iter().any(|r| r.is_match(&lc)) || len > 500 {
        return 0.8;
    }
    if len < 100 && FAST_PATTERNS.iter().any(|r| r.is_match(&lc)) {
        return 0.2;
    }
    0.5
}

/// Whether the compiled cognition layer is wired up.
pub fn is_compiled_cognition_available() -> bool {
    // In the Rust port the compiled layer is statically linked — always
    // available so long as `call_prompt` exists. The JS version only ever
    // reported false when the upstream JS `require` of its cognition module threw.
    true
}

/// Best-effort access to the compiled provider for direct LLM calls.
/// The JS version returns the `SmallCoder` model's provider; we return the
/// raw JSON value for now since the Rust providers live behind dedicated
/// types — callers can pattern-match if needed.
pub fn get_compiled_provider() -> Option<Value> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty message defaults to 0.5 complexity (neutral) — never panics.
    #[test]
    fn empty_message_defaults_to_05() {
        assert_eq!(estimate_complexity(""), 0.5);
    }

    /// Strong-pattern hits return 0.8 (high complexity).
    #[test]
    fn refactor_signals_high_complexity() {
        assert_eq!(estimate_complexity("refactor the auth module"), 0.8);
        assert_eq!(estimate_complexity("rewrite the parser in TypeScript"), 0.8);
        assert_eq!(estimate_complexity("migrate to a new database"), 0.8);
    }

    /// Multi-file signals also push to strong.
    #[test]
    fn multi_file_signals_high_complexity() {
        assert_eq!(estimate_complexity("change this across files"), 0.8);
        assert_eq!(estimate_complexity("multi-file edit"), 0.8);
    }

    /// Very long messages (>500 chars) score as strong regardless of content.
    #[test]
    fn long_message_signals_high_complexity() {
        let msg = "x".repeat(600);
        assert_eq!(estimate_complexity(&msg), 0.8);
    }

    /// Short fast-intent messages route to low complexity (0.2).
    #[test]
    fn fast_intents_score_low_complexity() {
        assert_eq!(estimate_complexity("fix typo in readme"), 0.2);
        assert_eq!(estimate_complexity("explain this"), 0.2);
        assert_eq!(estimate_complexity("rename foo"), 0.2);
    }

    /// Long fast-keyword messages DON'T route to fast — they get default.
    #[test]
    fn long_fast_keyword_message_is_default() {
        let msg = format!("explain in detail: {}", "more context ".repeat(20));
        assert_eq!(estimate_complexity(&msg), 0.5);
    }

    /// Neutral messages get 0.5 default.
    #[test]
    fn neutral_message_is_default() {
        assert_eq!(estimate_complexity("add a function"), 0.5);
    }

    /// `route_to_tier` delegates to the cognition router.
    #[test]
    fn route_to_tier_dispatches_to_router() {
        let r = route_to_tier(0.2);
        assert_eq!(r.tier, "trivial");
        let r = route_to_tier(0.5);
        assert_eq!(r.tier, "simple");
        let r = route_to_tier(0.9);
        assert_eq!(r.tier, "complex");
    }

    /// `route_fallback` returns the safe-default MediumCoder.
    #[test]
    fn route_fallback_is_medium() {
        let r = route_fallback();
        assert_eq!(r.tier, "fallback");
        assert_eq!(r.model_id, "MediumCoder");
    }

    /// `is_compiled_cognition_available` is true in this port (cognition is statically linked).
    #[test]
    fn compiled_cognition_is_available_in_rust_port() {
        assert!(is_compiled_cognition_available());
    }

    /// `get_compiled_provider` returns None — the Rust port has dedicated provider types.
    #[test]
    fn get_compiled_provider_is_none() {
        assert!(get_compiled_provider().is_none());
    }
}
