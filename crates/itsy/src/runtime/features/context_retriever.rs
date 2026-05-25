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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── pick_within_budget ─────────────────────────────────────────────────

    fn cand(key: &str, score: f64, tokens: usize) -> Candidate {
        Candidate { key: key.into(), text: format!("text:{key}"), score, tokens }
    }

    #[test]
    fn picker_returns_highest_score_first_within_budget() {
        let cs = vec![cand("a", 0.5, 100), cand("b", 0.9, 100), cand("c", 0.7, 100)];
        let r = pick_within_budget(cs, 350); // 350 ≥ 3×100 — all three fit
        let keys: Vec<&str> = r.iter().map(|c| c.key.as_str()).collect();
        // Sorted by score desc: b (0.9) > c (0.7) > a (0.5).
        assert_eq!(keys, vec!["b", "c", "a"]);
    }

    /// At exactly the boundary, the third item gets squeezed out.
    #[test]
    fn picker_skips_third_item_at_tight_budget() {
        let cs = vec![cand("a", 0.5, 100), cand("b", 0.9, 100), cand("c", 0.7, 100)];
        let r = pick_within_budget(cs, 250); // exactly room for two
        let keys: Vec<&str> = r.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["b", "c"], "only top two fit in 250-token budget");
    }

    #[test]
    fn picker_skips_items_over_budget() {
        let cs = vec![cand("big", 0.9, 200), cand("small1", 0.5, 50), cand("small2", 0.4, 50)];
        let r = pick_within_budget(cs, 100);
        // big (200 > 100): skip. small1 (50) fits. small2 (50) brings total to 100, fits.
        let keys: Vec<&str> = r.iter().map(|c| c.key.as_str()).collect();
        assert!(!keys.contains(&"big"), "must skip over-budget item");
        assert_eq!(keys, vec!["small1", "small2"]);
    }

    #[test]
    fn picker_handles_empty_input() {
        let r = pick_within_budget(vec![], 1000);
        assert!(r.is_empty());
    }

    #[test]
    fn picker_zero_budget_picks_nothing() {
        let cs = vec![cand("a", 0.9, 1)];
        assert!(pick_within_budget(cs, 0).is_empty(),
            "zero budget must pick nothing");
    }

    // ── extract_keywords ──────────────────────────────────────────────────

    /// CamelCase / PascalCase words are kept verbatim (likely symbol names).
    #[test]
    fn extract_preserves_camelcase() {
        let k = extract_keywords("look at AuthService and UserRepository");
        assert!(k.contains(&"AuthService".to_string()));
        assert!(k.contains(&"UserRepository".to_string()));
    }

    /// Lowercase meaningful words get lowercased.
    #[test]
    fn extract_lowercases_meaningful_words() {
        let k = extract_keywords("debug the parser logic");
        assert!(k.contains(&"parser".to_string()));
        assert!(k.contains(&"logic".to_string()));
    }

    /// Stop words are filtered.
    #[test]
    fn extract_filters_stop_words() {
        let k = extract_keywords("please find the file and fix the bug");
        // "please", "find", "the", "fix" all should be filtered.
        for w in ["please", "find", "the", "fix"] {
            assert!(!k.iter().any(|kw| kw.eq_ignore_ascii_case(w)),
                "stop word {w} must be filtered; got {k:?}");
        }
    }

    /// Output capped at 5 keywords.
    #[test]
    fn extract_caps_at_five() {
        let k = extract_keywords("parser lexer tokenizer evaluator interpreter compiler runtime");
        assert!(k.len() <= 5, "must cap at 5; got {} keywords: {k:?}", k.len());
    }

    /// CamelCase words come BEFORE lowercase ones — pin the priority.
    #[test]
    fn extract_prioritises_camelcase() {
        let k = extract_keywords("rewrite parser and AuthService logic");
        // First non-stopword should be the CamelCase one.
        assert_eq!(k.first().map(|s| s.as_str()), Some("AuthService"),
            "CamelCase must lead; got {k:?}");
    }

    /// Short words (<4 chars lowercase) are filtered.
    #[test]
    fn extract_drops_short_lowercase_words() {
        let k = extract_keywords("a b cd efghijk");
        // "a", "b", "cd" all too short (<4 chars).
        assert!(!k.iter().any(|w| w == "a" || w == "b" || w == "cd"));
        assert!(k.iter().any(|w| w == "efghijk"));
    }

    /// Punctuation is treated as separator.
    #[test]
    fn extract_treats_punctuation_as_separator() {
        let k = extract_keywords("parser,lexer.tokenizer");
        assert!(k.iter().any(|w| w == "parser"));
        assert!(k.iter().any(|w| w == "lexer"));
        assert!(k.iter().any(|w| w == "tokenizer"));
    }

    // ── retrieve_context_from_results ─────────────────────────────────────

    #[test]
    fn retrieve_empty_results_is_default() {
        let r = retrieve_context_from_results(&[], 10);
        assert!(r.files.is_empty());
        assert!(r.symbols.is_empty());
        assert_eq!(r.token_estimate, 0);
    }

    #[test]
    fn retrieve_extracts_file_paths_from_content_array() {
        let results = vec![json!({
            "content": [{"text": "found in src/auth/login.rs and lib/parser.py"}]
        })];
        let r = retrieve_context_from_results(&results, 10);
        assert!(r.files.iter().any(|f| f.contains("login.rs")));
        assert!(r.files.iter().any(|f| f.contains("parser.py")));
    }

    #[test]
    fn retrieve_extracts_symbols_from_text() {
        let results = vec![json!({
            "content": [{"text": "calls AuthService and UserRepository, function login()"}]
        })];
        let r = retrieve_context_from_results(&results, 10);
        assert!(r.symbols.iter().any(|s| s == "AuthService"));
        assert!(r.symbols.iter().any(|s| s == "UserRepository"));
        assert!(r.symbols.iter().any(|s| s == "login"),
            "must extract function name (strip 'function ' prefix); got {:?}", r.symbols);
    }

    #[test]
    fn retrieve_handles_string_result() {
        // Raw string (no `content` array).
        let results = vec![json!("see auth.rs and ParserService")];
        let r = retrieve_context_from_results(&results, 10);
        assert!(r.files.iter().any(|f| f.contains("auth.rs")));
        assert!(r.symbols.iter().any(|s| s == "ParserService"));
    }

    #[test]
    fn retrieve_respects_max_files() {
        let big_text = (0..50).map(|i| format!("file{i}.rs")).collect::<Vec<_>>().join(" ");
        let results = vec![json!({"content": [{"text": big_text}]})];
        let r = retrieve_context_from_results(&results, 5);
        assert!(r.files.len() <= 5, "must cap at max_files=5; got {} files", r.files.len());
    }

    #[test]
    fn retrieve_dedupes_repeated_files() {
        let results = vec![json!({"content": [{"text": "auth.rs auth.rs auth.rs"}]})];
        let r = retrieve_context_from_results(&results, 10);
        let auth_count = r.files.iter().filter(|f| f.contains("auth.rs")).count();
        assert_eq!(auth_count, 1, "must dedupe; got {auth_count} copies of auth.rs");
    }

    /// `keywords_for_search` caps the keyword list at `limit`.
    #[test]
    fn keywords_for_search_respects_limit() {
        let k = keywords_for_search("parser lexer tokenizer evaluator interpreter", 2);
        assert!(k.len() <= 2);
    }
}
