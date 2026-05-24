//! Knowledge Injection
//!
//! Loads short reference docs from a `knowledge/` directory and injects only
//! the most relevant ones into the system prompt based on keywords in the
//! user's last message. Designed for small models that benefit from having
//! algorithm cheat sheets, syntax reminders, or domain notes inline rather
//! than reasoning everything from first principles.
//!
//! Layout:
//! ```text
//!   knowledge/
//!     algorithms/binary-search.md
//!     syntax/python-fstrings.md
//!     conventions/git-commit-style.md
//!     ...
//! ```
//!
//! Each .md file is a focused 100-500 word note. The first line should be a
//! `# Title` and the rest should be content. Optional front-matter with
//! `keywords:` controls when it gets injected (otherwise we infer from filename
//! + first heading).
//!
//! Configuration:
//! - `ITSY_KNOWLEDGE_DIR=./knowledge`   path to knowledge directory
//! - `ITSY_KNOWLEDGE_MAX_TOKENS=1500`   per-message injection cap
//! - `ITSY_KNOWLEDGE_DISABLE=true`      turn off entirely
//!
//! Selection algorithm:
//!   1. Parse user message into normalized words
//!   2. Score each .md file by keyword overlap (filename + frontmatter + heading)
//!   3. Pick top-K such that total chars stay under the budget
//!
//! We deliberately do NOT use embeddings here — keeping the implementation
//! fully local and dependency-free. Filename + heading match is good enough
//! for the cheat-sheet use case.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use once_cell::sync::{Lazy, OnceCell};
use parking_lot::Mutex;
use regex::Regex;

pub const DEFAULT_MAX_TOKENS: usize = 1500;

const DEFAULT_DIR_NAMES: &[&str] = &["knowledge", ".knowledge", "docs/knowledge"];
const MAX_FILE_SIZE: u64 = 100 * 1024;
const PER_ENTRY_CHAR_CAP: usize = 1500;

// Common English stop words so a query like "how do I sort an array" doesn't
// pull every note that happens to contain "do" or "an".
static STOP_WORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "the", "and", "for", "with", "this", "that", "from", "have", "has", "had",
        "are", "was", "were", "will", "would", "should", "could", "can", "may",
        "how", "what", "when", "where", "why", "which", "who", "whom",
        "into", "onto", "about", "over", "under", "between", "through",
        "all", "any", "some", "each", "every", "both", "either", "neither",
        "not", "only", "just", "also", "too", "very", "much", "many",
        "one", "two", "three", "first", "second", "last", "next", "previous",
        "use", "used", "using", "make", "made", "get", "got", "set", "put",
        "you", "your", "they", "them", "their", "these", "those",
    ]
    .iter()
    .copied()
    .collect()
});

static FRONTMATTER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)^---\n(.*?)\n---\n(.*)$").expect("valid regex literal"));
static FRONTMATTER_STRIP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)^---\n.*?\n---\n").expect("valid regex literal"));
static KEYWORDS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)keywords?:\s*(.+)").expect("valid regex literal"));
static HEADING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^#\s+(.+)").expect("valid regex literal"));

// ─── Public types (kept compatible with previous stub) ──────────────────────

/// Minimal document representation used by callers that just want raw content.
#[derive(Debug, Clone)]
pub struct KnowledgeDoc {
    pub key: String,
    pub content: String,
}

/// A selected knowledge entry chosen for prompt injection.
#[derive(Debug, Clone)]
pub struct SelectedEntry {
    pub name: String,
    pub rel_path: String,
    pub content: String,
    pub score: u32,
}

/// Parsed metadata for one knowledge file, plus its body. We cache the body
/// at parse time because knowledge files are small (capped at 100KB).
#[derive(Debug, Clone)]
struct IndexEntry {
    path: PathBuf,
    rel_path: String,
    name: String,
    heading: String,
    keywords: Vec<String>,
    /// Body with frontmatter already stripped.
    body: String,
    /// File mtime for cache invalidation.
    mtime: Option<SystemTime>,
}

