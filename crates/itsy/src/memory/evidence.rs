//! Evidence ledger — tracks files, snippets, commands, and observations the
//! agent has gathered during a turn so it can cite or revisit them later.
//!
//! Port of `src/memory/evidence.js` from the upstream SmallCode JS repo, with
//! the in-turn `EvidenceLog` ledger kept and the trace-summariser machinery
//! (`summarize_trace`, helpers) added for parity.
//!
//! Two distinct concerns live in this module:
//!
//! 1. [`EvidenceLog`] — live, in-turn ledger of artifacts the agent has seen
//!    (files read, snippets extracted, commands run, search hits, errors,
//!    free-form notes). Drives the "citations" feature so model outputs can
//!    reference real artifacts rather than hallucinate. Supports recency +
//!    relevance scoring when too many entries accumulate, plus JSON
//!    round-tripping for session persistence.
//!
//! 2. [`summarize_trace`] / [`record_evidence`] — automated capture of
//!    "what was tried, what worked, what failed" per task. Distinct from
//!    manual memory (decisions / conventions): evidence is auto-derived from a
//!    trace at task end into a 1–3 KB digest suitable for re-injection into
//!    future context. We deliberately do NOT store full trace contents — those
//!    are 5–50 KB each.

use std::collections::BTreeSet;

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ─── EvidenceLog (in-turn ledger) ──────────────────────────────────────────

/// Kind of evidence captured. Mirrors the JS `type: 'context'` taxonomy but
/// keeps each variant distinct for type-safe matching downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A file the agent read, optionally with a line range.
    File,
    /// An extracted code block / excerpt.
    Snippet,
    /// Output (stdout/stderr) from a shell command.
    CommandOutput,
    /// A search hit (ripgrep / glob / grep).
    SearchResult,
    /// An error message worth remembering.
    Error,
    /// A free-form observation.
    Note,
}

impl EvidenceKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Snippet => "snippet",
            Self::CommandOutput => "command",
            Self::SearchResult => "search",
            Self::Error => "error",
            Self::Note => "note",
        }
    }
}

/// A single piece of evidence collected during a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub kind: EvidenceKind,
    /// File / target path, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional `(start, end)` line range, 1-based and inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<(u32, u32)>,
    /// Body content (file slice, snippet, command output, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Short human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Unix-epoch milliseconds when added (used for recency ranking).
    pub timestamp_ms: i64,
}

impl EvidenceEntry {
    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn new(kind: EvidenceKind) -> Self {
        Self {
            kind,
            path: None,
            line_range: None,
            snippet: None,
            note: None,
            timestamp_ms: Self::now_ms(),
        }
    }
}

/// In-turn evidence ledger.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceLog {
    pub entries: Vec<EvidenceEntry>,
}

