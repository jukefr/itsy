//! Cognition layer — repair prompts.
//!
//! One repair function per prompt that declares
//! `on_invalid: retry_with_repair_prompt`. Repair calls are single-shot
//! (max_attempts: 1) and use a smaller, more constrained message than the
//! original prompt.
//!
//! Also exposes [`repair_json`], a heuristic that coerces small-model JSON
//! output (fenced code blocks, single-quoted keys, trailing commas) into
//! parseable JSON.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON output repair heuristics
// ---------------------------------------------------------------------------

/// Attempt to repair common JSON serialisation errors emitted by small models.
/// Returns the original input unchanged if no repair is applicable.
pub fn repair_json(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return input.to_string();
    }

    // Strip ```json or ``` fences.
    let mut s = trimmed.to_string();
    if let Some(rest) = s.strip_prefix("```json") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("```") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_suffix("```") {
        s = rest.to_string();
    }
    let s = s.trim().to_string();

    // Quote unquoted keys (common small-model bug).
    let s = replace_unquoted_keys(&s);
    // Replace single-quoted strings/keys with double-quoted equivalents.
    let s = replace_single_quotes(&s);
    // Drop trailing commas before `}` / `]`.
    let s = strip_trailing_commas(&s);

    // If it now parses, re-serialise into a canonical form.
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        return serde_json::to_string(&v).unwrap_or(s);
    }
    s
}

fn replace_unquoted_keys(input: &str) -> String {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"([\{,]\s*)([A-Za-z_][A-Za-z0-9_]*)\s*:"#).expect("valid regex literal"));
    RE.replace_all(input, "$1\"$2\":").into_owned()
}

fn replace_single_quotes(input: &str) -> String {
    // Best-effort: only convert single-quoted *values* whose content has no
    // embedded single or double quotes. This avoids breaking apostrophes
    // inside legitimate double-quoted strings.
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"'([^'"\\]*)'"#).expect("valid regex literal"));
    RE.replace_all(input, "\"$1\"").into_owned()
}

fn strip_trailing_commas(input: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#",(\s*[}\]])"#).expect("valid regex literal"));
    RE.replace_all(input, "$1").into_owned()
}

// ---------------------------------------------------------------------------
// Cognition repair message builder
// ---------------------------------------------------------------------------

/// Options describing a failed prompt invocation that needs a repair attempt.
#[derive(Debug, Clone)]
pub struct RepairOpts<'a> {
    pub prompt_name: &'a str,
    pub original_input: &'a Value,
    pub bad_output: &'a Value,
    pub issues: &'a [String],
    pub output_type: &'a str,
}

/// Build the user-facing repair prompt for a failed prompt invocation.
/// Mirrors `buildRepairMessage` in the JS source.
pub fn build_repair_message(opts: &RepairOpts<'_>) -> String {
    let input_json = serde_json::to_string_pretty(opts.original_input)
        .unwrap_or_default();
    let input_json = truncate(&input_json, 1500);

    let bad_json = if let Some(s) = opts.bad_output.as_str() {
        truncate(s, 1500)
    } else {
        truncate(
            &serde_json::to_string_pretty(opts.bad_output).unwrap_or_default(),
            1500,
        )
    };

    let issues_list = if opts.issues.is_empty() {
        "output failed validation (no specific issues reported)".to_string()
    } else {
        opts.issues
            .iter()
            .take(5)
            .enumerate()
            .map(|(n, i)| format!("{}. {}", n + 1, truncate(i, 200)))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Pattern-matched targeted guidance for the most common failure shapes.
    let issues_text = opts.issues.join(" ");
    let mut guidance: Vec<String> = Vec::new();

    static UNTERMINATED_TPL: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)Unterminated template literal").expect("valid regex literal"));
    static CANT_FIND_MODULE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)Cannot find module").expect("valid regex literal"));
    static CANT_FIND_NAME: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)Cannot find name").expect("valid regex literal"));

    if UNTERMINATED_TPL.is_match(&issues_text) {
        guidance.push(
            "Specific guidance: an unterminated template literal means a backtick \
             (`) somewhere in your file isn't matched. The most common cause is \
             emitting a TypeScript template literal that contains markdown code \
             fences (which use backticks). Rewrite the file to NOT use backtick \
             strings at all. Use single-quoted strings, double-quoted strings, \
             or arrays of strings joined with '\\n'. If you need string \
             interpolation, use the '+' operator. This avoids the problem entirely."
                .into(),
        );
    }
    if CANT_FIND_MODULE.is_match(&issues_text) {
        guidance.push(
            "Specific guidance: one or more imports reference a package that \
             doesn't exist. Either swap to a real package name (verify it actually \
             exists on npm) or drop the import and reimplement using only Node.js \
             builtins (fs, path, crypto, child_process, http, https, os, url, \
             events, stream)."
                .into(),
        );
    }
    if CANT_FIND_NAME.is_match(&issues_text) {
        guidance.push(
            "Specific guidance: you referenced a type or value that's never \
             declared. Add a real `interface` or `type` declaration for any type \
             you reference. JSDoc-only types do NOT count — they must be real TS \
             declarations."
                .into(),
        );
    }

    let mut lines: Vec<String> = vec![
        "You produced an output that failed validation. Fix it. Reply ONLY with the corrected output.".into(),
        format!("Expected output type: {}", opts.output_type),
        String::new(),
        "Original input:".into(),
        input_json,
        String::new(),
        "Your previous output:".into(),
        bad_json,
        String::new(),
        "Validation issues:".into(),
        issues_list,
    ];
    if !guidance.is_empty() {
        lines.push(String::new());
        lines.extend(guidance);
    }
    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Truncate by char boundary to be safe with multi-byte UTF-8.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

