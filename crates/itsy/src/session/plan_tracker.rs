//! Plan-Then-Execute mode.
//!
//! Small models drift on multi-step tasks. They forget step 3 by the time
//! they finish step 1, repeat work, or skip steps entirely. This module
//! asks the model to emit a numbered plan FIRST (before any tool calls),
//! then re-injects that plan as an anchor in subsequent turns.
//!
//! Heuristic: only kick in for tasks the model categorizes as needing a plan
//! (multi-step keywords, long messages, file count > 1) — single-shot tasks
//! like "create hello.py" don't need a plan and shouldn't pay the latency.
//!
//! Plan format (we ask the model for):
//!   PLAN:
//!   1. <step>
//!   2. <step>
//!   3. <step>
//!
//! On subsequent turns we inject:
//!   ACTIVE PLAN (step N of M):
//!   ✓ 1. <done step>
//!   → 2. <current step>
//!     3. <pending step>
//!
//! Configuration (environment variables):
//!   ITSY_PLAN=true          force-enable for all tasks (default: heuristic)
//!   ITSY_PLAN=false         disable entirely
//!   ITSY_PLAN_MIN_STEPS=2   minimum step count to keep a plan
//!   ITSY_PLAN_MAX_STEPS=8   trim plans to this many steps

use std::collections::BTreeSet;
use std::env;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Default minimum number of steps for a parsed plan to be kept.
pub fn default_min_steps() -> usize {
    env::var("ITSY_PLAN_MIN_STEPS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2)
}

/// Default maximum number of steps; plans are truncated to this length.
pub fn default_max_steps() -> usize {
    env::var("ITSY_PLAN_MAX_STEPS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(8)
}

// Keywords that strongly suggest a multi-step task. Kept short on purpose so
// we don't trigger on simple prompts.
static PLAN_HINTS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)\b(refactor|migrate|rewrite|reorganize)\b").unwrap(),
        Regex::new(
            r"(?i)\b(implement|build|create)\b.*\b(feature|module|service|api|app|system|project)\b",
        )
        .unwrap(),
        Regex::new(r"(?i)\bstep\s*(by|-)?\s*step\b").unwrap(),
        Regex::new(r"(?i)\b(multiple|several|all)\b.*\b(files?|tests?|functions?|endpoints?)\b")
            .unwrap(),
        Regex::new(r"(?i)\bend.to.end\b").unwrap(),
    ]
});

/// Decide whether a prompt should trigger plan-mode.
pub fn should_plan(user_message: &str) -> bool {
    match env::var("ITSY_PLAN").ok().as_deref() {
        Some("false") => return false,
        Some("true") => return true,
        _ => {}
    }

    if user_message.is_empty() {
        return false;
    }

    // Long messages are usually multi-step.
    if user_message.len() > 300 {
        return true;
    }

    // Keyword hints — strong indicators of a multi-step task.
    if PLAN_HINTS.iter().any(|p| p.is_match(user_message)) {
        return true;
    }

    // Multiple imperative sentences AND the message is reasonably long.
    // Pure 3-sentence prompts like "Fix the bug in X. It uses Y. Use Z." are
    // single-step in practice — we require length > 150 chars too.
    if user_message.len() > 150 {
        static SENT_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[.!?]+").unwrap());
        let count = SENT_SPLIT
            .split(user_message)
            .map(|s| s.trim())
            .filter(|s| s.len() > 10)
            .count();
        if count >= 3 {
            return true;
        }
    }

    false
}