// ─── Backward-compatible free function ─────────────────────────────────────

/// Walks the `knowledge/` directory under `root` and returns every `.md`/`.txt`
/// document. Kept for callers that just want the raw docs without scoring.
pub fn load_all(root: &Path) -> Vec<KnowledgeDoc> {
    let mut out = Vec::new();
    let dir = root.join("knowledge");
    if !dir.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
        if entry.file_type().is_file() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext == "md" || ext == "txt" {
                if let Ok(content) = fs::read_to_string(path) {
                    let key = path
                        .strip_prefix(&dir)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned();
                    out.push(KnowledgeDoc { key, content });
                }
            }
        }
    }
    out
}

/// Keyword-scored search over already-loaded docs. Matches the JS scoring
/// algorithm: 1 point per matched keyword token from the path/name; heading
/// tokens count double; an exact name match adds 1 more.
pub fn search<'a>(
    docs: &'a [KnowledgeDoc],
    query: &str,
    max_results: usize,
) -> Vec<&'a KnowledgeDoc> {
    let query_tokens: HashSet<String> = tokenize(query).into_iter().collect();
    if query_tokens.is_empty() || max_results == 0 {
        return Vec::new();
    }

    let mut scored: Vec<(u32, &KnowledgeDoc)> = docs
        .iter()
        .map(|d| {
            let (keywords, heading, name) = derive_signals(&d.key, &d.content);
            (score_signals(&keywords, &heading, &name, &query_tokens), d)
        })
        .filter(|(s, _)| *s > 0)
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored.into_iter().take(max_results).map(|(_, d)| d).collect()
}

/// Format a slice of pre-selected docs as a system-prompt block, respecting a
/// rough token budget (1 token ≈ 4 chars). Returns an empty string if the
/// input is empty.
pub fn format_for_prompt(docs: &[&KnowledgeDoc], max_tokens: usize) -> String {
    if docs.is_empty() {
        return String::new();
    }
    let max_chars = max_tokens.saturating_mul(4);
    let mut out = String::from("\n\nRelevant reference notes:\n");
    let mut used: usize = 0;
    let mut included = 0usize;

    for d in docs {
        let cleaned = strip_frontmatter(&d.content).trim().to_string();
        if cleaned.is_empty() {
            continue;
        }
        let truncated = if cleaned.len() > PER_ENTRY_CHAR_CAP {
            let mut s = safe_slice(&cleaned, PER_ENTRY_CHAR_CAP);
            s.push_str("\n[...truncated]");
            s
        } else {
            cleaned
        };

        let block = format!("\n--- {} ---\n{}\n", d.key, truncated);
        if used + block.len() > max_chars {
            if included == 0 {
                // Always include at least the top hit, but truncated to fit.
                let header = format!("\n--- {} ---\n", d.key);
                let footer = "\n";
                let budget = max_chars
                    .saturating_sub(out.len())
                    .saturating_sub(header.len())
                    .saturating_sub(footer.len())
                    .saturating_sub(100);
                let fit = safe_slice(&truncated, budget);
                out.push_str(&header);
                out.push_str(&fit);
                out.push_str(footer);
            }
            break;
        }
        out.push_str(&block);
        used += block.len();
        included += 1;
    }

    if included == 0 && used == 0 {
        // Nothing actually emitted (all empty / no budget).
        return String::new();
    }
    out
}

// ─── KnowledgeLoader: indexed, cached, query-driven ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct KnowledgeLoaderOptions {
    pub root_dir: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub max_tokens: Option<usize>,
    pub disable: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct SelectOptions {
    pub max_tokens: Option<usize>,
}

