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

/// Like `call_prompt` but uses an explicit model name and base URL instead of
/// reading from env vars. Used by validate_edits when a second-opinion model
/// is configured.
pub async fn call_prompt_with_endpoint(
    name: &str,
    input: Value,
    model: &str,
    base_url: &str,
) -> Result<String> {
    let rendered = render(name, &input)?;
    let cache_key = derive_key(&format!("{name}:{model}:{base_url}"), &rendered);
    if let Some(hit) = cache_get(&cache_key) {
        return Ok(hit);
    }
    let client = reqwest::Client::builder().timeout(timeout_for(name)).build()?;
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": rendered}],
        "temperature": 0.1,
        "max_tokens": 512,
    });
    let url = format!("{base_url}/chat/completions");
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
    cache_put(cache_key, content.clone(), ttl_for(name));
    Ok(content)
}

pub fn known_prompts() -> &'static [&'static str] {
    &[
        "repair_tool_call",
        "summarize_file",
        "validate_edit",
        "intent_clarifier",
        "commit_message",
        "error_diagnosis",
        "decompose_task",
        "semantic_merge",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// All known prompt names render successfully with empty input.
    /// Anti-regression: a new prompt added to `known_prompts` but not to `render`
    /// would fall through to the Err arm — catch that immediately.
    #[test]
    fn every_known_prompt_renders() {
        for name in known_prompts() {
            let r = render(name, &json!({}));
            assert!(r.is_ok(), "known prompt {name} must render, got {:?}", r);
        }
    }

    /// Unknown prompt returns Err — never silently produces an empty prompt.
    #[test]
    fn unknown_prompt_returns_error() {
        let r = render("nonexistent_xyz", &json!({}));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("Unknown prompt"));
    }

    /// `summarize_file` interpolates the file path, content, and target token cap.
    #[test]
    fn summarize_file_includes_inputs() {
        let r = render("summarize_file", &json!({
            "file_path": "src/foo.rs",
            "target_tokens": 1000,
            "content": "fn foo() {}"
        })).unwrap();
        assert!(r.contains("src/foo.rs"));
        assert!(r.contains("1000"));
        assert!(r.contains("fn foo() {}"));
    }

    /// `repair_tool_call` instructs the model to return only corrected JSON.
    #[test]
    fn repair_tool_call_demands_json_only() {
        let r = render("repair_tool_call", &json!({
            "original_call": "bad json",
            "error": "missing brace",
            "tool_schema": "{}"
        })).unwrap();
        assert!(r.contains("Return ONLY valid JSON"),
            "must instruct JSON-only reply");
    }

    /// `intent_clarifier` truncates very long user messages to ~300 chars.
    /// (The prompt template itself may include the letter 'x' a couple times.)
    #[test]
    fn intent_clarifier_truncates_message() {
        let huge_msg = "x".repeat(2000);
        let r = render("intent_clarifier", &json!({"user_message": huge_msg})).unwrap();
        let xs = r.matches('x').count();
        // 300 from user_message + maybe a couple stray 'x' in the prompt template.
        assert!(xs <= 305, "user_message must be capped near 300 chars; got {xs} xs");
        assert!(xs >= 300, "user_message should be 300 chars after truncation; got {xs}");
    }

    /// `commit_message` requires conventional-commit format.
    #[test]
    fn commit_message_specifies_conventional_format() {
        let r = render("commit_message", &json!({
            "task": "fix the bug",
            "changed_files": "a.rs, b.rs"
        })).unwrap();
        assert!(r.contains("feat|fix|docs"),
            "must enumerate the allowed conventional-commit types");
        assert!(r.contains("under 72 chars"),
            "must impose subject-line length limit");
    }

    /// `error_diagnosis` truncates very long stderr.
    #[test]
    fn error_diagnosis_truncates_stderr() {
        let huge = "X".repeat(5000);
        let r = render("error_diagnosis", &json!({
            "command": "cargo test",
            "exit_code": 1,
            "stderr": huge
        })).unwrap();
        let xs = r.matches('X').count();
        assert!(xs <= 1500, "stderr must be capped at 1500 chars; got {xs}");
        assert!(r.contains("syntax|runtime|permission"),
            "must enumerate diagnosis categories");
    }

    /// `decompose_task` lists the four known strategy types.
    #[test]
    fn decompose_task_lists_strategies() {
        let r = render("decompose_task", &json!({
            "task": "fix it", "errors": "x", "file_context": "y"
        })).unwrap();
        assert!(r.contains("split_file"));
        assert!(r.contains("one_error_at_a_time"));
        assert!(r.contains("rewrite_section"));
        assert!(r.contains("extract_function"));
    }

    /// `semantic_merge` truncates large current_content to 3000 chars.
    #[test]
    fn semantic_merge_truncates_current_content() {
        let huge = "Z".repeat(10_000);
        let r = render("semantic_merge", &json!({
            "file": "x.rs",
            "intended_change": "rename foo",
            "current_content": huge
        })).unwrap();
        let zs = r.matches('Z').count();
        assert!(zs <= 3000, "current_content must be capped at 3000; got {zs}");
    }

    // ── TTLs ────────────────────────────────────────────────────────────────

    /// `ttl_for` returns longer TTLs for stable prompts, shorter for volatile ones.
    #[test]
    fn ttl_ordering_reflects_volatility() {
        // Summarize/commit are stable — long TTL.
        assert!(ttl_for("summarize_file") >= Duration::from_secs(1800));
        assert!(ttl_for("commit_message") >= Duration::from_secs(1800));
        // Error diagnosis and decompose are volatile — short TTL.
        assert!(ttl_for("error_diagnosis") < Duration::from_secs(1800));
        // semantic_merge has shortest TTL (60s).
        assert!(ttl_for("semantic_merge") <= Duration::from_secs(60));
        // Unknown gets a sensible default (600s).
        assert_eq!(ttl_for("anything_else"), Duration::from_secs(600));
    }

    /// `timeout_for` gives repair_tool_call 15s, everything else 20s.
    #[test]
    fn repair_tool_call_has_tighter_timeout() {
        assert_eq!(timeout_for("repair_tool_call"), Duration::from_millis(15_000));
        assert_eq!(timeout_for("summarize_file"), Duration::from_millis(20_000));
        assert_eq!(timeout_for("other"), Duration::from_millis(20_000));
    }

    /// `derive_key` is stable for same name + rendered text, and differs for
    /// distinct inputs.
    #[test]
    fn derive_key_is_content_addressed() {
        let k1 = derive_key("p", "hello");
        let k2 = derive_key("p", "hello");
        let k3 = derive_key("p", "world");
        let k4 = derive_key("q", "hello");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
    }

    /// `cache_put` then `cache_get` round-trips when not yet expired.
    #[test]
    fn cache_put_then_get_round_trips() {
        let key = format!("test-key-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        cache_put(key.clone(), "value".into(), Duration::from_secs(60));
        assert_eq!(cache_get(&key).as_deref(), Some("value"));
    }

    /// Expired entries return None and don't surface stale data.
    #[test]
    fn cache_expired_entries_return_none() {
        let key = format!("expired-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        cache_put(key.clone(), "stale".into(), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache_get(&key).is_none(),
            "expired entry must NOT be returned");
    }
}
