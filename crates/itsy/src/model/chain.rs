//! Multi-model chaining (port of `src/model/chain.js`).
//!
//! Routes specific sub-tasks to cheaper/faster models, then passes the result
//! forward to the main executor model. Unlike the escalation engine
//! (failure path: local → cloud), chaining is a SUCCESS path:
//!
//!   1B classifier → 4B planner → 8B executor
//!
//! So the expensive model only runs on the hard parts.
//!
//! Configuration (`itsy.toml` / `.env`):
//!   ITSY_CHAIN=true                 enable chaining
//!   ITSY_CHAIN_CLASSIFIER=<name>    classifier model (tiny)
//!   ITSY_CHAIN_PLANNER=<name>       planner model (small)
//!   ITSY_CHAIN_EXECUTOR=<name>      executor model (main)
//!   ITSY_CHAIN_BASE_URL=<url>       shared base URL (defaults to ITSY_BASE_URL)
//!   ITSY_CHAIN_CLASSIFIER_URL=<url> per-step base URL override
//!   ITSY_CHAIN_PLANNER_URL=<url>
//!   ITSY_CHAIN_EXECUTOR_URL=<url>
//!
//! Behavior:
//! - On a new user turn, if chaining is enabled AND the task looks multi-step,
//!   call the PLANNER model with a minimal prompt to produce a numbered plan.
//! - The plan is injected as a system message for the EXECUTOR model.
//! - On simple tasks (fast complexity), skip directly to the executor.
//! - The planner call uses a stripped-down context (no tools, no history) so
//!   it stays fast and cheap — typically 500-1000 tokens total.
//!
//! If any chain step fails (model unavailable, timeout), we fall through to
//! the executor directly — chaining is best-effort, never blocking.

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::config::Config;
use crate::model::profiles::EffectiveProfile;

/// Resolved chain configuration (cached after first read — env doesn't change
/// mid-run).
#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub enabled: bool,
    pub classifier: Option<String>,
    pub planner: Option<String>,
    pub executor: Option<String>,
    pub base_url: String,
    pub classifier_url: Option<String>,
    pub planner_url: Option<String>,
    pub executor_url: Option<String>,
}

static CHAIN_CONFIG: OnceLock<ChainConfig> = OnceLock::new();

fn env_some(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

/// Return the (cached) chain configuration derived from environment.
pub fn get_chain_config() -> &'static ChainConfig {
    CHAIN_CONFIG.get_or_init(|| ChainConfig {
        enabled: env::var("ITSY_CHAIN").ok().as_deref() == Some("true"),
        classifier: env_some("ITSY_CHAIN_CLASSIFIER"),
        planner: env_some("ITSY_CHAIN_PLANNER"),
        executor: env_some("ITSY_CHAIN_EXECUTOR"),
        base_url: env_some("ITSY_CHAIN_BASE_URL")
            .or_else(|| env_some("ITSY_BASE_URL"))
            .unwrap_or_else(|| "http://localhost:1234/v1".into()),
        classifier_url: env_some("ITSY_CHAIN_CLASSIFIER_URL"),
        planner_url: env_some("ITSY_CHAIN_PLANNER_URL"),
        executor_url: env_some("ITSY_CHAIN_EXECUTOR_URL"),
    })
}

// ─── Planning ─────────────────────────────────────────────────────────────

/// Regex that matches at least one numbered list item (`1.` or `1)` at the
/// start of a line).
static NUMBERED_LIST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*\d+[\.\)]").expect("valid numbered-list regex")
});

fn build_planner_headers(config: &Config) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = env_some("OPENAI_API_KEY")
        .or_else(|| env_some("ANTHROPIC_API_KEY"))
        .or_else(|| config.model.api_key.clone());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    headers
}

