//! Context compaction and mid-turn eviction.
//!
//! These functions manage the conversation history budget during an agent turn.
//! [`maybe_compact`] trims oldest non-system messages between turns.
//! [`mid_turn_evict`] truncates large arguments and evicts tool results mid-turn.

use serde_json::{json, Value};

use crate::session::tokens::estimate_history_tokens;

/// Auto-compact: trim oldest non-system messages once the budget is exceeded.
/// Mirrors JS lines 700-760 but without the LLM-based summary path.
pub fn maybe_compact(history: &mut Vec<Value>) -> bool {
    let estimated = estimate_history_tokens(history);
    let s = crate::settings::get();
    let max_ctx_tokens =
        (s.detected_window as f64) * (s.max_budget_pct as f64 / 100.0);
    if (estimated as f64) <= max_ctx_tokens * 0.8 && history.len() <= 30 {
        return false;
    }
    let target = max_ctx_tokens * 0.7;
    let mut dropped = false;
    while history.len() > 6 {
        let est = estimate_history_tokens(history) as f64;
        if est < target {
            break;
        }
        let remove_idx = history
            .iter()
            .position(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));
        let Some(idx) = remove_idx else { break };
        history.remove(idx);
        dropped = true;
    }
    if dropped {
        let summary = format!(
            "[Context compacted to fit {} token budget]",
            max_ctx_tokens as u32
        );
        history.insert(0, json!({"role": "system", "content": summary}));
    }
    dropped
}

/// Mid-turn eviction: truncate large arguments in old assistant messages and
/// replace tool results with stubs. JS lines 786-863.
pub fn mid_turn_evict(history: &mut [Value]) -> u32 {
    let s = crate::settings::get();
    let max_budget = (s.detected_window as f64) * 0.6;
    if (estimate_history_tokens(history) as f64) <= max_budget {
        return 0;
    }
    // Find last assistant index with tool_calls — we won't touch that one.
    let last_assistant_idx = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("tool_calls").is_some())
        .map(|(i, _)| i)
        .next_back()
        .unwrap_or(0);
    // First pass: truncate huge args in older assistant tool_calls.
    for m in history.iter_mut().take(last_assistant_idx) {
        let Some(calls) = m.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for tc in calls.iter_mut() {
            let Some(args) = tc.pointer_mut("/function/arguments") else { continue };
            let Some(s) = args.as_str() else { continue };
            if s.len() <= 200 {
                continue;
            }
            let minimal = serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| {
                    let obj = v.as_object()?;
                    let mut out = serde_json::Map::new();
                    for (k, v) in obj.iter() {
                        match v {
                            Value::String(s) if s.len() > 100 => {
                                out.insert(
                                    k.clone(),
                                    Value::String(format!("{}…", &s[..80.min(s.len())])),
                                );
                            }
                            other => {
                                out.insert(k.clone(), other.clone());
                            }
                        }
                    }
                    Some(Value::Object(out).to_string())
                })
                .unwrap_or_else(|| "{}".into());
            *args = Value::String(minimal);
        }
    }
    // Second pass: evict tool results in the first half.
    let half = history.len() / 2;
    let mut evicted = 0u32;
    let mut i = 0;
    while i < half && i < history.len() {
        let role = history[i].get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" {
            let content = history[i]
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let approx = (content.len() / 4) as u64;
            history[i]["content"] = json!(format!("[evicted: {approx} tokens]"));
            evicted += 1;
        }
        i += 1;
        if (estimate_history_tokens(history) as f64) <= max_budget * 0.7 {
            break;
        }
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static SETTINGS_INIT: Once = Once::new();

    /// Tests share a global settings singleton — initialize it once with
    /// a tight 4k window so the compaction logic actually fires in tests.
    fn init_test_settings() {
        SETTINGS_INIT.call_once(|| {
            let mut s = crate::settings::Settings::defaults();
            s.detected_window = 4096;
            s.max_budget_pct = 70;
            crate::settings::init(s);
        });
    }

    fn user_msg(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant_msg(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    fn system_msg(text: &str) -> Value {
        json!({"role": "system", "content": text})
    }

    fn tool_msg(content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": "x", "content": content})
    }

    /// Empty / short history is left alone — compaction is a no-op.
    #[test]
    fn maybe_compact_short_history_is_noop() {
        init_test_settings();
        let mut h = vec![system_msg("sys"), user_msg("hi"), assistant_msg("hello")];
        let before = h.len();
        let dropped = maybe_compact(&mut h);
        assert!(!dropped, "short history must not compact");
        assert_eq!(h.len(), before, "short history length must be preserved");
    }

    /// When compaction fires, the system message must remain (it's the
    /// only role that's not removable). Invariant: at least one system
    /// message at index 0 after compaction.
    #[test]
    fn maybe_compact_preserves_system_role() {
        init_test_settings();
        // Build a long history with a huge user message to blow past budget.
        let huge = "x".repeat(60_000);
        let mut h = vec![system_msg("sys-prompt")];
        for i in 0..40 {
            h.push(user_msg(&format!("u{i}: {huge}")));
            h.push(assistant_msg(&format!("a{i}")));
        }

        let dropped = maybe_compact(&mut h);
        assert!(dropped, "huge history must trigger compaction");
        assert_eq!(h[0].get("role").and_then(|r| r.as_str()), Some("system"),
            "head of history must still be a system role after compaction");
    }

    /// Compaction must not panic on a history without any non-system
    /// messages (degenerate case).
    #[test]
    fn maybe_compact_handles_only_system_messages() {
        init_test_settings();
        let mut h = vec![system_msg("a"), system_msg("b"), system_msg("c")];
        let _ = maybe_compact(&mut h); // Must not panic.
        assert!(h.iter().all(|m| m.get("role").and_then(|r| r.as_str()) == Some("system")));
    }

    /// `mid_turn_evict` returns 0 when under budget — no churn for small
    /// histories.
    #[test]
    fn mid_turn_evict_returns_zero_under_budget() {
        init_test_settings();
        let mut h = vec![system_msg("s"), user_msg("hi"), assistant_msg("ok")];
        assert_eq!(mid_turn_evict(&mut h), 0,
            "under-budget history must produce zero evictions");
    }

    /// When over budget, `mid_turn_evict` replaces tool results in the
    /// first half with stubs (preserves role + tool_call_id, mutates content).
    #[test]
    fn mid_turn_evict_replaces_tool_content_with_stub() {
        init_test_settings();
        let big = "Z".repeat(40_000); // ~10k tokens
        let mut h: Vec<Value> = Vec::new();
        h.push(system_msg("s"));
        for _ in 0..20 {
            h.push(assistant_msg("calling tool"));
            h.push(tool_msg(&big));
        }
        // Add a fresh assistant message at the end so older content is "old".
        h.push(assistant_msg("latest"));

        let evicted = mid_turn_evict(&mut h);
        assert!(evicted > 0, "over-budget history must evict tool messages");

        // Some tool message in the first half must now have an `[evicted: ...]` stub.
        let stub_count = h.iter()
            .filter(|m| m.get("content").and_then(|c| c.as_str())
                .map(|s| s.starts_with("[evicted:"))
                .unwrap_or(false))
            .count();
        assert!(stub_count > 0, "expected at least one [evicted: ...] stub");
    }
}