/// Loader with lazy directory resolution + in-memory index keyed by file mtime.
pub struct KnowledgeLoader {
    root_dir: PathBuf,
    dir_override: Option<PathBuf>,
    max_tokens: usize,
    disabled: bool,
    /// `Some(Some(dir))` = resolved to a directory.
    /// `Some(None)` = resolved, no directory exists.
    /// `None` = not yet resolved.
    resolved_dir: Mutex<Option<Option<PathBuf>>>,
    /// Cached index keyed by file path → (entry, mtime).
    index: Mutex<Option<Vec<IndexEntry>>>,
}

impl KnowledgeLoader {
    pub fn new(options: KnowledgeLoaderOptions) -> Self {
        let max_tokens = options
            .max_tokens
            .or_else(|| {
                env::var("ITSY_KNOWLEDGE_MAX_TOKENS")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
            })
            .unwrap_or(DEFAULT_MAX_TOKENS);

        let disabled = options.disable.unwrap_or_else(|| {
            env::var("ITSY_KNOWLEDGE_DISABLE").ok().as_deref() == Some("true")
        });

        let dir_override = options
            .dir
            .or_else(|| env::var("ITSY_KNOWLEDGE_DIR").ok().map(PathBuf::from));

        let root_dir = options
            .root_dir
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            root_dir,
            dir_override,
            max_tokens,
            disabled,
            resolved_dir: Mutex::new(None),
            index: Mutex::new(None),
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Resolve which directory we're loading from. First match wins.
    /// Returns `None` if no knowledge directory exists.
    fn resolve_dir(&self) -> Option<PathBuf> {
        let mut slot = self.resolved_dir.lock();
        if let Some(cached) = slot.as_ref() {
            return cached.clone();
        }
        if self.disabled {
            *slot = Some(None);
            return None;
        }

        let candidates: Vec<PathBuf> = if let Some(d) = &self.dir_override {
            vec![d.clone()]
        } else {
            DEFAULT_DIR_NAMES
                .iter()
                .map(|n| self.root_dir.join(n))
                .collect()
        };

        for c in candidates {
            if let Ok(stat) = fs::metadata(&c) {
                if stat.is_dir() {
                    *slot = Some(Some(c.clone()));
                    return Some(c);
                }
            }
        }
        *slot = Some(None);
        None
    }

    /// Walk the knowledge directory and build / refresh the in-memory index.
    /// We refresh when any file's mtime has changed since last scan.
    fn build_index(&self) -> Vec<IndexEntry> {
        let dir = match self.resolve_dir() {
            Some(d) => d,
            None => {
                *self.index.lock() = Some(Vec::new());
                return Vec::new();
            }
        };

        // Scan disk first so we can compare mtimes to the cached entries.
        let mut scanned: HashMap<PathBuf, SystemTime> = HashMap::new();
        for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if ext != "md" && ext != "txt" {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.len() > MAX_FILE_SIZE {
                    continue;
                }
                let mtime = meta.modified().ok().unwrap_or(SystemTime::UNIX_EPOCH);
                scanned.insert(path.to_path_buf(), mtime);
            }
        }

        // Check whether the cache is still valid: same set of paths, same mtimes.
        {
            let cache = self.index.lock();
            if let Some(existing) = cache.as_ref() {
                let same_size = existing.len() == scanned.len();
                let all_match = same_size
                    && existing.iter().all(|e| {
                        scanned
                            .get(&e.path)
                            .map(|m| Some(*m) == e.mtime)
                            .unwrap_or(false)
                    });
                if all_match {
                    return existing.clone();
                }
            }
        }

        // Rebuild.
        let mut entries: Vec<IndexEntry> = Vec::with_capacity(scanned.len());
        for (path, mtime) in scanned {
            if let Some(entry) = parse_entry(&path, &dir, Some(mtime)) {
                entries.push(entry);
            }
        }
        // Stable order for deterministic results when scores tie.
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

        *self.index.lock() = Some(entries.clone());
        entries
    }

