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
