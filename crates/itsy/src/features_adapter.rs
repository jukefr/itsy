//! Features adapter — bridges the agent loop to the compiled features
//! prompts in [`crate::runtime::features::prompts`]. All functions are
//! defensive: any model failure falls back to a safe default rather than
//! crashing the agent. Mirrors `bin/features_adapter.js`.

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::runtime::features::prompts::{call_prompt, call_prompt_with_endpoint};

// ─── Feature 1: Repair a malformed tool call ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepairResult {
    pub ok: bool,
    pub repaired_call: Option<String>,
    pub error: Option<String>,
}

pub async fn repair_tool_call(original_call: &str, error: &str, tool_schema: &str) -> RepairResult {
    let result = call_prompt(
        "repair_tool_call",
        json!({
            "original_call": truncate(original_call, 2000),
            "error": truncate(error, 500),
            "tool_schema": truncate(tool_schema, 1000),
        }),
    )
    .await;
    match result {
        Ok(s) => RepairResult { ok: true, repaired_call: Some(s), error: None },
        Err(e) => RepairResult { ok: false, repaired_call: None, error: Some(e.to_string()) },
    }
}

// ─── Feature 2: Summarize a large file ───────────────────────────────────────

/// Returns a summary string or `None` on any failure / files under 100 lines.
pub async fn summarize_file_compiled(file_path: &str, content: &str, target_tokens: u32) -> Option<String> {
    if content.split('\n').count() < 100 {
        return None;
    }
    let truncated = truncate(content, 8000);
    let r = call_prompt(
        "summarize_file",
        json!({
            "file_path": file_path,
            "content": truncated,
            "target_tokens": target_tokens,
        }),
    )
    .await
    .ok()?;
    Some(r)
}

// ─── Feature 4: Checkpoint approval flow ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointDecision {
    Approve,
    Reject,
    Edit,
}

pub type ApprovalHandler = Box<
    dyn Fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = CheckpointDecision> + Send>>
        + Send
        + Sync,
>;

static APPROVAL_HANDLER: once_cell::sync::OnceCell<parking_lot::Mutex<Option<ApprovalHandler>>> =
    once_cell::sync::OnceCell::new();

pub fn set_approval_handler(handler: ApprovalHandler) {
    let slot = APPROVAL_HANDLER.get_or_init(|| parking_lot::Mutex::new(None));
    *slot.lock() = Some(handler);
}

pub async fn await_checkpoint_decision(flow_run_id: &str, checkpoint_name: &str) -> CheckpointDecision {
    let Some(slot) = APPROVAL_HANDLER.get() else { return CheckpointDecision::Approve };
    let guard = slot.lock();
    let Some(handler) = guard.as_ref() else { return CheckpointDecision::Approve };
    let fut = handler(flow_run_id.to_string(), checkpoint_name.to_string());
    drop(guard);
    fut.await
}


// ─── Feature 2 (auto-commit): generate commit message ───────────────────────

pub async fn generate_commit_message(task: &str, changed_files: &[String]) -> String {
    let fallback = format!(
        "itsy: {}",
        task.chars()
            .take(50)
            .collect::<String>()
            .replace(['\n', '\r', '"', '\'', '`', '$', '\\'], " ")
            .trim()
            .to_string()
    );
    let files_joined = changed_files.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
    let r = match call_prompt(
        "commit_message",
        json!({ "task": task, "changed_files": files_joined }),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return fallback,
    };
    let cv_re = regex::Regex::new(r"^(feat|fix|docs|refactor|test|chore|style|ci|perf|build|revert)(\(.+\))?:").unwrap();
    let trimmed = r
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim_end_matches('.')
        .chars()
        .take(72)
        .collect::<String>();
    if cv_re.is_match(&trimmed) {
        trimmed
    } else {
        let short: String = trimmed.chars().take(65).collect();
        format!("chore: {short}")
    }
}

// ─── Clarifier check ────────────────────────────────────────────────────────

/// Returns `true` if the message is too vague to act on. On any model
/// failure, defers to the regex-based fallback in [`crate::session::clarify`].
pub async fn check_needs_clarification(user_message: &str) -> bool {
    let r = match call_prompt("intent_clarifier", json!({ "user_message": user_message })).await {
        Ok(s) => s,
        Err(_) => return crate::session::clarify::needs_clarification(user_message).is_some(),
    };
    r.trim().to_lowercase().starts_with("vague")
}

// ─── Validate edit ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ValidateEditResult {
    pub ok: bool,
    pub issues: Vec<String>,
}

pub async fn validate_edit_compiled(file_path: &str, content: &str, original_task: &str) -> ValidateEditResult {
    let truncated = truncate(content, 4000);
    let r = match call_prompt(
        "validate_edit",
        json!({
            "file_path": file_path,
            "content": truncated,
            "original_task": truncate(original_task, 500),
        }),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return ValidateEditResult { ok: true, issues: Vec::new() },
    };
    let lc = r.to_lowercase();
    let passed = lc.contains("ok")
        || lc.contains("correct")
        || lc.contains("looks good")
        || lc.contains("valid")
        || lc.contains("pass")
        || !lc.contains("error");
    ValidateEditResult {
        ok: passed,
        issues: if passed { Vec::new() } else { vec![truncate(&r, 200)] },
    }
}