    fn score_entry(&self, entry: &IndexEntry, query_tokens: &HashSet<String>) -> u32 {
        if query_tokens.is_empty() {
            return 0;
        }
        score_signals(&entry.keywords, &entry.heading, &entry.name, query_tokens)
    }

    /// Pick the most relevant knowledge notes for the given query, fitting
    /// under the token budget.
    pub fn select_for_query(&self, query: &str, opts: &SelectOptions) -> Vec<SelectedEntry> {
        if self.disabled {
            return Vec::new();
        }
        let index = self.build_index();
        if index.is_empty() {
            return Vec::new();
        }
        let query_tokens: HashSet<String> = tokenize(query).into_iter().collect();
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(u32, IndexEntry)> = index
            .into_iter()
            .map(|e| {
                let s = self.score_entry(&e, &query_tokens);
                (s, e)
            })
            .filter(|(s, _)| *s > 0)
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));

        let max_chars = opts.max_tokens.unwrap_or(self.max_tokens).saturating_mul(4);
        let mut out: Vec<SelectedEntry> = Vec::new();
        let mut used: usize = 0;

        for (score, e) in scored {
            let cleaned = strip_frontmatter(&e.body).trim().to_string();
            if cleaned.is_empty() {
                continue;
            }
            let truncated = if cleaned.len() > PER_ENTRY_CHAR_CAP {
                let mut s = safe_slice(&cleaned, PER_ENTRY_CHAR_CAP);
                s.push_str("\n[...truncated]");
                s
            } else {
                cleaned
            };

            if used + truncated.len() > max_chars {
                if out.is_empty() {
                    let budget = max_chars.saturating_sub(100);
                    let fit = safe_slice(&truncated, budget);
                    out.push(SelectedEntry {
                        name: e.name.clone(),
                        rel_path: e.rel_path.clone(),
                        content: fit,
                        score,
                    });
                }
                break;
            }
            used += truncated.len();
            out.push(SelectedEntry {
                name: e.name.clone(),
                rel_path: e.rel_path.clone(),
                content: truncated,
                score,
            });
        }

        out
    }

    /// Format selected entries as a system-prompt block. Returns `""` if
    /// nothing matches.
    pub fn format_for_prompt(&self, query: &str, opts: &SelectOptions) -> String {
        let entries = self.select_for_query(query, opts);
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nRelevant reference notes:\n");
        for e in entries {
            out.push_str(&format!("\n--- {} ---\n{}\n", e.rel_path, e.content));
        }
        out
    }

    /// Drop the cached index — next call rebuilds.
    pub fn invalidate(&self) {
        *self.index.lock() = None;
        *self.resolved_dir.lock() = None;
    }
}

// ─── Singleton ─────────────────────────────────────────────────────────────

static INSTANCE: OnceCell<Mutex<Option<std::sync::Arc<KnowledgeLoader>>>> = OnceCell::new();

fn instance_slot() -> &'static Mutex<Option<std::sync::Arc<KnowledgeLoader>>> {
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Get the singleton loader, constructing it on first use.
pub fn get_knowledge_loader() -> std::sync::Arc<KnowledgeLoader> {
    let slot = instance_slot();
    let mut guard = slot.lock();
    if let Some(existing) = guard.as_ref() {
        return existing.clone();
    }
    let new = std::sync::Arc::new(KnowledgeLoader::new(KnowledgeLoaderOptions::default()));
    *guard = Some(new.clone());
    new
}

/// Replace the singleton with a configured loader.
pub fn init_knowledge_loader(options: KnowledgeLoaderOptions) -> std::sync::Arc<KnowledgeLoader> {
    let slot = instance_slot();
    let mut guard = slot.lock();
    let new = std::sync::Arc::new(KnowledgeLoader::new(options));
    *guard = Some(new.clone());
    new
}

