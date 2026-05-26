//! Deterministic routing decision
//! trees expressed as enums.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub model_id: &'static str,
    pub tier: &'static str,
}

fn pick_by_path(input: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = input;
    for k in path {
        cur = cur.get(*k)?;
    }
    match cur {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        Value::Array(arr) => Some(arr.len() as f64),
        Value::Object(obj) => Some(obj.len() as f64),
        _ => None,
    }
}

pub fn coding_router_route(input: &Value) -> RouteDecision {
    if let Some(v) = pick_by_path(input, &["complexity"]) {
        if v <= 0.3 {
            return RouteDecision { model_id: "TinyClassifier", tier: "trivial" };
        }
        if v <= 0.6 {
            return RouteDecision { model_id: "SmallCoder", tier: "simple" };
        }
    }
    RouteDecision { model_id: "MediumCoder", tier: "complex" }
}

pub fn coding_router_escalate(current: &str) -> Option<RouteDecision> {
    let order = [
        ("trivial", "TinyClassifier"),
        ("simple", "SmallCoder"),
        ("complex", "MediumCoder"),
    ];
    let idx = order.iter().position(|(n, _)| *n == current)?;
    if idx + 1 >= order.len() {
        return None;
    }
    let (tier, model_id) = order[idx + 1];
    Some(RouteDecision { model_id, tier })
}

pub fn coding_router_fallback() -> RouteDecision {
    RouteDecision { model_id: "MediumCoder", tier: "fallback" }
}

#[derive(Debug, Clone)]
pub enum Router {
    CodingRouter,
}

pub fn get_router(name: &str) -> Option<Router> {
    match name {
        "coding_router" => Some(Router::CodingRouter),
        _ => None,
    }
}

/// Classify a user message into a task type. Calls the `classify_task_type`
/// prompt in the compiled cognition layer; on any failure, defers to the
/// caller-supplied regex fallback (see [`crate::governor::classify_task`]).
///
/// The compiled-layer response is one of:
/// `coding | editing | search | shell | explanation | multi_step | debugging | backend`.
/// Anything else routes through `fallback`.
pub async fn classify_task_compiled<F: Fn(&str) -> &'static str>(
    user_message: &str,
    fallback: F,
) -> &'static str {
    let result = super::prompts::call_prompt(
        "classify_task_type",
        serde_json::json!({ "user_message": user_message }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Complexity ≤ 0.3 → trivial tier (TinyClassifier).
    #[test]
    fn low_complexity_routes_to_tiny_classifier() {
        let r = coding_router_route(&json!({"complexity": 0.2}));
        assert_eq!(r.model_id, "TinyClassifier");
        assert_eq!(r.tier, "trivial");
    }

    /// 0.3 < complexity ≤ 0.6 → simple tier (SmallCoder).
    #[test]
    fn mid_complexity_routes_to_small_coder() {
        let r = coding_router_route(&json!({"complexity": 0.5}));
        assert_eq!(r.model_id, "SmallCoder");
        assert_eq!(r.tier, "simple");
    }

    /// Threshold boundary: exactly 0.3 stays in trivial.
    #[test]
    fn exactly_03_is_trivial() {
        assert_eq!(coding_router_route(&json!({"complexity": 0.3})).tier, "trivial");
    }

    /// Threshold boundary: exactly 0.6 stays in simple.
    #[test]
    fn exactly_06_is_simple() {
        assert_eq!(coding_router_route(&json!({"complexity": 0.6})).tier, "simple");
    }

    /// High complexity → complex tier (MediumCoder).
    #[test]
    fn high_complexity_routes_to_medium_coder() {
        let r = coding_router_route(&json!({"complexity": 0.9}));
        assert_eq!(r.tier, "complex");
    }

    /// Missing `complexity` field defaults to complex (fail-safe).
    /// Anti-regression: an absent field must NOT silently route to trivial.
    #[test]
    fn missing_complexity_defaults_to_complex() {
        let r = coding_router_route(&json!({}));
        assert_eq!(r.tier, "complex",
            "missing complexity must fail to the most capable tier, not the smallest");
    }

    /// Escalation walks the tier ladder: trivial → simple → complex → None.
    #[test]
    fn escalate_walks_ladder() {
        assert_eq!(coding_router_escalate("trivial").unwrap().tier, "simple");
        assert_eq!(coding_router_escalate("simple").unwrap().tier, "complex");
        assert!(coding_router_escalate("complex").is_none(),
            "complex is the top tier — must return None");
    }

    /// Unknown tier returns None (no panic, no wraparound).
    #[test]
    fn escalate_unknown_tier_returns_none() {
        assert!(coding_router_escalate("nonexistent").is_none());
    }

    /// `get_router` only knows the coding_router name.
    #[test]
    fn get_router_knows_only_coding_router() {
        assert!(get_router("coding_router").is_some());
        assert!(get_router("other").is_none());
        assert!(get_router("").is_none());
    }
}
