//! Features-layer prompt dispatcher — ported from
//! upstream JS `features/prompts.js`. Self-contained: uses env config +
//! direct OpenAI-compatible HTTP, with an in-memory TTL cache. The 9 prompts
//! here back the [`crate::features_adapter`] surface.

use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const VERIFY_AND_FIX: &str = include_str!("../../assets/prompts/verify_and_fix.txt");
pub const MULTI_FILE_EDIT: &str = include_str!("../../assets/prompts/multi_file_edit.txt");
pub const SEMANTIC_MERGE: &str = include_str!("../../assets/prompts/semantic_merge.txt");
pub const ERROR_DIAGNOSIS: &str = include_str!("../../assets/prompts/error_diagnosis.txt");

// ─── Config helpers (mirrors crate::config patterns) ────────────────────────

fn base_url() -> String {
    let raw = env::var("ITSY_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| env::var("OLLAMA_HOST").ok().map(|h| format!("{h}/v1")))
        .unwrap_or_else(|| "http://localhost:1234/v1".to_string());
    crate::config::normalize_base_url(&raw)
}

fn model_name() -> Option<String> {
    env::var("ITSY_MODEL").ok().filter(|s| !s.is_empty())
}

fn build_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))
        .or_else(|_| env::var("DEEPSEEK_API_KEY"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    headers
}

// ─── In-memory cache (sha256(name + rendered) → value with TTL) ─────────────

struct CacheEntry {
    value: String,
    expires: Instant,
}

static CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, CacheEntry>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn cache_get(key: &str) -> Option<String> {
    let mut g = CACHE.lock();
    if let Some(entry) = g.get(key) {
        if Instant::now() < entry.expires {
            return Some(entry.value.clone());
        }
        g.remove(key);
    }
    None
}

fn cache_put(key: String, value: String, ttl: Duration) {
    CACHE.lock().insert(key, CacheEntry { value, expires: Instant::now() + ttl });
}

fn derive_key(name: &str, rendered: &str) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b":");
    h.update(rendered.as_bytes());
    let hex = format!("{:x}", h.finalize());
    hex[..32].to_string()
}

// ─── Templates ──────────────────────────────────────────────────────────────

fn render(name: &str, input: &Value) -> Result<String> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let n = |k: &str| input.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(match name {
        "repair_tool_call" => format!(
            "The following tool call failed. Fix the JSON and return ONLY the corrected JSON tool call arguments.\n\nOriginal call: {}\nError: {}\nTool schema: {}\n\nReturn ONLY valid JSON.",
            s("original_call"), s("error"), s("tool_schema")
        ),
        "summarize_file" => format!(
            "Summarize this {} file to function signatures and key logic. Be concise, max {} tokens.\n\n{}",
            s("file_path"), n("target_tokens"), s("content")
        ),
        "validate_edit" => format!(
            "Review this edit to {}. Task was: {}. Does the code look correct? Reply with 'ok' if it looks good, or describe any issues.\n\n{}",
            s("file_path"), s("original_task"), s("content")
        ),
        "intent_clarifier" => {
            let msg: String = s("user_message").replace('"', "\\\"").chars().take(300).collect();
            format!(
                "Is this coding task request clear enough to act on, or is it too vague?\n\nA request is VAGUE if it lacks a specific target (e.g. \"fix it\", \"make it better\", \"do the thing\").\nA request is CLEAR if it specifies what to do, even if brief (e.g. \"run tests\", \"fix the null check in auth.js\", \"add logging\").\n\nReply with ONLY one word: \"clear\" or \"vague\"\n\nRequest: \"{msg}\""
            )
        }
        "extract_plan" => {
            let resp: String = s("response").chars().take(2000).collect();
            format!(
                "Extract the numbered steps from this text. The text may contain a plan, todo list, or step-by-step instructions in any format.\n\nRules:\n- Return ONLY a JSON array of strings, one per step\n- Maximum 8 steps, minimum 2\n- Each step should be a short action phrase (under 100 chars)\n- If no clear plan exists, return: []\n\nText:\n{resp}\n\nJSON array of steps:"
            )
        }
        "commit_message" => {
            let task: String = s("task").chars().take(200).collect();
            let files: String = s("changed_files").chars().take(300).collect();
            format!(
                "Generate a git commit message for this change.\n\nTask: {task}\nChanged files: {files}\n\nRules:\n- Start with a type: feat|fix|docs|refactor|test|chore|style\n- Format: type: short description (under 72 chars total)\n- No period at end, no quotes\n- Be specific about what changed\n\nReply with ONLY the commit message, nothing else."
            )
        }
        "error_diagnosis" => {
            let stderr_trunc: String = s("stderr").chars().take(1500).collect();
            format!(
                "Analyze this command failure.\n\nCommand: {}\nExit code: {}\nOutput:\n{stderr_trunc}\n\nReturn JSON only: {{\"type\":\"syntax|runtime|permission|notfound|timeout|unknown\",\"file\":\"<path or null>\",\"line\":<number or null>,\"suggestion\":\"<one line fix>\"}}",
                s("command"), s("exit_code")
            )
        }
        "decompose_task" => {
            let task: String = s("task").chars().take(300).collect();
            let errs: String = s("errors").chars().take(500).collect();
            let ctx: String = s("file_context").chars().take(1000).collect();
            format!(
                "A coding task has failed after multiple attempts. Suggest a decomposition strategy.\n\nTask: {task}\nErrors: {errs}\nFile context: {ctx}\n\nReturn JSON: {{\"strategy\":\"split_file|one_error_at_a_time|rewrite_section|extract_function\",\"reason\":\"<why>\",\"instruction\":\"<2-3 sentence instruction for the model>\"}}"
            )
        }
        "semantic_merge" => {
            let intended: String = s("intended_change").chars().take(500).collect();
            let current: String = s("current_content").chars().take(3000).collect();
            format!(
                "A patch failed because the target text changed. Merge the intended change into the current file.\n\nFile: {}\nIntended change description: {intended}\nCurrent content:\n{current}\n\nReturn ONLY the complete corrected file content, no explanation.",
                s("file")
            )
        }
        other => return Err(anyhow!("Unknown prompt: {other}")),
    })
}

