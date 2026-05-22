//! Thinking-budget control.
//!
//! Ported from the upstream JS module `src/model/thinking_budget.js`.
//!
//! Modern reasoning models (Qwen3, DeepSeek R1, GPT-5 reasoning, Claude with
//! extended thinking) emit "thinking" tokens before their final answer.
//! Without a soft cap a small reasoning model can spend many thousands of
//! tokens "thinking" about a trivial edit — burning context and latency.
//!
//! This module provides two pieces:
//!
//! * [`thinking_budget`] — heuristic that picks a token budget from a task
//!   type and message length. This is the agent-side budget calculator.
//!
//! * [`apply_thinking_budget`] — mutates an outgoing `chat/completions`
//!   request body so the budget is *advised* to the provider. Different
//!   providers expose different fields; we set only the fields the detected
//!   provider accepts (OpenAI rejects unknown top-level params with HTTP
//!   400, so provider gating is not optional).

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

/// Heuristic budget calculator. Picks a token budget from a coarse task
/// classification plus a small bonus for long prompts.
///
/// `[features].thinking_budget` overrides everything: when set to a
/// positive integer in config (or via `--thinking-budget`), that value
/// is returned verbatim regardless of task type.
pub fn thinking_budget(task_type: &str, message_len: usize) -> u32 {
    let override_val = crate::settings::get().thinking_budget;
    if override_val > 0 {
        return override_val;
    }
    let base = match task_type {
        "coding" | "backend" => 512,
        "editing" => 256,
        "debugging" => 768,
        "multi_step" => 1024,
        "explanation" => 384,
        _ => 256,
    };
    let bonus = if message_len > 400 { 256 } else { 0 };
    base + bonus
}

// Reasoning-model detector. Matches at start-of-string or after `/`, `-`, `_`
// so we catch `openrouter/anthropic/claude-3-7-sonnet`, `qwen3-coder`, etc.,
// without false-matching e.g. `gpt-4o3-something`.
static REASONING_MODEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(^|[/\-_])(o1|o3|o4|qwen3|qwq|deepseek-r|deepseek-v3-reason|claude-3-7|claude-4)",
    )
    .expect("reasoning model regex compiles")
});

