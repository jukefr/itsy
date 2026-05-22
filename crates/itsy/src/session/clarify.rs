//! Clarification loop.
//!
//! Detects vague / ambiguous prompts and asks the user for clarification
//! before wasting tool calls on a misunderstood task.
//!
//! Triggers ONLY when:
//!   * Prompt matches a specific vague pattern (not just being short).
//!   * Multiple interpretations are genuinely possible.
//!
//! Does NOT trigger on:
//!   * Short but actionable commands ("run tests", "fix bug", "add logging").
//!   * Greetings ("hi", "hello") — the model should respond naturally.
//!   * Confirmations ("yes", "no", "ok") — these answer prior questions.

use once_cell::sync::Lazy;
use regex::Regex;

/// Hard-coded vague-phrase patterns lifted from the JS implementation.
static VAGUE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    let raws: &[&str] = &[
        r"(?i)^(fix|do|make|change|update|improve)\s+(it|this|that|things?)$",
        r"(?i)^(help|please|can you|could you)$",
        r"(?i)^(make it|do the|fix the)\s+(better|work|thing|stuff)$",
        r"(?i)^(same|again|more|another)$",
    ];
    raws.iter().map(|r| Regex::new(r).unwrap()).collect()
});

static CONFIRMATION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(yes|no|ok|sure|go|do it|y|n|yep|nope|yeah|nah)$").unwrap());

static MULTI_WORD_CONFIRMATION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(go ahead|go for it|just do it|do that|do both|read it|show me|that one|sounds good|let's do it|let's go|that works)\b")
        .unwrap()
});

static MULTI_NUMBER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(both\s+)?\d+(\s*,\s*|\s+and\s+)\d+$").unwrap());

/// Boolean check: does this message look too vague to act on?
pub fn is_vague(message: &str) -> bool {
    let msg = message.trim();
    if msg.is_empty() || msg.starts_with('@') || msg.starts_with('/') {
        return false;
    }
    if CONFIRMATION_RE.is_match(msg) {
        return false;
    }
    if MULTI_WORD_CONFIRMATION_RE.is_match(msg) {
        return false;
    }
    if MULTI_NUMBER_RE.is_match(msg) {
        return false;
    }
    VAGUE_PATTERNS.iter().any(|re| re.is_match(msg))
}

/// Backward-compatible API: returns a default clarification question when
/// the message is vague, or `None` otherwise. The previous implementation
/// returned bespoke questions per pattern — we now defer to the model via
/// [`get_clarification_instruction`], so a single generic question is fine.
pub fn needs_clarification(message: &str) -> Option<String> {
    if !is_vague(message) {
        return None;
    }
    let msg = message.trim().to_lowercase();
    // Keep a couple of pattern-specific hints for nicer CLI UX.
    if msg.contains("the file") && !msg.contains('@') {
        return Some("Which file? Paste a path or use @path/to/file.".into());
    }
    if msg.starts_with("fix it") || msg.starts_with("fix this") || msg.starts_with("fix that") {
        return Some("Fix what? Describe the error or paste it.".into());
    }
    Some("Could you say a bit more about what you'd like me to do?".into())
}

/// System-prompt addendum the model should follow when a vague message is
/// detected. Tells the model to ask before acting.
pub fn get_clarification_instruction() -> &'static str {
    "The user's message is vague or very short. Before taking action:\n\
     1. State what you THINK they want (your best interpretation)\n\
     2. Ask ONE specific clarifying question\n\
     3. Do NOT use any tools until the user confirms or clarifies."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_on_fix_it() {
        assert!(is_vague("fix it"));
        assert!(is_vague("make it better"));
        assert!(is_vague("again"));
        assert!(is_vague("please"));
    }

    #[test]
    fn ignores_confirmations() {
        for c in ["yes", "no", "ok", "sure", "yep", "nope"] {
            assert!(!is_vague(c), "{} should not be vague", c);
        }
    }

    #[test]
    fn ignores_file_refs_and_commands() {
        assert!(!is_vague("@src/main.rs"));
        assert!(!is_vague("/help"));
    }

    #[test]
    fn allows_actionable_short_commands() {
        assert!(!is_vague("run tests"));
        assert!(!is_vague("fix bug"));
        assert!(!is_vague("add logging"));
    }
}
