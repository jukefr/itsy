//! `retrieve_context` feature — pure graph traversal, zero LLM calls.
//!
//! Two surfaces are exposed:
//!   1. [`extract_keywords`] / [`retrieve_context`] mirror the JS
//!      keyword extraction + MCP graph-walk used by the cognition layer.
//!   2. [`Candidate`] / [`pick_within_budget`] is the lightweight token-budget
//!      picker that callers use after retrieval to fit results into a context
//!      window.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Token-budget picker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub key: String,
    pub text: String,
    pub score: f64,
    pub tokens: usize,
}

pub fn pick_within_budget(mut candidates: Vec<Candidate>, max_tokens: usize) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    let mut used = 0usize;
    for c in candidates {
        if used + c.tokens > max_tokens {
            continue;
        }
        used += c.tokens;
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Keyword extraction + graph-walk retrieval
// ---------------------------------------------------------------------------

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could", "should",
    "may", "might", "shall", "can", "need", "dare", "ought", "used", "it", "its",
    "this", "that", "these", "those", "i", "me", "my", "we", "our", "you", "your",
    "he", "she", "they", "them", "their", "what", "which", "who", "whom", "when",
    "where", "why", "how", "all", "both", "each", "few", "more", "most", "other",
    "some", "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too",
    "very", "just", "in", "on", "at", "by", "for", "with", "about", "against",
    "to", "from", "up", "down", "of", "off", "out", "over", "under", "into", "and",
    "or", "but", "if", "then", "else", "file", "files", "code", "function", "class",
    "please", "make", "show", "tell", "give", "get", "set", "run", "use", "add",
    "fix", "find", "look", "check", "help", "want", "need", "try", "let", "create",
];

fn is_stop_word(w: &str) -> bool {
    let lower = w.to_lowercase();
    STOP_WORDS.contains(&lower.as_str())
}

/// Extract search keywords from a user message.
///
/// Filters stop words and returns CamelCase / PascalCase words first (they're
/// likely symbol names), then meaningful lowercase words. Capped at 5.
pub fn extract_keywords(message: &str) -> Vec<String> {
    static SPLITTER: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"[\s,.\-_/\\()\[\]{}'"`!?;:]+"#).expect("valid regex literal"));
    static HAS_UPPER: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Z]").expect("valid regex literal"));

    let words: Vec<&str> = SPLITTER.split(message).filter(|w| !w.is_empty()).collect();
    let mut keywords: Vec<String> = Vec::new();

    // Prefer CamelCase/PascalCase first.
    for w in &words {
        if w.len() >= 3 && HAS_UPPER.is_match(w) && !is_stop_word(w) {
            keywords.push((*w).to_string());
        }
    }

    // Then meaningful lowercase words.
    for w in &words {
        if w.len() >= 4 && !HAS_UPPER.is_match(w) && !is_stop_word(w) {
            let lower = w.to_lowercase();
            if !keywords.iter().any(|k| k == &lower) {
                keywords.push(lower);
            }
        }
    }

    keywords.truncate(5);
    keywords
}

/// Outcome of a graph-walk retrieval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetrievedContext {
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub token_estimate: usize,
}

/// Given a slice of raw MCP results (each `r` is the JSON value the MCP server
/// returned), distill file paths and symbol names. Matches the JS routine
/// (`tools/call` response shape with `content: [{ text }]`, otherwise a raw
/// string or any JSON value).
pub fn retrieve_context_from_results(results: &[Value], max_files: usize) -> RetrievedContext {
    if results.is_empty() {
        return RetrievedContext::default();
    }

    static PATH_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"[a-zA-Z0-9_\-/\\]+\.(ts|js|py|rs|go|java|c|cpp|md)").expect("valid regex literal")
    });
    static SYM_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b[A-Z][a-zA-Z0-9]+\b|\bfunction\s+(\w+)").expect("valid regex literal"));
    static FN_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^function\s+").expect("valid regex literal"));

    let mut file_set: HashSet<String> = HashSet::new();
    let mut sym_set: HashSet<String> = HashSet::new();

    for r in results {
        let text = if let Some(arr) = r.get("content").and_then(|v| v.as_array()) {
            arr.iter()
                .map(|c| c.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(s) = r.as_str() {
            s.to_string()
        } else {
            serde_json::to_string(r).unwrap_or_default()
        };

        for m in PATH_RE.find_iter(&text).take(max_files) {
            file_set.insert(m.as_str().to_string());
        }
        for m in SYM_RE.find_iter(&text).take(20) {
            let s = FN_PREFIX.replace(m.as_str(), "").to_string();
            sym_set.insert(s);
        }
    }

    let mut files: Vec<String> = file_set.into_iter().collect();
    files.sort();
    files.truncate(max_files);

    let mut symbols: Vec<String> = sym_set.into_iter().collect();
    symbols.sort();
    symbols.truncate(20);

    let token_estimate = files.len() * 50 + symbols.len() * 10;
    RetrievedContext {
        files,
        symbols,
        token_estimate,
    }
}

/// Convenience helper: pick the top-N keywords for a graph search. The actual
/// MCP plumbing lives outside this crate; callers feed the keywords to the
/// MCP bridge and then pass the responses into [`retrieve_context_from_results`].
pub fn keywords_for_search(message: &str, limit: usize) -> Vec<String> {
    let mut k = extract_keywords(message);
    if k.len() > limit {
        k.truncate(limit);
    }
    k
}
