//! Features adapter — bridges the agent loop to the compiled features
//! prompts in [`crate::compiled::features::prompts`]. All functions are
//! defensive: any model failure falls back to a safe default rather than
//! crashing the agent. Mirrors `bin/features_adapter.js`.

use anyhow::Result;
use serde_json::{json, Value};

use crate::compiled::features::prompts::call_prompt;

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

// ─── Feature 6: Plan extraction ──────────────────────────────────────────────

/// Extract structured plan steps from prose. Returns `None` if no clear plan
/// can be extracted — caller should fall back to regex parser.
pub async fn extract_plan_steps(response: &str) -> Option<Vec<String>> {
    let r = call_prompt("extract_plan", json!({ "response": response })).await.ok()?;
    let cleaned = strip_fences(&r);
    let parsed: Vec<String> = serde_json::from_str::<Value>(&cleaned).ok()?.as_array()?.iter()
        .filter_map(|v| v.as_str().map(|s| truncate(s, 200)))
        .collect();
    if parsed.len() < 2 {
        return None;
    }
    Some(parsed.into_iter().take(8).collect())
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

// ─── Availability check ──────────────────────────────────────────────────────

pub fn is_features_available() -> bool {
    // In the Rust port the features prompts module is statically linked.
    // Available iff ITSY_MODEL is configured.
    std::env::var("ITSY_MODEL").ok().filter(|s| !s.is_empty()).is_some()
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
