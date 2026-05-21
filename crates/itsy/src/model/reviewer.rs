//! Reviewer agent (port of `src/model/reviewer.js`).
//!
//! A second model call that critiques the executor's most recent assistant
//! message before it's acted upon. Catches obvious issues the writer missed:
//!
//!   - Missing error handling in generated code
//!   - Wrong function/variable names vs the task
//!   - Incomplete implementations ("TODO" left in)
//!   - Contradicting the user's requirements
//!
//! Runs async and non-blocking — the executor keeps going while the reviewer
//! thinks. The reviewer result is injected as a follow-up system note if it
//! finds a real problem; if no problem, silent.
//!
//! Designed to be cheap: uses the same model or a smaller one, with a very
//! short prompt (task + response summary) and tiny token budget (256 tokens).
//!
//! Configuration:
//!   ITSY_REVIEWER=true              enable reviewer
//!   ITSY_REVIEWER_MODEL=<name>      reviewer model (defaults to main model)
//!   ITSY_REVIEWER_URL=<url>         reviewer endpoint (defaults to main)
//!   ITSY_REVIEWER_THRESHOLD=0.7     confidence threshold to inject (0-1)
//!   ITSY_REVIEWER_MAX_TOKENS=256    reviewer response token cap

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;

fn env_some(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn threshold() -> f64 {
    env_f64("ITSY_REVIEWER_THRESHOLD", 0.7)
}

fn max_tokens() -> u32 {
    env_u32("ITSY_REVIEWER_MAX_TOKENS", 256)
}

#[derive(Debug, Clone)]
pub struct ReviewerConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub base_url: String,
}

static REVIEWER_CONFIG: OnceLock<ReviewerConfig> = OnceLock::new();

/// Return the (cached) reviewer configuration derived from environment and
/// the main agent config (for defaults).
pub fn get_reviewer_config(main_config: &Config) -> &'static ReviewerConfig {
    REVIEWER_CONFIG.get_or_init(|| ReviewerConfig {
        enabled: env::var("ITSY_REVIEWER").ok().as_deref() == Some("true"),
        model: env_some("ITSY_REVIEWER_MODEL").or_else(|| {
            if main_config.model.name.is_empty() {
                None
            } else {
                Some(main_config.model.name.clone())
            }
        }),
        base_url: env_some("ITSY_REVIEWER_URL")
            .or_else(|| {
                if main_config.model.base_url.is_empty() {
                    None
                } else {
                    Some(main_config.model.base_url.clone())
                }
            })
            .unwrap_or_else(|| "http://localhost:1234/v1".into()),
    })
}

/// Structured reviewer verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub ok: bool,
    pub issues: Vec<String>,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

fn build_reviewer_headers(config: &Config) -> HeaderMap {
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

static LGTM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^lgtm\.?$").expect("lgtm regex")
});
static LOOKS_GOOD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)looks? (good|correct|fine)").expect("looks-good regex")
});
static NO_ISSUE_PREFIX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(lgtm|looks? good|no issues)").expect("no-issue regex")
});
static SPLIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\n•\-]+").expect("split regex")
});

fn parse_reviewer_output(content: &str) -> ReviewResult {
    let trimmed = content.trim();
    if LGTM_RE.is_match(trimmed) || LOOKS_GOOD_RE.is_match(trimmed) {
        return ReviewResult { ok: true, issues: vec![], confidence: 0.9, raw: None };
    }
    let issues: Vec<String> = SPLIT_RE
        .split(trimmed)
        .map(|l| l.trim().to_string())
        .filter(|l| l.len() > 10 && !NO_ISSUE_PREFIX_RE.is_match(l))
        .collect();
    if issues.is_empty() {
        return ReviewResult { ok: true, issues: vec![], confidence: 0.8, raw: None };
    }
    ReviewResult {
        ok: false,
        issues,
        confidence: 0.8,
        raw: Some(trimmed.to_string()),
    }
}