/// Parse a model response that should contain a plan. Returns `None` if no
/// recognizable plan is found.
///
/// Tolerates several formats:
///   `1. step\n2. step`
///   `- step\n- step`
///   `* step\n* step`
///   `PLAN:\n1. step\n...`
///   `STEPS:\n1. step\n...`
pub fn parse_plan(text: &str) -> Option<Vec<String>> {
    if text.is_empty() {
        return None;
    }

    // Strip markdown code fences and bold markers.
    static CODE_FENCE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"```[A-Za-z0-9_]*\n?|\n?```").unwrap());
    static BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*").unwrap());
    let stripped = CODE_FENCE.replace_all(text, "");
    let clean = BOLD.replace_all(&stripped, "").into_owned();

    // Look for a "PLAN:" / "STEPS:" / "APPROACH:" header and use only what
    // follows, until a blank-line-then-uppercase-section break or end of text.
    let body = extract_header_section(&clean).unwrap_or(clean.clone());

    let lines: Vec<String> = body
        .split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let min = default_min_steps();

    // Numbered lines: "1. foo", "1) foo", "1 - foo", "1: foo".
    static NUMBERED: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(\d{1,2})[.\)\-:]\s+(.+)$").unwrap());
    static STARTS_LOWER: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-zA-Z]").unwrap());
    static ENDS_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[:.]$").unwrap());

    let mut numbered: Vec<String> = Vec::new();
    for line in &lines {
        if let Some(caps) = NUMBERED.captures(line) {
            numbered.push(caps[2].trim().to_string());
        } else if !numbered.is_empty()
            && STARTS_LOWER.is_match(line)
            && !ENDS_PUNCT.is_match(line)
            && line.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
            && line.len() < 80
        {
            // Continuation of previous item — only merge short lowercase
            // fragments, not full sentences ending in punctuation or section
            // headers.
            let last = numbered.last_mut().unwrap();
            last.push(' ');
            last.push_str(line);
        }
    }
    if numbered.len() >= min {
        return Some(trim_plan(numbered));
    }

    // Bulleted lines: "- foo", "* foo", "• foo".
    static BULLET: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[-*\u{2022}]\s+(.+)$").unwrap());
    let mut bulleted: Vec<String> = Vec::new();
    for line in &lines {
        if let Some(caps) = BULLET.captures(line) {
            bulleted.push(caps[1].trim().to_string());
        }
    }
    if bulleted.len() >= min {
        return Some(trim_plan(bulleted));
    }

    None
}

/// Extract the body that follows a `PLAN:` / `STEPS:` / `APPROACH:` header.
/// Stops at a blank-line-then-uppercase-section break, or end of text.
fn extract_header_section(text: &str) -> Option<String> {
    static HEADER: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?im)(?:^|\n)(?:plan|steps?|approach):?\s*\n").unwrap()
    });
    let m = HEADER.find(text)?;
    let rest = &text[m.end()..];

    // The JS regex uses a lazy match with `(?=\n\n[A-Z]|$)`; the Rust `regex`
    // crate has no lookahead, so scan manually for the same boundary.
    let bytes = rest.as_bytes();
    let mut end = bytes.len();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            // Find the first non-newline char after the blank line.
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j] == b'\n' || bytes[j] == b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_uppercase() {
                end = i;
                break;
            }
        }
        i += 1;
    }

    Some(rest[..end].to_string())
}

fn trim_plan(steps: Vec<String>) -> Vec<String> {
    let max = default_max_steps();
    steps
        .into_iter()
        .map(|s| {
            // 200 chars (not bytes). Use char_indices for safety on multibyte.
            if s.chars().count() > 200 {
                let cut = s
                    .char_indices()
                    .nth(200)
                    .map(|(i, _)| i)
                    .unwrap_or(s.len());
                let mut out = s[..cut].to_string();
                out.push('\u{2026}');
                out
            } else {
                s
            }
        })
        .take(max)
        .collect()
}

/// A single plan step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub idx: u32,
    pub text: String,
    pub done: bool,
}

/// State for an active plan during a single agent run.
/// One instance per agent loop invocation.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PlanTracker {
    /// Ordered list of steps. Empty when no plan has been ingested.
    pub items: Vec<PlanItem>,
    /// 0-indexed cursor pointing at the next step to work on.
    #[serde(default)]
    pub current_step: usize,
    /// Set of completed step indices (0-indexed). Tracked separately so
    /// callers can mark out-of-order completion.
    #[serde(default)]
    pub completed_steps: BTreeSet<usize>,
    /// Whether plan-mode is active for this run.
    #[serde(default)]
    pub should_inject: bool,
}

