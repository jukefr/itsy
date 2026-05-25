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

#[allow(clippy::await_holding_lock)]
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
    let cv_re = regex::Regex::new(r"^(feat|fix|docs|refactor|test|chore|style|ci|perf|build|revert)(\(.+\))?:").expect("valid regex literal");
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

pub async fn decompose_task(
    task: &str,
    errors: &str,
    file_context: &str,
    config: &crate::config::Config,
) -> Option<DecomposeStrategy> {
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
    let main_reason = truncate(parsed.get("reason").and_then(|v| v.as_str()).unwrap_or(""), 300);
    let main_instruction = truncate(parsed.get("instruction").and_then(|v| v.as_str()).unwrap_or(""), 600);

    // Second opinion: ask the second model for its diagnosis. If it disagrees on
    // strategy, surface both perspectives in the instruction so the model gets
    // a richer view of why it's stuck.
    let instruction = if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        main_instruction
    } else {
        let second_model = config.second_opinion.resolved_model(config).to_string();
        let second_url = config.second_opinion.resolved_endpoint(config).to_string();
        let task_s: String = task.chars().take(300).collect();
        let errors_s: String = errors.chars().take(500).collect();
        let ctx_s: String = file_context.chars().take(1000).collect();
        let prompt = format!(
            "A coding task has failed after multiple attempts. Suggest a decomposition strategy.\n\n\
             Task: {task_s}\nErrors: {errors_s}\nFile context: {ctx_s}\n\n\
             Return JSON: {{\"strategy\":\"split_file|one_error_at_a_time|rewrite_section|extract_function\",\
             \"reason\":\"<why>\",\"instruction\":\"<2-3 sentence instruction for the model>\"}}"
        );
        match direct_chat(&prompt, &second_model, &second_url, 120).await {
            Ok(raw) => {
                let v: Value = serde_json::from_str(&strip_fences(&raw)).unwrap_or(Value::Null);
                let second_instr = truncate(v.get("instruction").and_then(|v| v.as_str()).unwrap_or(""), 400);
                let second_strat = v.get("strategy").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !second_instr.is_empty() && second_strat != strategy {
                    format!("{main_instruction}\n[Second opinion — {second_strat}]: {second_instr}")
                } else if !second_instr.is_empty() {
                    format!("{main_instruction} {second_instr}")
                } else {
                    main_instruction
                }
            }
            Err(_) => main_instruction,
        }
    };

    Some(DecomposeStrategy { strategy, reason: main_reason, instruction })
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

// ─── Second-opinion assertion verification ───────────────────────────────────

/// Ask the second model whether a single assertion is genuinely passed.
/// Returns `None` (verified OK) or `Some(reason)` (disputed).
/// No-op when second opinion is not configured.
pub async fn verify_assertion_passed(
    assertion_text: &str,
    evidence: &str,
    command: Option<&str>,
    exit_code: Option<i64>,
    observation: Option<&str>,
    config: &crate::config::Config,
) -> Option<String> {
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        return None;
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();

    let mut ev_block = format!("Description: {evidence}");
    if let (Some(cmd), Some(ec), Some(obs)) = (command, exit_code, observation) {
        ev_block.push_str(&format!("\nCommand: {cmd}\nExit code: {ec}\nOutput: {obs}"));
    }

    let prompt = format!(
        "You are an independent verifier checking whether a software task assertion was correctly verified.\n\n\
         Assertion: {assertion_text}\n\nEvidence:\n{ev_block}\n\n\
         Is this assertion ACTUALLY passed based on the evidence?\n\
         - Return {{\"verified\":true}} if the evidence clearly and specifically confirms it.\n\
         - Return {{\"verified\":false,\"reason\":\"...\"}} if the evidence is insufficient, vague, or contradicts the assertion.\n\n\
         Return ONLY JSON."
    );

    let raw = direct_chat(&prompt, &second_model, &second_url, 120).await.ok()?;
    let parsed: Value = serde_json::from_str(&strip_fences(&raw)).ok()?;
    if parsed.get("verified").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    Some(
        parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("insufficient evidence")
            .to_string(),
    )
}