/// Async reviewer call. Returns `Some(ReviewResult)` if the reviewer model
/// produced a usable verdict; `None` if reviewer is disabled, the response
/// is too short, or the model is unreachable.
pub async fn review_response(
    task: &str,
    response: &str,
    edited_files: &[String],
    main_config: &Config,
) -> Option<ReviewResult> {
    let cfg = get_reviewer_config(main_config);
    if !cfg.enabled {
        return None;
    }
    let model = cfg.model.as_ref()?;
    if response.len() < 50 {
        return None;
    }

    let files_note = if edited_files.is_empty() {
        String::new()
    } else {
        let head: Vec<&str> =
            edited_files.iter().take(5).map(|s| s.as_str()).collect();
        format!("Files modified: {}.", head.join(", "))
    };

    let task_head: String = task.chars().take(300).collect();
    let resp_head: String = response.chars().take(500).collect();
    let review_prompt = format!(
        "You are a code reviewer. Given a task and an AI assistant's response, identify ONLY critical issues (missing error handling, wrong logic, incomplete implementation, contradicts requirements). Be terse. If the response looks correct, say \"LGTM\".\n\nTask: {task_head}\n{files_note}\nResponse summary: {resp_head}\n\nCritical issues (or \"LGTM\"):"
    );

    let body = json!({
        "model": model,
        "temperature": 0.1,
        "max_tokens": max_tokens(),
        "messages": [
            { "role": "user", "content": review_prompt },
        ],
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;

    let url = format!("{}/chat/completions", cfg.base_url);
    let resp = client
        .post(&url)
        .headers(build_reviewer_headers(main_config))
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
        .unwrap_or("")
        .trim()
        .to_string();
    if content.len() < 3 {
        return None;
    }
    Some(parse_reviewer_output(&content))
}

/// Format a reviewer result for injection into conversation history.
/// Returns an empty string if no injection is warranted (no result, ok,
/// below threshold, or no issues).
pub fn format_reviewer_injection(result: Option<&ReviewResult>) -> String {
    let r = match result {
        None => return String::new(),
        Some(r) => r,
    };
    if r.ok {
        return String::new();
    }
    if r.confidence < threshold() {
        return String::new();
    }
    if r.issues.is_empty() {
        return String::new();
    }
    let top: Vec<String> = r.issues.iter().take(2).map(|i| format!("- {i}")).collect();
    format!(
        "[REVIEWER] Potential issues in the response above:\n{}\n\nAddress these before finalizing.",
        top.join("\n")
    )
}

// ─── Local heuristic review (no network) ──────────────────────────────────
//
// Cheap pre-checks that catch obvious garbage before we bother spinning up a
// reviewer model call. These are pure functions over the assistant's output.

/// Heuristic pattern check used by `quick_review`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSignal {
    Empty,
    Nonsense,
    Hallucination,
    Contradiction,
    BadJson,
    LowConfidence,
    PoorReasoning,
}

impl ReviewSignal {
    pub fn as_reason(&self) -> &'static str {
        match self {
            ReviewSignal::Empty => "empty response",
            ReviewSignal::Nonsense => "nonsense response",
            ReviewSignal::Hallucination => "possible hallucination pattern",
            ReviewSignal::Contradiction => "contradictory statements",
            ReviewSignal::BadJson => "expected JSON but parse failed",
            ReviewSignal::LowConfidence => "model expressed low confidence",
            ReviewSignal::PoorReasoning => "shallow or empty reasoning",
        }
    }
}

static HALLUCINATION_RE: Lazy<Regex> = Lazy::new(|| {
    // Common admissions-as-fabrication patterns. Case-insensitive.
    Regex::new(
        r"(?i)(as an ai language model|i (cannot|can't) actually|i (don't|do not) have access to|i (made|am making) (this|that) up|i (don't|do not) know but)",
    )
    .expect("hallucination regex")
});

static CONTRADICTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(however|but|actually|wait|on second thought|correction|i was wrong)\b",
    )
    .expect("contradiction regex")
});

static LOW_CONF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(i('m| am) not sure|maybe|might be|could be|i think|possibly|perhaps|i guess)\b",
    )
    .expect("low-confidence regex")
});

static POOR_REASONING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(just because|obviously|clearly|trivially|trust me)\b",
    )
    .expect("poor-reasoning regex")
});

/// Lightweight, offline review that flags obvious problems in `content`.
///
/// When `expect_json` is true, also validates the content parses as JSON.
/// Returns the first failing signal — callers that want all signals should
/// use [`review_signals`].
pub fn quick_review(content: &str) -> ReviewResult {
    review_with(content, false)
}

/// Same as [`quick_review`] but enforces JSON-shape validation when
/// `expect_json` is true.
pub fn review_with(content: &str, expect_json: bool) -> ReviewResult {
    let signals = review_signals(content, expect_json);
    if signals.is_empty() {
        return ReviewResult { ok: true, issues: vec![], confidence: 0.9, raw: None };
    }
    let confidence = score_confidence(&signals);
    let issues: Vec<String> = signals.iter().map(|s| s.as_reason().to_string()).collect();
    ReviewResult { ok: false, issues, confidence, raw: None }
}

