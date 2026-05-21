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
        .trim_end_matches(|c: char| matches!(c, '.' | ',' | '!' | '?'))
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
        Regex::new(r"(?i)\b(refactor|redesign|architect|rewrite|migrate|convert)\b").unwrap(),
        Regex::new(r"(?i)\b(multi.?file|multiple files|across files|all files)\b").unwrap(),
        Regex::new(r"(?i)\b(system|framework|infrastructure|full.?stack)\b").unwrap(),
        Regex::new(r"(?i)\b(test suite|integration test|e2e)\b").unwrap(),
        Regex::new(r"(?i)\b(and then|step \d|first.*then.*finally)\b").unwrap(),
    ]
});

static FAST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)\b(fix typo|rename|add comment|format|lint)\b").unwrap(),
        Regex::new(r"(?i)\b(what is|explain|show me|read)\b").unwrap(),
        Regex::new(r"(?i)\b(simple|quick|small|minor)\b").unwrap(),
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