/// Ask the second model whether ALL passed assertions together represent a complete solution.
/// Returns `None` (all good) or `Some(disputed_ids)`.
/// No-op when second opinion is not configured.
pub async fn verify_contract_complete(
    brief: &str,
    assertions: &[(String, String, String)], // (id, text, evidence)
    config: &crate::config::Config,
) -> Option<Vec<String>> {
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        return None;
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();

    let list = assertions
        .iter()
        .map(|(id, text, ev)| format!("[{id}] {text}\n       evidence: {ev}"))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are doing a final review before a software task is marked complete.\n\n\
         Task brief: {brief}\n\nAssertions marked passed:\n{list}\n\n\
         Are you confident ALL assertions are genuinely satisfied and the solution is complete?\n\
         - Return {{\"accept\":true}} if yes.\n\
         - Return {{\"accept\":false,\"disputed\":[\"A1\"],\"reason\":\"...\"}} if you doubt any.\n\n\
         Return ONLY JSON."
    );

    let raw = direct_chat(&prompt, &second_model, &second_url, 120).await.ok()?;
    let parsed: Value = serde_json::from_str(&strip_fences(&raw)).ok()?;
    if parsed.get("accept").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    let disputed = parsed
        .get("disputed")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
        .unwrap_or_default();
    if disputed.is_empty() { None } else { Some(disputed) }
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
        eprintln!("[negotiate] skipped: no second_opinion configured");
        return (main_assertions, false);
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();
    let main_model = config.model.name.clone();
    let main_url = config.model.base_url.clone();
    eprintln!("[negotiate] start: main={main_model} second={second_model} url={second_url}");

    // Step 1: second model independently proposes assertions.
    let second_assertions =
        match ask_for_assertions(brief, title, &second_model, &second_url).await {
            Some(a) if !a.is_empty() => {
                eprintln!("[negotiate] second model returned {} assertions", a.len());
                a
            }
            _ => {
                eprintln!("[negotiate] second model returned empty/None — falling back");
                return (main_assertions, false);
            }
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
For every constraint in the brief, write an assertion that verifies the constraint \
directly — not a proxy. If the brief constrains the *content* of a modification (what \
something becomes, what range/set it must come from, what shape it must have), the \
assertion must check the modification itself, not just the existence or integrity of \
related files. A file-unchanged or compile-succeeded check does not verify a content \
constraint.\n\n\
Title: {title}\n\
Brief: {brief}\n\n\
Return ONLY a JSON array, no markdown or explanation:\n\
[{{\"id\":\"A1\",\"text\":\"<specific verifiable statement>\"}},...]"
    );
    let raw = match direct_chat(&prompt, model, base_url, 120).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[negotiate] direct_chat({model}) failed: {e}");
            return None;
        }
    };
    let parsed = parse_assertion_array(&raw);
    if parsed.is_none() {
        eprintln!("[negotiate] direct_chat returned but parse_assertion_array failed. \
                   Raw response (first 300 chars): {}",
                  raw.chars().take(300).collect::<String>());
    }
    parsed
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
For every constraint in the brief, check that an assertion verifies the constraint \
directly. A proxy check (file unchanged, compile succeeded, output exists) is not \
enough when the brief restricts the *content* of a modification — that requires an \
assertion that examines the modification itself. If a constraint is unverified, add \
or rewrite an assertion to cover it.\n\n\
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
    let mut seen_texts = std::collections::HashSet::new();
    let mut result: Vec<(String, String)> = Vec::new();
    for (id, text) in a.into_iter().chain(b) {
        // Semantic dedup: if another assertion already says the same thing
        // (after lowercasing + stripping punctuation + collapsing whitespace),
        // drop this one. Two-model negotiation routinely produces verbatim or
        // near-verbatim duplicates ("pdflatex compiles successfully" appears
        // in both sets); without this we ended up with 12 assertions where 4
        // were exact duplicates and 4 more were semantic overlaps.
        let key = normalize_for_dedup(&text);
        if !key.is_empty() && !seen_texts.insert(key) {
            continue;
        }
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

/// Normalize an assertion text for deduplication: lowercase, drop non-
/// alphanumeric characters, collapse runs of whitespace. Catches verbatim
/// duplicates and most punctuation-only / phrasing-trivial variations.
/// Two assertions that hash to the same key are treated as the same
/// constraint.
fn normalize_for_dedup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            prev_space = false;
        } else if c.is_whitespace() || c.is_ascii_punctuation() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        }
        // Non-ASCII non-alphanumeric symbols (emoji, etc.) are dropped silently.
    }
    out.trim().to_string()
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

pub async fn direct_chat(
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
    // Reasoning-enabled models (Qwen3, Gemma3+, DeepSeek-R1, ...) spend
    // `max_tokens` on reasoning_content first, then on output. With
    // max_tokens=1024 the entire budget gets eaten by thinking and the
    // content field comes back empty — that's the bug we hit when the
    // assertion-negotiation second-opinion call always returned "".
    //
    // Allocate generous headroom: ~2048 for thinking, ~4096 for the
    // actual JSON response. Match the explicit chat_template_kwargs +
    // thinking_budget shape the main client uses so llama-server-backed
    // reasoning models pick it up correctly.
    let thinking_budget = 2048;
    let max_tokens = thinking_budget + 4096;
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.2,
        "max_tokens": max_tokens,
        "enable_thinking": true,
        "thinking_budget": thinking_budget,
        "chat_template_kwargs": {
            "enable_thinking": true,
            "thinking_budget": thinking_budget,
        },
        "thinking": {"type": "enabled", "budget_tokens": thinking_budget},
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_fences ────────────────────────────────────────────────────────

    #[test]
    fn strip_fences_handles_json_fence() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_handles_bare_fence() {
        assert_eq!(strip_fences("```\ndata\n```"), "data");
    }

    #[test]
    fn strip_fences_passes_through_unfenced() {
        assert_eq!(strip_fences("plain text"), "plain text");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_handles_empty() {
        assert_eq!(strip_fences(""), "");
        assert_eq!(strip_fences("   "), "");
    }

    // ── truncate ────────────────────────────────────────────────────────────

    #[test]
    fn truncate_short_strings_pass_through() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_strings_cap_at_n() {
        let s = "x".repeat(100);
        assert_eq!(truncate(&s, 20).len(), 20);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // 'é' is 2 bytes; cutting at 5 in "héllo" needs to land on a char boundary.
        let r = truncate("héllo wörld", 5);
        // Must not panic, and must yield valid UTF-8.
        assert!(r.len() <= 5);
        assert!(r.chars().count() > 0);
    }

    // ── merge_assertions ────────────────────────────────────────────────────

    #[test]
    fn merge_assertions_concatenates_unique_ids() {
        let a = vec![("A.001".into(), "first".into())];
        let b = vec![("A.002".into(), "second".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, "A.001");
        assert_eq!(merged[1].0, "A.002");
    }

    /// Conflicting IDs are renumbered (`A.001` → `A.001_2`) — anti-regression
    /// for silent data loss when two assertion sets share IDs.
    #[test]
    fn merge_assertions_renames_collisions() {
        let a = vec![("A.001".into(), "from a".into())];
        let b = vec![("A.001".into(), "from b".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2, "no assertion may be silently dropped");
        assert_eq!(merged[0].0, "A.001");
        assert_eq!(merged[1].0, "A.001_2",
            "duplicate ID must be renumbered, not collapsed");
        assert_eq!(merged[0].1, "from a");
        assert_eq!(merged[1].1, "from b");
    }

    /// Multiple collisions of the same id keep increasing the suffix.
    #[test]
    fn merge_assertions_handles_triple_collision() {
        let a = vec![("X".into(), "a".into()), ("X".into(), "b".into())];
        let b = vec![("X".into(), "c".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 3);
        let ids: Vec<&str> = merged.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["X", "X_2", "X_3"]);
    }

    /// Empty inputs handled gracefully.
    #[test]
    fn merge_assertions_empty_inputs() {
        assert!(merge_assertions(vec![], vec![]).is_empty());
        let a = vec![("A.001".into(), "x".into())];
        assert_eq!(merge_assertions(a.clone(), vec![]), a);
        assert_eq!(merge_assertions(vec![], a.clone()), a);
    }

    /// Semantic dedup: identical text under different ids collapses to one.
    /// Anti-regression for the case where two-model negotiation produced 12
    /// assertions because every Qwen assertion appeared again verbatim from
    /// Gemma under a renumbered id (`A1` + `A1_2`).
    #[test]
    fn merge_assertions_dedupes_identical_text() {
        let a = vec![
            ("A1".into(), "pdflatex compiles main.tex successfully".into()),
            ("A2".into(), "no overfull hbox warnings".into()),
        ];
        let b = vec![
            ("B1".into(), "pdflatex compiles main.tex successfully".into()),
            ("B2".into(), "synonyms.txt is unchanged".into()),
        ];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 3, "duplicate text must collapse");
        let texts: Vec<&str> = merged.iter().map(|(_, t)| t.as_str()).collect();
        assert!(texts.contains(&"pdflatex compiles main.tex successfully"));
        assert!(texts.contains(&"no overfull hbox warnings"));
        assert!(texts.contains(&"synonyms.txt is unchanged"));
    }

    /// Punctuation / case differences should also be treated as duplicates.
    #[test]
    fn merge_assertions_dedupes_phrasing_variants() {
        let a = vec![("A1".into(), "pdflatex compiles main.tex successfully (exit code 0)".into())];
        let b = vec![("B1".into(), "pdflatex   compiles main.tex successfully, exit code 0.".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 1,
            "casing/punctuation/whitespace differences must not bypass dedup");
    }

    /// Different texts with the same id still both ship (with renumbering).
    /// Keeps the existing `merge_assertions_renames_collisions` behaviour
    /// intact when the texts are actually distinct.
    #[test]
    fn merge_assertions_preserves_distinct_texts_under_same_id() {
        let a = vec![("A.001".into(), "first thing".into())];
        let b = vec![("A.001".into(), "totally different thing".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2, "different text must not be deduped");
    }

    // ── normalize_for_dedup ────────────────────────────────────────────────

    #[test]
    fn normalize_strips_case_and_punctuation() {
        assert_eq!(normalize_for_dedup("Hello, World!"), "hello world");
        assert_eq!(normalize_for_dedup("  HELLO   world  "), "hello world");
        assert_eq!(normalize_for_dedup("foo.bar:baz"), "foo bar baz");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_for_dedup("a\n\tb  c"), "a b c");
    }

    #[test]
    fn normalize_empty_is_empty() {
        assert_eq!(normalize_for_dedup(""), "");
        assert_eq!(normalize_for_dedup("...  ,, !!"), "");
    }

    // ── parse_assertion_array ──────────────────────────────────────────────

    #[test]
    fn parse_assertions_valid_array() {
        let raw = r#"[{"id":"A.001","text":"hello world"},{"id":"A.002","text":"goodbye"}]"#;
        let r = parse_assertion_array(raw).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("A.001".into(), "hello world".into()));
        assert_eq!(r[1], ("A.002".into(), "goodbye".into()));
    }

    /// Parse strips fences first.
    #[test]
    fn parse_assertions_strips_fences_first() {
        let raw = "```json\n[{\"id\":\"A.001\",\"text\":\"hello world\"}]\n```";
        let r = parse_assertion_array(raw).unwrap();
        assert_eq!(r.len(), 1);
    }

    /// Missing fields → assertion dropped.
    #[test]
    fn parse_assertions_drops_invalid_items() {
        let raw = r#"[
            {"id":"A.001","text":"valid one"},
            {"id":"","text":"empty id"},
            {"text":"missing id"},
            {"id":"A.002","text":"x"}
        ]"#;
        let r = parse_assertion_array(raw).unwrap();
        // A.001 stays; empty id, missing id dropped; A.002 text too short (3 chars).
        assert!(r.iter().any(|(id, _)| id == "A.001"));
        assert!(!r.iter().any(|(id, _)| id.is_empty()));
        assert!(!r.iter().any(|(id, _)| id == "A.002"),
            "text shorter than 5 chars must be dropped; got {r:?}");
    }

    /// Non-array / malformed JSON returns None.
    #[test]
    fn parse_assertions_rejects_non_array() {
        assert!(parse_assertion_array("not json").is_none());
        assert!(parse_assertion_array("{\"obj\":true}").is_none());
        assert!(parse_assertion_array("[]").is_none(), "empty array → None");
    }
}