impl PlanTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Activate plan-mode for this run. Caller decides via [`should_plan`].
    pub fn activate(&mut self) {
        self.should_inject = true;
    }

    /// Returns `true` if plan-mode is on but no plan has been extracted yet.
    pub fn needs_plan(&self) -> bool {
        self.should_inject && self.items.is_empty()
    }

    /// Set the plan directly from a list of step texts.
    pub fn set_plan(&mut self, items: Vec<String>) {
        self.items = items
            .into_iter()
            .enumerate()
            .map(|(i, text)| PlanItem {
                idx: (i + 1) as u32,
                text,
                done: false,
            })
            .collect();
        self.current_step = 0;
        self.completed_steps.clear();
    }

    /// Try to extract a plan from a model response. Returns `true` on success.
    pub fn ingest_response(&mut self, text: &str) -> bool {
        if !self.should_inject || !self.items.is_empty() {
            return false;
        }
        let Some(parsed) = parse_plan(text) else {
            return false;
        };
        if parsed.len() < default_min_steps() {
            return false;
        }
        self.set_plan(parsed);
        true
    }

    /// Mark step `idx` (1-indexed, matching `PlanItem::idx`) as complete.
    pub fn mark_done(&mut self, idx: u32) {
        let mut hit = None;
        for (i, item) in self.items.iter_mut().enumerate() {
            if item.idx == idx {
                item.done = true;
                hit = Some(i);
                break;
            }
        }
        if let Some(i) = hit {
            self.completed_steps.insert(i);
            // Advance current_step past any already-completed steps.
            while self.completed_steps.contains(&self.current_step) {
                self.current_step += 1;
            }
        }
    }

    /// Mark step `n` (0-indexed) as complete. Mirrors the JS `completeStep(n)`.
    pub fn complete_step(&mut self, n: usize) {
        if n < self.items.len() {
            self.items[n].done = true;
            self.completed_steps.insert(n);
            while self.completed_steps.contains(&self.current_step) {
                self.current_step += 1;
            }
        }
    }

    /// Heuristic hook for successful tool results. Currently a no-op:
    /// auto-advance was found to cause drift in long traces, so callers must
    /// explicitly mark completion.
    pub fn notify_tool_success(&self) {
        // Intentionally empty — mirrors the JS implementation.
    }

    /// First step not yet marked done.
    pub fn next_pending(&self) -> Option<&PlanItem> {
        self.items.iter().find(|i| !i.done)
    }

    /// True once every step is marked done. False when there is no plan.
    pub fn is_complete(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|i| i.done)
    }

    /// Plain checkbox-style rendering, useful for status output.
    pub fn pretty(&self) -> String {
        self.items
            .iter()
            .map(|i| format!("{}. [{}] {}", i.idx, if i.done { "x" } else { " " }, i.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render the plan as a system-prompt fragment. Returns an empty string
    /// when no plan is active.
    pub fn format_for_prompt(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let total = self.items.len();
        let all_done = self.completed_steps.len() >= total;
        let cur = if all_done {
            total
        } else {
            (self.current_step + 1).min(total)
        };
        let mut out = if all_done {
            format!("\n\nCOMPLETED PLAN (all {total} steps done):")
        } else {
            format!("\n\nACTIVE PLAN (step {cur} of {total}):")
        };
        for (i, item) in self.items.iter().enumerate() {
            let mark = if self.completed_steps.contains(&i) {
                '\u{2713}' // ✓
            } else if !all_done && i == self.current_step {
                '\u{2192}' // →
            } else {
                ' '
            };
            out.push_str(&format!("\n{} {}. {}", mark, i + 1, item.text));
        }
        if !all_done {
            out.push_str(&format!(
                "\n\nWork on the current step (\u{2192}). When done, mention \"step {cur} done\" or move on naturally."
            ));
        }
        out
    }

    /// Re-anchoring prompt to inject when the model is drifting from the plan.
    /// Returns `None` if there is no active plan or it's already complete.
    pub fn re_anchor_prompt(&self) -> Option<String> {
        if self.items.is_empty() || self.is_complete() {
            return None;
        }
        Some(self.format_for_prompt())
    }

    /// The instruction we inject when asking the model to produce a plan.
    pub fn plan_request_instruction() -> String {
        let max = default_max_steps();
        format!(
            "\n\nThis is a multi-step task. Before any tool calls, briefly emit a numbered plan in this format:\n\nPLAN:\n1. <first step>\n2. <second step>\n3. <third step>\n\nKeep it to {max} steps or fewer.\n\nIMPORTANT: After the plan, IMMEDIATELY start executing step 1 with the appropriate tool call. Do NOT stop after writing the plan — the plan is just a header for your work, not the work itself. The user expects you to actually do all the steps."
        )
    }

    /// Reset all state. The tracker becomes equivalent to [`PlanTracker::new`].
    pub fn reset(&mut self) {
        self.items.clear();
        self.current_step = 0;
        self.completed_steps.clear();
        self.should_inject = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numbered() {
        let plan = parse_plan("1. first\n2. second\n3. third").unwrap();
        assert_eq!(plan, vec!["first", "second", "third"]);
    }

    #[test]
    fn parse_with_header() {
        let text = "Sure, here's my plan.\n\nPLAN:\n1. read the file\n2. edit it\n3. run tests\n\nLet's start.";
        let plan = parse_plan(text).unwrap();
        assert_eq!(plan, vec!["read the file", "edit it", "run tests"]);
    }

    #[test]
    fn parse_bulleted() {
        let plan = parse_plan("- foo\n- bar\n- baz").unwrap();
        assert_eq!(plan, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn parse_bulleted_unicode() {
        let plan = parse_plan("\u{2022} foo\n\u{2022} bar").unwrap();
        assert_eq!(plan, vec!["foo", "bar"]);
    }

    #[test]
    fn parse_rejects_single_item() {
        assert!(parse_plan("1. only one").is_none());
    }

    #[test]
    fn parse_strips_code_fences_and_bold() {
        let text = "```\n1. **alpha**\n2. **beta**\n```";
        let plan = parse_plan(text).unwrap();
        assert_eq!(plan, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_continuation_merge() {
        let text = "1. start the task\nand keep going\n2. finish up";
        let plan = parse_plan(text).unwrap();
        assert_eq!(plan, vec!["start the task and keep going", "finish up"]);
    }

    #[test]
    fn trim_caps_at_max_steps() {
        // SAFETY: tests are single-threaded by default for this module access.
        let many = (1..=20)
            .map(|i| format!("{i}. step number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let plan = parse_plan(&many).unwrap();
        assert!(plan.len() <= default_max_steps());
    }

    #[test]
    fn tracker_lifecycle() {
        let mut t = PlanTracker::new();
        t.set_plan(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(t.items.len(), 3);
        assert_eq!(t.next_pending().unwrap().text, "a");
        t.mark_done(1);
        assert_eq!(t.next_pending().unwrap().text, "b");
        assert_eq!(t.current_step, 1);
        t.mark_done(2);
        t.mark_done(3);
        assert!(t.is_complete());
        assert!(t.next_pending().is_none());
    }

    #[test]
    fn re_anchor_only_when_active() {
        let mut t = PlanTracker::new();
        assert!(t.re_anchor_prompt().is_none());
        t.set_plan(vec!["a".into(), "b".into()]);
        let prompt = t.re_anchor_prompt().unwrap();
        assert!(prompt.contains("ACTIVE PLAN"));
        t.mark_done(1);
        t.mark_done(2);
        assert!(t.re_anchor_prompt().is_none()); // complete -> no anchor
    }

    #[test]
    fn format_for_prompt_marks_current() {
        let mut t = PlanTracker::new();
        t.set_plan(vec!["alpha".into(), "beta".into(), "gamma".into()]);
        t.mark_done(1);
        let out = t.format_for_prompt();
        assert!(out.contains("\u{2713} 1. alpha"));
        assert!(out.contains("\u{2192} 2. beta"));
        assert!(out.contains("  3. gamma"));
    }

    #[test]
    fn ingest_requires_activation() {
        let mut t = PlanTracker::new();
        assert!(!t.ingest_response("1. a\n2. b"));
        t.activate();
        assert!(t.needs_plan());
        assert!(t.ingest_response("1. a\n2. b"));
        assert!(!t.needs_plan());
        assert_eq!(t.items.len(), 2);
    }

    #[test]
    fn should_plan_long_message() {
        let long = "x".repeat(400);
        assert!(should_plan(&long));
    }

    #[test]
    fn should_plan_keyword() {
        assert!(should_plan("please refactor this module"));
    }

    #[test]
    fn should_plan_short_simple() {
        assert!(!should_plan("create hello.py"));
    }
}