/// Drop the singleton (mostly for tests).
pub fn reset_knowledge_loader() {
    if let Some(slot) = INSTANCE.get() {
        *slot.lock() = None;
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Tokenize a query into normalized lowercase words ≥ 3 chars, skipping stop
/// words. Mirrors the JS `_tokenize`.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP_WORDS.contains(*w))
        .map(|w| w.to_string())
        .collect()
}

/// Split heading text into tokens > 2 chars (no stop-word filter — matches JS).
fn heading_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_string())
        .collect()
}

/// Compute the score given the precomputed signals. Shared between the
/// stateless `search()` and `KnowledgeLoader::score_entry`.
fn score_signals(
    keywords: &[String],
    heading: &str,
    name: &str,
    query_tokens: &HashSet<String>,
) -> u32 {
    if query_tokens.is_empty() {
        return 0;
    }
    let mut score: u32 = 0;
    for kw in keywords {
        if query_tokens.contains(kw) {
            score += 1;
        }
    }
    if !heading.is_empty() {
        for t in heading_tokens(heading) {
            if query_tokens.contains(&t) {
                score += 2;
            }
        }
    }
    if query_tokens.contains(&name.to_lowercase()) {
        score += 1;
    }
    score
}

/// Strip a YAML-ish frontmatter block from the head of a markdown body.
fn strip_frontmatter(body: &str) -> String {
    FRONTMATTER_STRIP_RE.replace(body, "").to_string()
}