/// Call the planner model to produce a lightweight numbered plan for a task.
///
/// Returns `Some(plan)` on success, or `None` if chaining is disabled, the
/// planner is unconfigured, the model is unreachable, the response is too
/// short, or the output doesn't look like a numbered plan.
pub async fn call_planner(task: &str, config: &Config) -> Option<String> {
    let chain = get_chain_config();
    if !chain.enabled {
        return None;
    }
    let model = chain.planner.as_ref()?;

    let base_url = chain.planner_url.as_ref().unwrap_or(&chain.base_url);
    let url = format!("{base_url}/chat/completions");

    let truncated: String = task.chars().take(800).collect();
    let body = json!({
        "model": model,
        "temperature": 0.1,
        "max_tokens": 512,
        "messages": [
            {
                "role": "system",
                "content": "You are a task planner. Given a coding task, output ONLY a numbered list of 2-6 concrete steps. No explanations. Just the numbered plan."
            },
            {
                "role": "user",
                "content": format!("Task: {truncated}")
            }
        ]
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;

    let resp = client
        .post(&url)
        .headers(build_planner_headers(config))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    let content = data
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if content.len() < 10 {
        return None;
    }
    if !NUMBERED_LIST_RE.is_match(content) {
        return None;
    }
    Some(content.trim().to_string())
}

/// Resolve the executor model name for a task, respecting chain config.
/// Falls back to `config.model.name` if chaining is disabled or unconfigured.
pub fn get_executor_model(_task: &str, config: &Config) -> String {
    let chain = get_chain_config();
    if !chain.enabled {
        return config.model.name.clone();
    }
    chain.executor.clone().unwrap_or_else(|| config.model.name.clone())
}

/// Resolve the executor base URL, respecting chain config.
pub fn get_executor_url(config: &Config) -> String {
    let chain = get_chain_config();
    if !chain.enabled {
        return if !config.model.base_url.is_empty() {
            config.model.base_url.clone()
        } else {
            chain.base_url.clone()
        };
    }
    chain.executor_url.clone().unwrap_or_else(|| {
        if !config.model.base_url.is_empty() {
            config.model.base_url.clone()
        } else {
            chain.base_url.clone()
        }
    })
}

/// Format a planner-produced plan for injection into the system prompt.
/// Returns an empty string if `plan` is `None`.
pub fn format_planner_injection(plan: Option<&str>) -> String {
    match plan {
        None => String::new(),
        Some(p) => format!(
            "\n\nPRE-ANALYZED PLAN (from lightweight planner model):\n{p}\n\nExecute these steps in order."
        ),
    }
}

// ─── Static chain shape ───────────────────────────────────────────────────
//
// Retained for callers that just want the shape of the chain (plan → execute
// → review) without making any network calls. The model field is filled with
// whatever the active profile resolved to.

#[derive(Debug, Clone)]
pub struct ChainStep {
    pub model: String,
    pub purpose: &'static str,
}

/// Return the static chain of steps to run for a given task type, falling
/// back to a single-step "execute" for trivial tasks.
pub fn pick_chain(profile: &EffectiveProfile, task_type: &str) -> Vec<ChainStep> {
    let model = profile.matched_key.unwrap_or("(default)").to_string();
    match task_type {
        "multi_step" | "backend" => vec![
            ChainStep { model: model.clone(), purpose: "plan" },
            ChainStep { model: model.clone(), purpose: "execute" },
            ChainStep { model, purpose: "review" },
        ],
        _ => vec![ChainStep { model, purpose: "execute" }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_injection_empty_when_none() {
        assert_eq!(format_planner_injection(None), "");
    }

    #[test]
    fn format_injection_wraps_plan() {
        let out = format_planner_injection(Some("1. do x\n2. do y"));
        assert!(out.contains("PRE-ANALYZED PLAN"));
        assert!(out.contains("1. do x"));
        assert!(out.ends_with("Execute these steps in order."));
    }

    #[test]
    fn numbered_list_detection() {
        assert!(NUMBERED_LIST_RE.is_match("1. step one\n2. step two"));
        assert!(NUMBERED_LIST_RE.is_match("  3) indented step"));
        assert!(!NUMBERED_LIST_RE.is_match("no numbers here at all"));
    }
}