/// Like `validate_edit_compiled` but routes the call through the second-opinion
/// model/endpoint when configured, falling back to the main model otherwise.
pub async fn validate_edit_with_config(
    file_path: &str,
    content: &str,
    original_task: &str,
    config: &crate::config::Config,
) -> ValidateEditResult {
    let truncated = truncate(content, 4000);
    let input = json!({
        "file_path": file_path,
        "content": truncated,
        "original_task": truncate(original_task, 500),
    });
    let model = config.second_opinion.resolved_model(config);
    let base_url = config.second_opinion.resolved_endpoint(config);
    let r = match call_prompt_with_endpoint("validate_edit", input, model, base_url).await {
        Ok(s) => s,
        Err(_) => return ValidateEditResult { ok: true, issues: Vec::new() },
    };
    let lc = r.to_lowercase();
    let passed = lc.contains("ok")
        || lc.contains("correct")
        || lc.contains("looks good")
        || lc.contains("valid")
        || lc.contains("pass")
        || !lc.contains("error");
    ValidateEditResult {
        ok: passed,
        issues: if passed { Vec::new() } else { vec![truncate(&r, 200)] },
    }
}

// ─── Diagnose error ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ErrorDiagnosis {
    pub kind: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub suggestion: String,
}

pub async fn diagnose_error(command: &str, stderr: &str, exit_code: i32) -> Option<ErrorDiagnosis> {
    let r = call_prompt(
        "error_diagnosis",
        json!({
            "command": truncate(command, 500),
            "stderr": truncate(stderr, 1500),
            "exit_code": exit_code.to_string(),
        }),
    )
    .await
    .ok()?;
    let cleaned = strip_fences(&r);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    Some(ErrorDiagnosis {
        kind: parsed.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        file: parsed.get("file").and_then(|v| v.as_str()).map(String::from),
        line: parsed.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
        suggestion: truncate(parsed.get("suggestion").and_then(|v| v.as_str()).unwrap_or(""), 200),
    })
}

// ─── Decompose task ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DecomposeStrategy {
    pub strategy: String,
    pub reason: String,
    pub instruction: String,
}

pub async fn decompose_task(task: &str, errors: &str, file_context: &str) -> Option<DecomposeStrategy> {
    let r = call_prompt(
        "decompose_task",
        json!({ "task": task, "errors": errors, "file_context": file_context }),
    )
    .await
    .ok()?;
    let cleaned = strip_fences(&r);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    let valid_strategies = ["split_file", "one_error_at_a_time", "rewrite_section", "extract_function"];
    let strategy = parsed
        .get("strategy")
        .and_then(|v| v.as_str())
        .filter(|s| valid_strategies.contains(s))
        .unwrap_or("rewrite_section")
        .to_string();
    Some(DecomposeStrategy {
        strategy,
        reason: truncate(parsed.get("reason").and_then(|v| v.as_str()).unwrap_or(""), 300),
        instruction: truncate(parsed.get("instruction").and_then(|v| v.as_str()).unwrap_or(""), 600),
    })
}

// ─── Semantic merge ─────────────────────────────────────────────────────────

/// Recover from a patch failure where `old_str` no longer appears. Returns
/// the full corrected file content or `None` on failure.
pub async fn semantic_merge(file_path: &str, intended_change: &str, current_content: &str) -> Option<String> {
    let r = call_prompt(
        "semantic_merge",
        json!({
            "file": file_path,
            "intended_change": intended_change,
            "current_content": current_content,
        }),
    )
    .await
    .ok()?;
    let stripped = strip_fences(&r);
    if stripped.is_empty() { None } else { Some(stripped) }
}

// ─── Contract assertion negotiation ─────────────────────────────────────────

/// Run dual-model negotiation on contract assertions.
///
/// If no distinct second-opinion model/endpoint is configured this is a no-op.
/// Otherwise:
/// 1. Second model independently proposes its own assertions for the brief.
/// 2. Both sets are merged into a combined candidate list.
/// 3. Each model reviews the candidate — if both accept, done.
/// 4. On any objection, the objecting model's revised list becomes the new
///    candidate and we loop. Max 3 rounds, then we return what we have.
///
/// Any network/model failure falls back to the main assertions unchanged.
pub async fn negotiate_assertions(
    brief: &str,
    title: &str,
    main_assertions: Vec<(String, String)>,
    config: &crate::config::Config,
) -> (Vec<(String, String)>, bool) {
    // Skip when no second opinion is configured.
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        return (main_assertions, false);
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();
    let main_model = config.model.name.clone();
    let main_url = config.model.base_url.clone();

    // Step 1: second model independently proposes assertions.
    let second_assertions =
        match ask_for_assertions(brief, title, &second_model, &second_url).await {
            Some(a) if !a.is_empty() => a,
            _ => return (main_assertions, false),
        };

    // Step 2: merge both sets (union; id collisions get a disambiguating suffix).
    let mut current = merge_assertions(main_assertions, second_assertions);

    // Step 3: negotiation loop.
    for _ in 0..3 {
        let main_rev = review_assertions(brief, &current, &main_model, &main_url).await;
        let second_rev = review_assertions(brief, &current, &second_model, &second_url).await;

        let main_revised = match main_rev {
            AssertionReview::Revise(r) if !r.is_empty() => Some(r),
            _ => None,
        };
        let second_revised = match second_rev {
            AssertionReview::Revise(r) if !r.is_empty() => Some(r),
            _ => None,
        };

        match (main_revised, second_revised) {
            (None, None) => break,
            (Some(r), None) => current = r,
            (None, Some(r)) => current = r,
            (Some(a), Some(b)) => current = merge_assertions(a, b),
        }
    }

    current.truncate(24);
    (current, true)
}