/// Run every offline check and return all matching signals.
pub fn review_signals(content: &str, expect_json: bool) -> Vec<ReviewSignal> {
    let mut out = Vec::new();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        out.push(ReviewSignal::Empty);
        return out;
    }
    if trimmed.len() < 8 && !trimmed.chars().any(|c| c.is_alphabetic()) {
        out.push(ReviewSignal::Nonsense);
    }
    if HALLUCINATION_RE.is_match(trimmed) {
        out.push(ReviewSignal::Hallucination);
    }
    if has_contradiction(trimmed) {
        out.push(ReviewSignal::Contradiction);
    }
    if LOW_CONF_RE.is_match(trimmed) {
        out.push(ReviewSignal::LowConfidence);
    }
    if has_poor_reasoning(trimmed) {
        out.push(ReviewSignal::PoorReasoning);
    }
    if expect_json && !looks_like_valid_json(trimmed) {
        out.push(ReviewSignal::BadJson);
    }
    out
}

/// Two or more contradiction tokens in the same response is suspicious; a
/// single "however" on its own is not.
fn has_contradiction(s: &str) -> bool {
    CONTRADICTION_RE.find_iter(s).count() >= 2
}

/// Poor reasoning is only flagged if at least one marker appears AND the
/// response is short (< 200 chars) — i.e. the model leaned on hand-waving
/// instead of giving substance.
fn has_poor_reasoning(s: &str) -> bool {
    POOR_REASONING_RE.is_match(s) && s.len() < 200
}

fn looks_like_valid_json(s: &str) -> bool {
    // Allow surrounding ```json fences from chatty models.
    let trimmed = s
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str::<Value>(trimmed).is_ok()
}

/// Map a set of signals to a confidence score in [0.0, 1.0]. More signals
/// lower the confidence; some signals weigh more than others.
fn score_confidence(signals: &[ReviewSignal]) -> f64 {
    let mut score: f64 = 0.95;
    for s in signals {
        let penalty = match s {
            ReviewSignal::Empty | ReviewSignal::Nonsense | ReviewSignal::BadJson => 0.5,
            ReviewSignal::Hallucination => 0.35,
            ReviewSignal::Contradiction => 0.20,
            ReviewSignal::LowConfidence => 0.15,
            ReviewSignal::PoorReasoning => 0.10,
        };
        score -= penalty;
    }
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_response_fails() {
        let r = quick_review("");
        assert!(!r.ok);
        assert!(r.issues.iter().any(|i| i.contains("empty")));
    }

    #[test]
    fn good_response_passes() {
        let r = quick_review("Here is the function you asked for. It returns the sum of two numbers.");
        assert!(r.ok);
    }

    #[test]
    fn hallucination_pattern_caught() {
        let r = quick_review("As an AI language model, I cannot run code, but I think this might work.");
        assert!(!r.ok);
    }

    #[test]
    fn contradiction_only_flags_with_multiple_markers() {
        let single = "Run npm install. However, you may need sudo.";
        assert!(quick_review(single).ok);
        let multi = "Use npm. However, actually, on second thought, use yarn.";
        assert!(!quick_review(multi).ok);
    }

    #[test]
    fn json_expected_but_garbage() {
        let r = review_with("not json at all", true);
        assert!(!r.ok);
        assert!(r.issues.iter().any(|i| i.contains("JSON")));
    }

    #[test]
    fn json_expected_and_valid_passes() {
        let r = review_with(r#"{"answer": 42}"#, true);
        assert!(r.ok);
    }

    #[test]
    fn parse_reviewer_output_lgtm() {
        let r = parse_reviewer_output("LGTM");
        assert!(r.ok);
        assert_eq!(r.confidence, 0.9);
    }

    #[test]
    fn parse_reviewer_output_issues() {
        let r = parse_reviewer_output("- missing error handling\n- variable name wrong");
        assert!(!r.ok);
        assert_eq!(r.issues.len(), 2);
    }

    #[test]
    fn format_injection_respects_threshold() {
        let low = ReviewResult { ok: false, issues: vec!["x".into()], confidence: 0.1, raw: None };
        assert_eq!(format_reviewer_injection(Some(&low)), "");
    }
}