/// Outcome of a single repair attempt.
#[derive(Debug, Clone)]
pub struct RepairOutcome {
    pub ok: bool,
    pub value: Option<String>,
    pub tokens_used: u64,
}

impl RepairOutcome {
    pub fn failure() -> Self {
        Self { ok: false, value: None, tokens_used: 0 }
    }
}

/// Synchronous repair shim: builds the repair message for `code_assist`.
///
/// The JS version makes a real network call. In the Rust port the actual
/// provider call lives in the cognition layer; this helper exists so callers
/// that want to construct the repair prompt without performing the request can
/// still hit the same code path.
pub fn repair_code_assist_message(
    original_input: &Value,
    bad_output: &Value,
    issues: &[String],
) -> String {
    build_repair_message(&RepairOpts {
        prompt_name: "code_assist",
        original_input,
        bad_output,
        issues,
        output_type: "string",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── repair_json ────────────────────────────────────────────────────────

    /// Strips ```json fences and re-serialises canonically.
    #[test]
    fn repair_strips_json_fence() {
        let s = "```json\n{\"a\":1}\n```";
        let out = repair_json(s);
        let v: Value = serde_json::from_str(&out).expect("output must parse");
        assert_eq!(v, json!({"a": 1}));
    }

    /// Strips bare ``` fences too.
    #[test]
    fn repair_strips_bare_fence() {
        let s = "```\n{\"x\":42}\n```";
        let out = repair_json(s);
        let v: Value = serde_json::from_str(&out).expect("output must parse");
        assert_eq!(v, json!({"x": 42}));
    }

    /// Trailing commas before `}` are dropped.
    #[test]
    fn repair_strips_trailing_commas() {
        let s = "{\"a\": 1, \"b\": 2,}";
        let out = repair_json(s);
        let v: Value = serde_json::from_str(&out).expect("output must parse");
        assert_eq!(v, json!({"a": 1, "b": 2}));
    }

    /// Unquoted keys are quoted, then parsed.
    #[test]
    fn repair_quotes_unquoted_keys() {
        let s = "{a: 1, b: \"text\"}";
        let out = repair_json(s);
        let v: Value = serde_json::from_str(&out).expect("output must parse");
        assert_eq!(v, json!({"a": 1, "b": "text"}));
    }

    /// Already-valid JSON survives a roundtrip (no false repairs).
    #[test]
    fn repair_is_idempotent_on_valid_json() {
        let s = "{\"a\":1,\"b\":[1,2,3]}";
        let out = repair_json(s);
        let v: Value = serde_json::from_str(&out).expect("output must parse");
        assert_eq!(v, json!({"a": 1, "b": [1, 2, 3]}));
    }

    /// Empty / whitespace-only input is returned unchanged (no panic).
    #[test]
    fn repair_handles_empty_input() {
        assert_eq!(repair_json(""), "");
        assert_eq!(repair_json("   "), "   ");
    }

    // ── build_repair_message ────────────────────────────────────────────────

    fn opts<'a>(issues: &'a [String]) -> RepairOpts<'a> {
        static INPUT: Lazy<Value> = Lazy::new(|| json!({"task": "do thing"}));
        static BAD: Lazy<Value> = Lazy::new(|| json!("garbage output"));
        RepairOpts {
            prompt_name: "test",
            original_input: &INPUT,
            bad_output: &BAD,
            issues,
            output_type: "json",
        }
    }

    /// Repair message includes original input, bad output, and issues.
    #[test]
    fn repair_message_includes_all_sections() {
        let issues = vec!["missing required field 'name'".to_string()];
        let msg = build_repair_message(&opts(&issues));
        assert!(msg.contains("Expected output type: json"));
        assert!(msg.contains("Original input:"));
        assert!(msg.contains("Your previous output:"));
        assert!(msg.contains("Validation issues:"));
        assert!(msg.contains("missing required field"));
    }

    /// Empty issues list falls back to a generic fallback message.
    #[test]
    fn repair_message_handles_no_issues() {
        let issues: Vec<String> = vec![];
        let msg = build_repair_message(&opts(&issues));
        assert!(msg.contains("output failed validation"));
    }

    /// Long issue lists are capped at 5 entries (anti-bloat regression).
    #[test]
    fn repair_message_caps_issue_list_at_five() {
        let issues: Vec<String> = (0..20).map(|i| format!("issue {i}")).collect();
        let msg = build_repair_message(&opts(&issues));
        assert!(msg.contains("issue 0"));
        assert!(msg.contains("issue 4"));
        assert!(!msg.contains("issue 7"),
            "issues 5+ must be dropped, msg was:\n{msg}");
    }

    /// Specific guidance for "Unterminated template literal" appears in output.
    #[test]
    fn repair_message_includes_template_literal_guidance() {
        let issues = vec!["TypeError: Unterminated template literal at line 42".to_string()];
        let msg = build_repair_message(&opts(&issues));
        assert!(msg.contains("backtick"),
            "template-literal guidance must mention backticks; got:\n{msg}");
    }

    /// Specific guidance for "Cannot find module" appears.
    #[test]
    fn repair_message_includes_missing_module_guidance() {
        let issues = vec!["Error: Cannot find module 'frobnicate'".to_string()];
        let msg = build_repair_message(&opts(&issues));
        assert!(msg.contains("doesn't exist") || msg.contains("real package"),
            "missing-module guidance expected; got:\n{msg}");
    }

    /// `repair_code_assist_message` is a thin shim — produces same shape.
    #[test]
    fn code_assist_shim_produces_string_output_type() {
        let input = json!({"q": "x"});
        let bad = json!("bad");
        let issues = vec!["err".to_string()];
        let msg = repair_code_assist_message(&input, &bad, &issues);
        assert!(msg.contains("Expected output type: string"),
            "shim must use string output_type");
    }
}