fn ttl_for(name: &str) -> Duration {
    match name {
        "summarize_file" => Duration::from_secs(3600),
        "intent_clarifier" => Duration::from_secs(1800),
        "commit_message" => Duration::from_secs(3600),
        "extract_plan" => Duration::from_secs(600),
        "error_diagnosis" => Duration::from_secs(300),
        "decompose_task" => Duration::from_secs(300),
        "semantic_merge" => Duration::from_secs(60),
        _ => Duration::from_secs(600),
    }
}

fn timeout_for(name: &str) -> Duration {
    Duration::from_millis(if name == "repair_tool_call" { 15_000 } else { 20_000 })
}

// ─── Chat call ──────────────────────────────────────────────────────────────

async fn chat(rendered: &str, timeout: Duration) -> Result<String> {
    let base = base_url();
    let model = model_name().ok_or_else(|| anyhow!("ITSY_MODEL not set — cannot call model"))?;
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": rendered}],
        "temperature": 0.1,
        "max_tokens": 512,
    });
    let url = format!("{base}/chat/completions");
    let res = client.post(&url).headers(build_headers()).json(&body).send().await
        .with_context(|| format!("POST {url}"))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        return Err(anyhow!("API {status}: {}", &text[..text.len().min(200)]));
    }
    let data: Value = res.json().await?;
    let content = data
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Empty response from model"))?
        .to_string();
    Ok(content)
}

// ─── Public dispatcher ──────────────────────────────────────────────────────

/// Dispatch a features-layer prompt by name. Returns the raw string response.
/// Caches on a sha256(name + rendered) key with per-prompt TTL.
pub async fn call_prompt(name: &str, input: Value) -> Result<String> {
    let rendered = render(name, &input)?;
    let key = derive_key(name, &rendered);
    if let Some(hit) = cache_get(&key) {
        return Ok(hit);
    }
    let value = chat(&rendered, timeout_for(name)).await?;
    cache_put(key, value.clone(), ttl_for(name));
    Ok(value)
}

pub fn known_prompts() -> &'static [&'static str] {
    &[
        "repair_tool_call",
        "summarize_file",
        "validate_edit",
        "intent_clarifier",
        "commit_message",
        "extract_plan",
        "error_diagnosis",
        "decompose_task",
        "semantic_merge",
    ]
}