impl EvidenceLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a file read. `line_range` is `(start, end)`, 1-based inclusive.
    pub fn add_file(
        &mut self,
        path: impl Into<String>,
        line_range: Option<(u32, u32)>,
        note: impl Into<String>,
    ) {
        let note = note.into();
        self.entries.push(EvidenceEntry {
            path: Some(path.into()),
            line_range,
            note: if note.is_empty() { None } else { Some(note) },
            ..EvidenceEntry::new(EvidenceKind::File)
        });
    }

    /// Record an extracted snippet.
    pub fn add_snippet(&mut self, path: impl Into<String>, snippet: impl Into<String>) {
        self.entries.push(EvidenceEntry {
            path: Some(path.into()),
            snippet: Some(snippet.into()),
            ..EvidenceEntry::new(EvidenceKind::Snippet)
        });
    }

    /// Record the result of a bash / shell command.
    pub fn add_command_output(
        &mut self,
        command: impl Into<String>,
        output: impl Into<String>,
    ) {
        self.entries.push(EvidenceEntry {
            note: Some(command.into()),
            snippet: Some(output.into()),
            ..EvidenceEntry::new(EvidenceKind::CommandOutput)
        });
    }

    /// Record a search result (e.g. grep / ripgrep hit).
    pub fn add_search_result(
        &mut self,
        query: impl Into<String>,
        path: impl Into<String>,
        snippet: impl Into<String>,
    ) {
        self.entries.push(EvidenceEntry {
            note: Some(query.into()),
            path: Some(path.into()),
            snippet: Some(snippet.into()),
            ..EvidenceEntry::new(EvidenceKind::SearchResult)
        });
    }

    /// Record an error worth remembering.
    pub fn add_error(&mut self, message: impl Into<String>) {
        self.entries.push(EvidenceEntry {
            note: Some(message.into()),
            ..EvidenceEntry::new(EvidenceKind::Error)
        });
    }

    /// Record a free-form note.
    pub fn add_note(&mut self, note: impl Into<String>) {
        self.entries.push(EvidenceEntry {
            note: Some(note.into()),
            ..EvidenceEntry::new(EvidenceKind::Note)
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// JSON round-trip helpers for session persistence.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Score an entry for ranking. Higher = more useful to keep.
    ///
    /// Combines:
    ///   * **recency** — newer entries beat older ones (linear decay over the
    ///     log's span);
    ///   * **kind weight** — errors / files / search hits are more actionable
    ///     than free-form notes;
    ///   * **relevance** — if `query` is non-empty, substring matches on path
    ///     / note / snippet add a boost.
    fn score(&self, idx: usize, query: Option<&str>) -> f64 {
        let entry = &self.entries[idx];

        // Recency: most-recent entry gets 1.0, oldest gets 0.0.
        let (oldest, newest) = self
            .entries
            .iter()
            .fold((i64::MAX, i64::MIN), |(lo, hi), e| {
                (lo.min(e.timestamp_ms), hi.max(e.timestamp_ms))
            });
        let recency = if newest == oldest {
            1.0
        } else {
            (entry.timestamp_ms - oldest) as f64 / (newest - oldest) as f64
        };

        let kind_weight = match entry.kind {
            EvidenceKind::Error => 1.0,
            EvidenceKind::File => 0.9,
            EvidenceKind::SearchResult => 0.8,
            EvidenceKind::CommandOutput => 0.7,
            EvidenceKind::Snippet => 0.6,
            EvidenceKind::Note => 0.4,
        };

        let relevance = match query.map(str::trim).filter(|q| !q.is_empty()) {
            None => 0.0,
            Some(q) => {
                let q = q.to_lowercase();
                let mut hits = 0u32;
                if let Some(p) = &entry.path {
                    if p.to_lowercase().contains(&q) {
                        hits += 2;
                    }
                }
                if let Some(n) = &entry.note {
                    if n.to_lowercase().contains(&q) {
                        hits += 1;
                    }
                }
                if let Some(s) = &entry.snippet {
                    if s.to_lowercase().contains(&q) {
                        hits += 1;
                    }
                }
                (hits as f64).min(4.0) / 4.0
            }
        };

        // Weighted sum — recency dominates, kind biases towards actionables,
        // relevance is a strong tie-breaker when supplied.
        0.45 * recency + 0.30 * kind_weight + 0.25 * relevance
    }

    /// Return up to `max` entries, preferring recent + relevant ones.
    /// `query` is optional; when supplied it boosts entries whose path / note /
    /// snippet mention it. Entries are returned in original (chronological)
    /// order so the prompt still reads top-to-bottom.
    pub fn top_entries(&self, max: usize, query: Option<&str>) -> Vec<&EvidenceEntry> {
        if self.entries.len() <= max {
            return self.entries.iter().collect();
        }
        let mut scored: Vec<(usize, f64)> = (0..self.entries.len())
            .map(|i| (i, self.score(i, query)))
            .collect();
        // Stable sort by score desc, then take top N.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut kept: Vec<usize> = scored.into_iter().take(max).map(|(i, _)| i).collect();
        kept.sort_unstable();
        kept.into_iter().map(|i| &self.entries[i]).collect()
    }

    /// Format the log for inclusion in the system prompt.
    ///
    /// When the log has at most ~12 entries everything is included verbatim;
    /// past that, entries are ranked and the top 12 surface (most recent
    /// chronological order preserved).
    pub fn format_for_prompt(&self) -> String {
        self.format_for_prompt_with(12, None)
    }

    /// Same as [`format_for_prompt`] but with explicit cap and relevance query.
    pub fn format_for_prompt_with(&self, max: usize, query: Option<&str>) -> String {
        if self.entries.is_empty() {
            return String::new();
        }

        let kept = self.top_entries(max, query);
        let truncated = kept.len() < self.entries.len();

        let mut out = String::from("\n\nEvidence collected this turn:\n");
        for e in kept {
            let prefix = format!("[{}]", e.kind.as_str());
            let mut line = format!("- {prefix}");
            if let Some(p) = &e.path {
                line.push(' ');
                line.push_str(p);
                if let Some((a, b)) = e.line_range {
                    line.push_str(&format!(":{a}-{b}"));
                }
            }
            if let Some(n) = &e.note {
                if !n.is_empty() {
                    line.push_str(": ");
                    line.push_str(&truncate(n, 200));
                }
            }
            if let Some(s) = &e.snippet {
                if !s.is_empty() {
                    let s = truncate(s, 240);
                    line.push_str("\n  ");
                    line.push_str(&s.replace('\n', "\n  "));
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
        if truncated {
            out.push_str(&format!(
                "[…{} more entries omitted by relevance ranking]\n",
                self.entries.len() - max
            ));
        }
        out
    }
}

// ─── Trace summariser (port of summarizeTrace / recordEvidence) ────────────

/// Tools whose results we surface in evidence (rest are noise).
pub const INTERESTING_TOOLS: &[&str] = &[
    "bash",
    "write_file",
    "patch",
    "create_file",
    "search",
    "graph_search",
];

/// Returns true if `tool` is in [`INTERESTING_TOOLS`].
pub fn is_interesting_tool(tool: &str) -> bool {
    INTERESTING_TOOLS.iter().any(|t| *t == tool)
}

/// Patterns that indicate failure even when an exit code looks OK.
///
/// Lowercased regex-equivalent substring checks; we avoid pulling `regex` in
/// for this one helper since the JS patterns are simple word matches.
fn looks_like_failure(text: &str) -> bool {
    let lowered = text.to_lowercase();
    const WORDS: &[&str] = &[
        "error",
        "failed",
        "failure",
        "exception",
        "traceback",
        "fatal",
        "panic",
        "segfault",
        "syntaxerror",
    ];
    if WORDS.iter().any(|w| word_boundary_contains(&lowered, w)) {
        return true;
    }
    if contains_phrase(&lowered, "cannot find") || contains_phrase(&lowered, "not found") {
        return true;
    }
    false
}

fn word_boundary_contains(haystack: &str, needle: &str) -> bool {
    // Simple word-ish boundary: needle must not be flanked by alphanumerics.
    let bytes = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || bytes.len() < n.len() {
        return false;
    }
    let mut i = 0;
    while i + n.len() <= bytes.len() {
        if &bytes[i..i + n.len()] == n {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = i + n.len();
            let after_ok = after == bytes.len() || !bytes[after].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    // Collapse runs of whitespace then substring-match — emulates `\s+` in JS.
    let collapsed: String = haystack.split_whitespace().collect::<Vec<_>>().join(" ");
    let target: String = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.contains(&target)
}

/// One step in an executed trace. Mirrors the duck-typed JS shape:
/// `{ type, name, args, result, passed }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceStep {
    /// `"tool_call"`, `"validation"`, etc.
    #[serde(rename = "type")]
    pub kind: String,
    /// Tool name, when `kind == "tool_call"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form structured args — we only inspect `command`, `path`, `pattern`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// Stringified tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// For `validation` steps: did it pass?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
}

/// Recorded trace passed to [`summarize_trace`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Trace {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub steps: Vec<TraceStep>,
    /// Wall-clock duration of the trace, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "durationMs")]
    pub duration_ms: Option<u64>,
}

/// Summary digest produced by [`summarize_trace`], ready to store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSummary {
    #[serde(rename = "type")]
    pub kind: String, // "context"
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
}

/// Knobs for [`summarize_trace`]. Mirrors the JS `options` object.
#[derive(Debug, Clone, Copy)]
pub struct SummarizeOptions {
    pub max_body_chars: usize,
}

impl Default for SummarizeOptions {
    fn default() -> Self {
        Self { max_body_chars: 1500 }
    }
}

/// Summarize a finished trace into an evidence digest.
///
/// Returns `None` if the trace has nothing worth storing (no tools, all reads).
pub fn summarize_trace(trace: &Trace, options: SummarizeOptions) -> Option<EvidenceSummary> {
    if trace.steps.is_empty() {
        return None;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut successes: Vec<String> = Vec::new();
    let mut files_edited: BTreeSet<String> = BTreeSet::new();
    let mut validations: u32 = 0;
    let mut validations_failed: u32 = 0;

    for step in &trace.steps {
        match step.kind.as_str() {
            "tool_call" => {
                let name = match step.name.as_deref() {
                    Some(n) => n,
                    None => continue,
                };
                if !is_interesting_tool(name) {
                    continue;
                }

                // Track file mutations.
                if name == "write_file" || name == "patch" {
                    if let Some(p) = step
                        .args
                        .as_ref()
                        .and_then(|a| a.get("path"))
                        .and_then(|v| v.as_str())
                    {
                        files_edited.insert(p.to_string());
                    }
                }

                let result = step.result.as_deref().unwrap_or("");
                let lowered = result.to_lowercase();
                let is_failure = lowered.contains("error: ")
                    || (lowered.contains("exit code") && !lowered.contains("exit code 0"))
                    || looks_like_failure(result);

                if let Some(summary) = compact_step(step, is_failure) {
                    if is_failure {
                        failures.push(summary);
                    } else {
                        successes.push(summary);
                    }
                }
            }
            "validation" => {
                validations += 1;
                if step.passed == Some(false) {
                    validations_failed += 1;
                }
            }
            _ => {}
        }
    }

    if failures.is_empty() && successes.is_empty() && files_edited.is_empty() {
        return None;
    }

    let deduped_failures = dedupe_adjacent(&failures);
    let deduped_successes = dedupe_adjacent(&successes);

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Task: {}", truncate(&trace.prompt, 200)));
    if !files_edited.is_empty() {
        let listed: Vec<&str> = files_edited.iter().take(10).map(|s| s.as_str()).collect();
        lines.push(format!("Files: {}", listed.join(", ")));
    }
    if !failures.is_empty() {
        lines.push(String::from("\nFailed steps:"));
        for f in deduped_failures.iter().take(5) {
            lines.push(format!("- {f}"));
        }
    }
    if !successes.is_empty() {
        lines.push(String::from("\nSuccessful steps:"));
        for s in deduped_successes.iter().take(5) {
            lines.push(format!("- {s}"));
        }
    }
    if validations > 0 {
        lines.push(format!(
            "\nValidations: {}/{} passed",
            validations - validations_failed,
            validations
        ));
    }
    if let Some(ms) = trace.duration_ms {
        lines.push(format!("\nDuration: {:.1}s", ms as f64 / 1000.0));
    }

    let mut body = lines.join("\n");
    if body.chars().count() > options.max_body_chars {
        let cut: String = body.chars().take(options.max_body_chars).collect();
        body = format!("{cut}\n[...truncated]");
    }

    // Title: first 80 chars of the prompt, whitespace-collapsed.
    let collapsed: String = trace.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let title_src = if collapsed.is_empty() {
        "task".to_string()
    } else {
        collapsed
    };
    let title = truncate(&title_src, 80);

    let mut tags = vec![String::from("evidence")];
    if failures.is_empty() && validations_failed == 0 {
        tags.push("success".into());
    } else if !failures.is_empty() {
        tags.push("partial-failure".into());
    }
    if validations_failed > 0 {
        tags.push("validation-failed".into());
    }

    let files: Vec<String> = files_edited.into_iter().take(20).collect();

    Some(EvidenceSummary {
        kind: "context".into(),
        title,
        content: body,
        tags,
        files,
        symbols: Vec::new(),
    })
}

/// Minimal interface for anything that can store a summary. Lets callers plug
/// in their own memory store without us depending on a concrete impl.
pub trait MemoryStore {
    fn remember(&mut self, summary: &EvidenceSummary) -> bool;
}

/// Persist evidence to a memory store. Returns `true` iff the summary was
/// stored. No-op (returns `false`) when:
///   * the env var `ITSY_EVIDENCE_DISABLE=true` is set;
///   * the trace contains nothing worth storing;
///   * the store's `remember` returns `false`.
pub fn record_evidence<S: MemoryStore>(
    store: &mut S,
    trace: &Trace,
    options: SummarizeOptions,
) -> bool {
    if crate::settings::get().evidence_disable {
        return false;
    }
    let summary = match summarize_trace(trace, options) {
        Some(s) => s,
        None => return false,
    };
    store.remember(&summary)
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn compact_step(step: &TraceStep, is_failure: bool) -> Option<String> {
    let name = step.name.as_deref()?;
    let mut detail = String::new();
    if let Some(args) = step.args.as_ref() {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            detail = format!("`{}`", truncate(cmd, 80));
        } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            detail = path.to_string();
        } else if let Some(pat) = args.get("pattern").and_then(|v| v.as_str()) {
            detail = format!("pattern: {}", truncate(pat, 50));
        }
    }

    if is_failure {
        if let Some(result) = step.result.as_deref() {
            let tail = extract_error_tail(result);
            if !tail.is_empty() {
                if !detail.is_empty() {
                    detail.push(' ');
                }
                detail.push_str("→ ");
                detail.push_str(&tail);
            }
        }
    }

    let mut summary = name.to_string();
    if !detail.is_empty() {
        summary.push(' ');
        summary.push_str(&detail);
    }
    // Cap at 200 chars (chars, not bytes).
    Some(truncate_hard(&summary, 200))
}

/// Find the most informative error tail in `result`.
fn extract_error_tail(result: &str) -> String {
    let lines: Vec<&str> = result
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    // Pass 1: a "specific" error/exception/traceback marker.
    for line in lines.iter().rev() {
        if is_specific_error(line) {
            return truncate(line, 120);
        }
    }
    // Pass 2: any failure hint that isn't just an exit-code line.
    for line in lines.iter().rev() {
        if looks_like_failure(line) && !is_generic_failure(line) {
            return truncate(line, 120);
        }
    }
    // Pass 3: generic exit-code line is better than nothing.
    for line in lines.iter().rev() {
        if looks_like_failure(line) {
            return truncate(line, 120);
        }
    }
    lines.last().map(|l| truncate(l, 120)).unwrap_or_default()
}

fn is_specific_error(line: &str) -> bool {
    // Look for a CamelCase name ending in Error/Exception, or one of a few
    // canonical phrases. Mirrors the JS regex without pulling in `regex`.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for an uppercase letter potentially starting a CamelCase ident.
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            let word = &line[start..j];
            if (word.ends_with("Error") || word.ends_with("Exception"))
                && word.len() > "Error".len()
            {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }

    let lowered = line.to_lowercase();
    if lowered.contains("traceback")
        || contains_phrase(&lowered, "cannot find")
        || contains_phrase(&lowered, "not found")
        || word_boundary_contains(&lowered, "undefined")
        || word_boundary_contains(&lowered, "undeclared")
    {
        return true;
    }
    false
}

fn is_generic_failure(line: &str) -> bool {
    let lowered = line.to_lowercase();
    contains_phrase(&lowered, "exit code") || word_boundary_contains(&lowered, "failed")
}

/// Truncate to `n` chars, appending `…` if cut. Matches JS `truncate`.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let kept: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        s.to_string()
    }
}

/// Hard cap at `n` chars with no ellipsis (used for the per-step 200-char cap
/// the JS does via `.slice(0, 200)`).
fn truncate_hard(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Collapse runs of identical adjacent strings into `"<item> (×N)"`.
fn dedupe_adjacent(arr: &[String]) -> Vec<String> {
    if arr.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<&String> = None;
    let mut run_count: usize = 0;
    for item in arr {
        if prev.map(|p| p == item).unwrap_or(false) {
            run_count += 1;
            continue;
        }
        if run_count > 0 {
            let last = out.len() - 1;
            out[last] = format!("{} (×{})", prev.unwrap(), run_count + 1);
        }
        out.push(item.clone());
        prev = Some(item);
        run_count = 0;
    }
    if run_count > 0 {
        let last = out.len() - 1;
        out[last] = format!("{} (×{})", prev.unwrap(), run_count + 1);
    }
    out
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ledger_basic_api() {
        let mut log = EvidenceLog::new();
        assert!(log.is_empty());
        log.add_file("/tmp/a.rs", Some((1, 10)), "module head");
        log.add_snippet("/tmp/a.rs", "fn x() {}");
        log.add_command_output("ls /tmp", "a.rs\nb.rs\n");
        log.add_search_result("fn main", "/tmp/b.rs", "fn main() {}");
        log.add_error("compile failed");
        log.add_note("look here later");
        assert_eq!(log.len(), 6);
        let out = log.format_for_prompt();
        assert!(out.contains("/tmp/a.rs:1-10"));
        assert!(out.contains("[file]"));
        assert!(out.contains("[error]"));
        assert!(out.contains("[search]"));
    }

    #[test]
    fn ledger_json_roundtrip() {
        let mut log = EvidenceLog::new();
        log.add_note("hi");
        let j = log.to_json().unwrap();
        let back = EvidenceLog::from_json(&j).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.entries[0].note.as_deref(), Some("hi"));
    }

    #[test]
    fn ledger_clear_and_empty() {
        let mut log = EvidenceLog::new();
        log.add_note("a");
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.format_for_prompt(), "");
    }

    #[test]
    fn ledger_ranks_when_overflow() {
        let mut log = EvidenceLog::new();
        for i in 0..20 {
            log.add_note(format!("note {i}"));
        }
        // Make one entry match a query.
        log.add_file("/src/needle.rs", None, "the special one");
        let top = log.top_entries(3, Some("needle"));
        assert!(top
            .iter()
            .any(|e| e.path.as_deref() == Some("/src/needle.rs")));
        assert_eq!(top.len(), 3);
    }

    #[test]
    fn summarize_empty_trace_returns_none() {
        let t = Trace::default();
        assert!(summarize_trace(&t, SummarizeOptions::default()).is_none());
    }

    #[test]
    fn summarize_skips_uninteresting_only() {
        let t = Trace {
            prompt: "do thing".into(),
            steps: vec![TraceStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                args: Some(json!({ "path": "/a" })),
                result: Some("ok".into()),
                passed: None,
            }],
            duration_ms: Some(1000),
        };
        assert!(summarize_trace(&t, SummarizeOptions::default()).is_none());
    }

    #[test]
    fn summarize_captures_writes_and_failures() {
        let t = Trace {
            prompt: "make build green".into(),
            steps: vec![
                TraceStep {
                    kind: "tool_call".into(),
                    name: Some("write_file".into()),
                    args: Some(json!({ "path": "src/lib.rs" })),
                    result: Some("ok".into()),
                    passed: None,
                },
                TraceStep {
                    kind: "tool_call".into(),
                    name: Some("bash".into()),
                    args: Some(json!({ "command": "cargo build" })),
                    result: Some("error: cannot find crate `foo`".into()),
                    passed: None,
                },
                TraceStep {
                    kind: "validation".into(),
                    name: None,
                    args: None,
                    result: None,
                    passed: Some(false),
                },
            ],
            duration_ms: Some(2500),
        };
        let s = summarize_trace(&t, SummarizeOptions::default()).expect("summary");
        assert_eq!(s.kind, "context");
        assert!(s.content.contains("Files: src/lib.rs"));
        assert!(s.content.contains("Failed steps:"));
        assert!(s.content.contains("Successful steps:"));
        assert!(s.content.contains("Validations: 0/1 passed"));
        assert!(s.content.contains("Duration: 2.5s"));
        assert!(s.tags.contains(&"evidence".to_string()));
        assert!(s.tags.contains(&"partial-failure".to_string()));
        assert!(s.tags.contains(&"validation-failed".to_string()));
        assert_eq!(s.files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn summarize_success_only_tags_success() {
        let t = Trace {
            prompt: "build".into(),
            steps: vec![TraceStep {
                kind: "tool_call".into(),
                name: Some("bash".into()),
                args: Some(json!({ "command": "cargo build" })),
                result: Some("Finished `dev` profile [unoptimized] target".into()),
                passed: None,
            }],
            duration_ms: None,
        };
        let s = summarize_trace(&t, SummarizeOptions::default()).expect("summary");
        assert!(s.tags.contains(&"success".to_string()));
        assert!(!s.tags.contains(&"partial-failure".to_string()));
    }

    #[test]
    fn dedupe_collapses_runs() {
        let v = vec![
            "patch foo.py → ok".to_string(),
            "patch foo.py → ok".to_string(),
            "patch foo.py → ok".to_string(),
            "bash `ls`".to_string(),
        ];
        let out = dedupe_adjacent(&v);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("(×3)"));
        assert_eq!(out[1], "bash `ls`");
    }

    #[test]
    fn extract_error_tail_prefers_specific() {
        let txt = "Exit code 1\nImportError: no module named foo\nfailed";
        let tail = extract_error_tail(txt);
        assert!(tail.contains("ImportError"));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn record_evidence_respects_disable_env() {
        struct Spy {
            called: bool,
        }
        impl MemoryStore for Spy {
            fn remember(&mut self, _: &EvidenceSummary) -> bool {
                self.called = true;
                true
            }
        }
        let trace = Trace {
            prompt: "p".into(),
            steps: vec![TraceStep {
                kind: "tool_call".into(),
                name: Some("bash".into()),
                args: Some(json!({ "command": "ls" })),
                result: Some("ok".into()),
                passed: None,
            }],
            duration_ms: None,
        };

        // Safety: tests in this module are otherwise serial w.r.t. this var.
        // SAFETY: std::env::set_var is unsafe in edition 2024.
        unsafe {
            std::env::set_var("ITSY_EVIDENCE_DISABLE", "true");
        }
        let mut spy = Spy { called: false };
        assert!(!record_evidence(&mut spy, &trace, SummarizeOptions::default()));
        assert!(!spy.called);
        unsafe {
            std::env::remove_var("ITSY_EVIDENCE_DISABLE");
        }

        let mut spy2 = Spy { called: false };
        assert!(record_evidence(
            &mut spy2,
            &trace,
            SummarizeOptions::default()
        ));
        assert!(spy2.called);
    }
}