/// Mutate the request body to add provider-gated reasoning fields.
///
/// Provider gating (preserved from the JS):
///
/// * `isOpenAICloud`  — `api.openai.com` or `openrouter.ai`
/// * `isAnthropic`    — `anthropic.com` or `claude`
/// * `isLocalLlamaCpp` — neither of the above (LM Studio / Ollama / llama.cpp)
///
/// Field application:
///
/// * `thinking: {type, budget_tokens}` — Anthropic, or local + reasoning model
/// * `reasoning_effort: low|medium|high` — reasoning models only
/// * `chat_template_kwargs.{enable_thinking,thinking_budget}` — local +
///   reasoning model only
///
/// When `disable` is true, `thinking` becomes `{type: "disabled"}`, no
/// `reasoning_effort` is set, and `chat_template_kwargs.enable_thinking` is
/// set to `false` (without a budget).
pub fn apply_thinking_budget(body: &mut Value, base_url: &str, tokens: u32, disable: bool) {
    let base_url = base_url.to_ascii_lowercase();

    let is_openai_cloud =
        base_url.contains("api.openai.com") || base_url.contains("openrouter.ai");
    let is_anthropic = base_url.contains("anthropic.com") || base_url.contains("claude");
    let is_local_llama_cpp = !is_openai_cloud && !is_anthropic;

    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_reasoning_model = REASONING_MODEL_RE.is_match(&model_name);

    let body_obj = match body.as_object_mut() {
        Some(o) => o,
        None => return, // not a JSON object — nothing we can safely mutate
    };

    // Anthropic-style `thinking` block. Anthropic always; local only when
    // the model is itself a reasoning model.
    if is_anthropic || (is_local_llama_cpp && is_reasoning_model) {
        let thinking = if disable {
            json!({ "type": "disabled" })
        } else {
            json!({ "type": "enabled", "budget_tokens": tokens })
        };
        body_obj.insert("thinking".to_string(), thinking);
    }

    // OpenAI-style `reasoning_effort`. Only safe on reasoning models — plain
    // gpt-4o / gpt-5.5 reject the field with HTTP 400.
    if is_reasoning_model && !disable {
        let effort = if tokens <= 500 {
            "low"
        } else if tokens <= 3000 {
            "medium"
        } else {
            "high"
        };
        body_obj.insert("reasoning_effort".to_string(), json!(effort));
    }

    // Qwen / llama.cpp local-only fields. Inserted under
    // `chat_template_kwargs` so the local server can forward them to the
    // chat template.
    if is_local_llama_cpp && is_reasoning_model {
        let kwargs = body_obj
            .entry("chat_template_kwargs".to_string())
            .or_insert_with(|| json!({}));
        if let Some(kwargs_obj) = kwargs.as_object_mut() {
            kwargs_obj.insert("enable_thinking".to_string(), json!(!disable));
            if !disable {
                kwargs_obj.insert("thinking_budget".to_string(), json!(tokens));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_heuristic_picks_base_plus_bonus() {
        assert_eq!(thinking_budget("coding", 100), 512);
        assert_eq!(thinking_budget("coding", 500), 512 + 256);
        assert_eq!(thinking_budget("editing", 0), 256);
        assert_eq!(thinking_budget("debugging", 401), 768 + 256);
        assert_eq!(thinking_budget("multi_step", 0), 1024);
        assert_eq!(thinking_budget("explanation", 0), 384);
        assert_eq!(thinking_budget("unknown", 0), 256);
    }

    #[test]
    fn anthropic_gets_thinking_block() {
        let mut body = json!({ "model": "claude-3-7-sonnet" });
        apply_thinking_budget(&mut body, "https://api.anthropic.com/v1", 2000, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2000);
        // Reasoning model regex matches claude-3-7 → effort is set.
        assert_eq!(body["reasoning_effort"], "medium");
        // Not local → no chat_template_kwargs.
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn anthropic_disable_sets_disabled_type() {
        let mut body = json!({ "model": "claude-4-opus" });
        apply_thinking_budget(&mut body, "https://api.anthropic.com/v1", 2000, true);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_non_reasoning_model_gets_nothing() {
        let mut body = json!({ "model": "gpt-4o" });
        apply_thinking_budget(&mut body, "https://api.openai.com/v1", 2000, false);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn openai_o3_gets_reasoning_effort_only() {
        let mut body = json!({ "model": "o3-mini" });
        apply_thinking_budget(&mut body, "https://api.openai.com/v1", 2000, false);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn local_reasoning_model_gets_all_fields() {
        let mut body = json!({ "model": "qwen3-coder-30b" });
        apply_thinking_budget(&mut body, "http://localhost:1234/v1", 400, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 400);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["thinking_budget"], 400);
    }

    #[test]
    fn local_non_reasoning_model_gets_nothing() {
        let mut body = json!({ "model": "llama-3-8b-instruct" });
        apply_thinking_budget(&mut body, "http://localhost:1234/v1", 2000, false);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn local_reasoning_disable_clears_budget_keeps_kwargs() {
        let mut body = json!({ "model": "deepseek-r1" });
        apply_thinking_budget(&mut body, "http://localhost:8080/v1", 2000, true);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert!(body["chat_template_kwargs"].get("thinking_budget").is_none());
    }

    #[test]
    fn effort_thresholds_match_spec() {
        let mk = |tokens| {
            let mut body = json!({ "model": "o3" });
            apply_thinking_budget(&mut body, "https://api.openai.com/v1", tokens, false);
            body["reasoning_effort"].as_str().unwrap().to_string()
        };
        assert_eq!(mk(100), "low");
        assert_eq!(mk(500), "low");
        assert_eq!(mk(501), "medium");
        assert_eq!(mk(3000), "medium");
        assert_eq!(mk(3001), "high");
    }

    #[test]
    fn regex_requires_boundary() {
        // `gpt-4o3-foo` must NOT match — `o3` is not after `/`, `-`, `_` start.
        // It IS preceded by nothing in `4o3` (the `o` follows `4` directly),
        // so the boundary class rejects it.
        let mut body = json!({ "model": "gpt-4o3-experiment" });
        apply_thinking_budget(&mut body, "https://api.openai.com/v1", 2000, false);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn regex_matches_after_slash() {
        let mut body = json!({ "model": "openrouter/anthropic/claude-3-7-sonnet" });
        // Openrouter is openai-cloud — only reasoning_effort applies.
        apply_thinking_budget(&mut body, "https://openrouter.ai/api/v1", 2000, false);
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("thinking").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn non_object_body_is_noop() {
        let mut body = json!("not an object");
        apply_thinking_budget(&mut body, "https://api.anthropic.com", 2000, false);
        assert_eq!(body, json!("not an object"));
    }
}