enum AssertionReview {
    Accept,
    Revise(Vec<(String, String)>),
}

async fn ask_for_assertions(
    brief: &str,
    title: &str,
    model: &str,
    base_url: &str,
) -> Option<Vec<(String, String)>> {
    let prompt = format!(
        "You are reviewing a coding task. Generate a list of testable assertions \
(acceptance criteria) that can be verified by running commands or inspecting files. \
Each assertion must be specific and concrete.\n\n\
Title: {title}\n\
Brief: {brief}\n\n\
Return ONLY a JSON array, no markdown or explanation:\n\
[{{\"id\":\"A1\",\"text\":\"<specific verifiable statement>\"}},...]"
    );
    let raw = direct_chat(&prompt, model, base_url, 120).await.ok()?;
    parse_assertion_array(&raw)
}

async fn review_assertions(
    brief: &str,
    assertions: &[(String, String)],
    model: &str,
    base_url: &str,
) -> AssertionReview {
    let assertions_json = serde_json::to_string(
        &assertions
            .iter()
            .map(|(id, text)| json!({"id": id, "text": text}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    let prompt = format!(
        "Review these contract assertions for a coding task. \
If they fully and correctly cover what needs to be done, return {{\"accept\":true}}. \
If any are missing, wrong, or too broad to verify, return the full revised list as \
{{\"accept\":false,\"revised\":[{{\"id\":\"A1\",\"text\":\"...\"}},...]}}\n\
Break broad assertions into specific verifiable ones. Remove duplicates.\n\n\
Brief: {brief}\n\nAssertions:\n{assertions_json}\n\nReturn ONLY JSON."
    );

    let raw = match direct_chat(&prompt, model, base_url, 120).await {
        Ok(s) => s,
        Err(_) => return AssertionReview::Accept,
    };

    let cleaned = strip_fences(&raw);
    let parsed: Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(_) => return AssertionReview::Accept,
    };

    if parsed.get("accept").and_then(|v| v.as_bool()).unwrap_or(true) {
        return AssertionReview::Accept;
    }

    let revised = parsed
        .get("revised")
        .and_then(|v| v.as_array())
        .map(|arr| parse_assertion_array_from_value(arr))
        .unwrap_or_default();

    if revised.is_empty() {
        AssertionReview::Accept
    } else {
        AssertionReview::Revise(revised)
    }
}

fn merge_assertions(
    a: Vec<(String, String)>,
    b: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut result: Vec<(String, String)> = Vec::new();
    for (id, text) in a.into_iter().chain(b) {
        if seen_ids.contains(&id) {
            let mut n = 2u32;
            let mut new_id = format!("{id}_{n}");
            while seen_ids.contains(&new_id) {
                n += 1;
                new_id = format!("{id}_{n}");
            }
            seen_ids.insert(new_id.clone());
            result.push((new_id, text));
        } else {
            seen_ids.insert(id.clone());
            result.push((id, text));
        }
    }
    result
}

fn parse_assertion_array(raw: &str) -> Option<Vec<(String, String)>> {
    let cleaned = strip_fences(raw);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    let arr = parsed.as_array()?;
    let items = parse_assertion_array_from_value(arr);
    if items.is_empty() { None } else { Some(items) }
}

fn parse_assertion_array_from_value(arr: &[Value]) -> Vec<(String, String)> {
    arr.iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim().to_string();
            let text = item.get("text")?.as_str()?.trim().to_string();
            if id.is_empty() || text.len() < 5 {
                return None;
            }
            Some((id, text))
        })
        .collect()
}

async fn direct_chat(
    prompt: &str,
    model: &str,
    base_url: &str,
    timeout_secs: u64,
) -> anyhow::Result<String> {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.2,
        "max_tokens": 1024,
    });
    let url = format!("{base_url}/chat/completions");
    let res = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        return Err(anyhow!("API {status}: {}", &text[..text.len().min(200)]));
    }
    let data: Value = res.json().await?;
    data.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("empty response"))
}

// ─── Availability check ──────────────────────────────────────────────────────

pub fn is_features_available() -> bool {
    // In the Rust port the features prompts module is statically linked.
    // Available iff a model name is configured.
    !crate::settings::get().model_name.is_empty()
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn strip_fences(s: &str) -> String {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string()
}