/// Derive (keywords, heading, name) signals from a doc's key + content,
/// without needing a parsed index entry. Used by the stateless `search()`.
fn derive_signals(rel: &str, content: &str) -> (Vec<String>, String, String) {
    let name_path = Path::new(rel);
    let name = name_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let path_tokens: Vec<String> = rel
        .to_lowercase()
        .split(['/', '\\', '_', '-', '.', ' ', '\t'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mut keywords_set: HashSet<String> = path_tokens.into_iter().collect();
    let mut body: &str = content;

    if let Some(caps) = FRONTMATTER_RE.captures(content) {
        let fm = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        body = caps.get(2).map(|m| m.as_str()).unwrap_or(content);
        if let Some(kw_caps) = KEYWORDS_RE.captures(fm) {
            if let Some(raw) = kw_caps.get(1) {
                for piece in raw.as_str().split(|c: char| c == ',' || c.is_whitespace()) {
                    let cleaned = piece
                        .trim()
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .trim_matches(|c: char| c == '\'' || c == '"')
                        .to_lowercase();
                    if !cleaned.is_empty() {
                        keywords_set.insert(cleaned);
                    }
                }
            }
        }
    }

    let heading = HEADING_RE
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();

    if !heading.is_empty() {
        for t in heading
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
        {
            keywords_set.insert(t.to_string());
        }
    }
    if !name.is_empty() {
        keywords_set.insert(name.to_lowercase());
    }

    (keywords_set.into_iter().collect(), heading, name)
}

/// Parse a single knowledge file's metadata + body. Returns `None` on I/O
/// error or oversized files.
fn parse_entry(path: &Path, root: &Path, mtime: Option<SystemTime>) -> Option<IndexEntry> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_SIZE {
        return None;
    }
    let head = fs::read_to_string(path).ok()?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let (keywords, heading, _name_from_signals) = derive_signals(&rel, &head);

    // Body = content with frontmatter still present (we strip when emitting).
    Some(IndexEntry {
        path: path.to_path_buf(),
        rel_path: rel,
        name,
        heading,
        keywords,
        body: head,
        mtime: mtime.or_else(|| meta.modified().ok()),
    })
}

/// Slice a string by a char-byte budget without splitting a UTF-8 sequence.
fn safe_slice(s: &str, max_bytes: usize) -> String {
    if max_bytes >= s.len() {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, rel: &str, body: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&full).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn tokenize_filters_stopwords_and_short_words() {
        let toks = tokenize("How do I sort an array of integers?");
        assert!(toks.contains(&"sort".into()));
        assert!(toks.contains(&"array".into()));
        assert!(toks.contains(&"integers".into()));
        assert!(!toks.contains(&"how".into()));
        assert!(!toks.contains(&"do".into()));
        assert!(!toks.contains(&"an".into()));
    }

    #[test]
    fn frontmatter_extracts_keywords_and_strips() {
        let body = "---\nkeywords: binary, search, divide\n---\n# Binary Search\n\nbody text\n";
        let (kws, heading, _) = derive_signals("algorithms/binary-search.md", body);
        assert!(kws.contains(&"binary".to_string()));
        assert!(kws.contains(&"search".to_string()));
        assert!(kws.contains(&"divide".to_string()));
        assert_eq!(heading, "Binary Search");
        let stripped = strip_frontmatter(body);
        assert!(stripped.starts_with("# Binary Search"));
    }

    #[test]
    fn search_ranks_by_overlap_and_heading_bonus() {
        let docs = vec![
            KnowledgeDoc {
                key: "algorithms/binary-search.md".into(),
                content: "# Binary Search\n\nO(log n) lookup".into(),
            },
            KnowledgeDoc {
                key: "syntax/python-fstrings.md".into(),
                content: "# Python F-Strings\n\nf\"{x}\"".into(),
            },
        ];
        let hits = search(&docs, "binary search algorithm", 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].key, "algorithms/binary-search.md");
    }

    #[test]
    fn loader_select_respects_disable_flag() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "knowledge/foo.md", "# Foo\n\nbar baz");
        let loader = KnowledgeLoader::new(KnowledgeLoaderOptions {
            root_dir: Some(tmp.path().to_path_buf()),
            disable: Some(true),
            ..Default::default()
        });
        assert!(loader.select_for_query("foo", &SelectOptions::default()).is_empty());
    }

    #[test]
    fn loader_finds_and_scores_docs() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            tmp.path(),
            "knowledge/algorithms/binary-search.md",
            "---\nkeywords: binary, search\n---\n# Binary Search\n\nO(log n)",
        );
        write_file(
            tmp.path(),
            "knowledge/syntax/python-fstrings.md",
            "# Python F-Strings\n\nf\"{x}\"",
        );
        let loader = KnowledgeLoader::new(KnowledgeLoaderOptions {
            root_dir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        });
        let selected = loader.select_for_query("binary search", &SelectOptions::default());
        assert!(!selected.is_empty());
        assert!(selected[0].rel_path.contains("binary-search"));
        assert!(selected[0].score > 0);

        let formatted = loader.format_for_prompt("binary search", &SelectOptions::default());
        assert!(formatted.contains("Relevant reference notes"));
        assert!(formatted.contains("Binary Search"));
        assert!(!formatted.contains("---\nkeywords"));
    }

    #[test]
    fn format_for_prompt_truncates_to_budget() {
        let big: String = "lorem ipsum ".repeat(500);
        let doc = KnowledgeDoc {
            key: "big.md".into(),
            content: format!("# Big\n\n{}", big),
        };
        let refs: Vec<&KnowledgeDoc> = vec![&doc];
        let out = format_for_prompt(&refs, 50); // 200 chars budget
        assert!(out.len() < 400);
    }

    #[test]
    fn loader_cache_invalidates_on_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "knowledge/x.md", "# X\n\nalpha beta");
        let loader = KnowledgeLoader::new(KnowledgeLoaderOptions {
            root_dir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        });
        let first = loader.build_index();
        assert_eq!(first.len(), 1);
        // Add a new file → cache should rebuild.
        write_file(tmp.path(), "knowledge/y.md", "# Y\n\ngamma delta");
        let second = loader.build_index();
        assert_eq!(second.len(), 2);
    }

    #[test]
    fn singleton_returns_same_instance() {
        reset_knowledge_loader();
        let a = get_knowledge_loader();
        let b = get_knowledge_loader();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        reset_knowledge_loader();
    }
}
