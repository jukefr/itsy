//! Tool-call deduplication.
//!
//! Small models often loop: they call `read_file` on the same file twice in
//! a row, or `search` for the same pattern, or run the same `bash` command.
//! Dedup short-circuits identical consecutive tool calls within a sliding
//! window, returning the cached result instead of re-executing. Only applies
//! to read-only / pure tools — never to anything with side effects.
//!
//! Configuration:
//!   `ITSY_DEDUP=false`            disable entirely
//!   `ITSY_DEDUP_WINDOW=5`         number of recent calls considered
//!   `ITSY_DEDUP_TTL_SECS=30`      default time-to-live per cache entry
//!   `ITSY_DEDUP_SOFT=1`           soft-dedup-with-warning mode (no cache hit,
//!                                  just log/mark — call still runs)
//!   `ITSY_DEDUP_SIMILARITY=0.92`  semantic-similarity threshold (0..=1)

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Pure / read-only tools — safe to deduplicate.
pub static PURE_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut s = HashSet::new();
    for t in [
        "read_file",
        "list_files",
        "search",
        "grep",
        "graph_search",
        "explain_symbol",
        "find_by_path",
        "find_by_signature",
        "fuzzy_find_symbol",
        "get_repo_stats",
        "list_projects",
        "memory_load",
        "memory_for_file",
        "memory_for_symbol",
        "memory_list",
        "web_search",
        "web_fetch",
    ] {
        s.insert(t);
    }
    s
});

/// Tools that are pure but very chatty — we let them through unconditionally
/// (no dedup, no warning). Override via `ITSY_DEDUP_NOISY=tool1,tool2`.
pub static NOISY_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut s = HashSet::new();
    s.insert("list_files");
    s
});

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key).ok().as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("on") => true,
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => default,
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Per-tool TTL overrides. Anything not listed falls back to the global TTL.
fn per_tool_ttl(name: &str) -> Option<Duration> {
    match name {
        // Web data goes stale quickly — small TTL.
        "web_search" | "web_fetch" => Some(Duration::from_secs(60)),
        // Filesystem reads are stable for a short while.
        "read_file" | "list_files" => Some(Duration::from_secs(15)),
        // Code search is more expensive — cache longer.
        "search" | "grep" | "graph_search" => Some(Duration::from_secs(45)),
        // Memory snapshots are intentionally cheap to re-read.
        "memory_load" | "memory_for_file" | "memory_for_symbol" | "memory_list" => {
            Some(Duration::from_secs(10))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct Entry {
    hash: String,
    name: String,
    args_norm: String,
    result: Option<Value>,
    ts: Instant,
    ttl: Duration,
}

/// Outcome of a dedup lookup.
#[derive(Debug, Clone)]
pub enum DedupOutcome {
    /// Tool is not eligible for dedup (impure or in the noisy allowlist).
    Skip,
    /// First time we've seen this call — record it before/after executing.
    Miss,
    /// Identical call was just made; cached result returned.
    Hit(Value),
    /// Soft mode: identical (or near-identical) call detected, but we still
    /// let the caller execute. Caller should attach the warning to the
    /// model-visible result so the model learns to stop repeating itself.
    SoftWarn(String),
}

/// Tool-call deduplicator with semantic similarity and per-tool TTL.
#[derive(Debug)]
pub struct ToolDedup {
    capacity: usize,
    disabled: bool,
    soft: bool,
    similarity_threshold: f64,
    default_ttl: Duration,
    noisy: HashSet<String>,
    recent: VecDeque<Entry>,
    pub hits: u64,
    pub misses: u64,
    pub soft_warnings: u64,
}

impl ToolDedup {
    pub fn new() -> Self {
        let capacity = env_usize("ITSY_DEDUP_WINDOW", 5).max(1);
        let disabled = !env_bool("ITSY_DEDUP", true);
        let soft = env_bool("ITSY_DEDUP_SOFT", false);
        let default_ttl = Duration::from_secs(env_u64("ITSY_DEDUP_TTL_SECS", 30));
        let similarity_threshold = env_f64("ITSY_DEDUP_SIMILARITY", 0.92).clamp(0.0, 1.0);
        let mut noisy: HashSet<String> = NOISY_TOOLS.iter().map(|s| s.to_string()).collect();
        if let Ok(extra) = std::env::var("ITSY_DEDUP_NOISY") {
            for t in extra.split(',') {
                let t = t.trim();
                if !t.is_empty() {
                    noisy.insert(t.to_string());
                }
            }
        }
        Self {
            capacity,
            disabled,
            soft,
            similarity_threshold,
            default_ttl,
            noisy,
            recent: VecDeque::new(),
            hits: 0,
            misses: 0,
            soft_warnings: 0,
        }
    }

    fn ttl_for(&self, name: &str) -> Duration {
        per_tool_ttl(name).unwrap_or(self.default_ttl)
    }

    fn purge_expired(&mut self, now: Instant) {
        while let Some(front) = self.recent.front() {
            if now.duration_since(front.ts) > front.ttl {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }

    fn hash(name: &str, args_norm: &str) -> String {
        let mut h = Sha256::new();
        h.update(name.as_bytes());
        h.update(b"|");
        h.update(args_norm.as_bytes());
        let digest = h.finalize();
        let mut s = String::with_capacity(16);
        for b in &digest[..8] {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// Look up a (name, args) pair. Returns:
    ///   * `Skip` if dedup is disabled/the tool is impure/the tool is noisy.
    ///   * `Hit(result)` if an identical OR semantically-similar entry is
    ///     present and not expired.
    ///   * `SoftWarn` in soft mode when we'd have hit, so the caller still
    ///     executes but can surface a warning.
    ///   * `Miss` otherwise — caller should execute, then call `record`.
    pub fn lookup(&mut self, name: &str, args: &Value) -> DedupOutcome {
        if self.disabled {
            return DedupOutcome::Skip;
        }
        if self.noisy.contains(name) {
            return DedupOutcome::Skip;
        }
        // `bash` is impure in general but the model loves to spam
        // identical read-only commands (`ls`, `cat`, `pwd`, `find`).
        // Treat bash as pure only if the command is a read-only one-liner.
        if !PURE_TOOLS.contains(name) && !(name == "bash" && bash_is_read_only(args)) {
            return DedupOutcome::Skip;
        }
        let now = Instant::now();
        self.purge_expired(now);

        let args_norm = canonical_json(args);
        let h = Self::hash(name, &args_norm);

        // Exact match first.
        if let Some(entry) = self.recent.iter().rev().find(|e| e.hash == h && e.name == name) {
            if let Some(cached) = entry.result.clone() {
                self.hits += 1;
                if self.soft {
                    self.soft_warnings += 1;
                    return DedupOutcome::SoftWarn(format!(
                        "soft-dedup: identical call to `{name}` already executed this turn"
                    ));
                }
                return DedupOutcome::Hit(mark_cached(cached));
            }
        }

        // Semantic similarity over same tool name.
        let mut best: Option<(f64, &Entry)> = None;
        for entry in self.recent.iter().rev() {
            if entry.name != name {
                continue;
            }
            let sim = jaccard_similarity(&args_norm, &entry.args_norm);
            if best.map(|(b, _)| sim > b).unwrap_or(true) {
                best = Some((sim, entry));
            }
        }
        if let Some((sim, entry)) = best {
            if sim >= self.similarity_threshold {
                if let Some(cached) = entry.result.clone() {
                    self.hits += 1;
                    if self.soft {
                        self.soft_warnings += 1;
                        return DedupOutcome::SoftWarn(format!(
                            "soft-dedup: `{name}` call is {:.0}% similar to a recent one",
                            sim * 100.0
                        ));
                    }
                    return DedupOutcome::Hit(mark_cached(cached));
                }
            }
        }

        self.misses += 1;
        DedupOutcome::Miss
    }

    /// Record the result of a just-executed call.
    pub fn record(&mut self, name: &str, args: &Value, result: &Value) {
        if self.disabled {
            return;
        }
        if !PURE_TOOLS.contains(name) && !(name == "bash" && bash_is_read_only(args)) {
            return;
        }
        if self.noisy.contains(name) {
            return;
        }
        // Don't cache errors — the model should be allowed to retry.
        if result.get("error").is_some() {
            return;
        }
        let args_norm = canonical_json(args);
        let h = Self::hash(name, &args_norm);
        // Move-to-front: drop existing entry with same hash, push fresh.
        self.recent.retain(|e| e.hash != h);
        self.recent.push_back(Entry {
            hash: h,
            name: name.to_string(),
            args_norm,
            result: Some(result.clone()),
            ts: Instant::now(),
            ttl: self.ttl_for(name),
        });
        while self.recent.len() > self.capacity {
            self.recent.pop_front();
        }
    }

    /// Backward-compatible boolean form retained for existing callers.
    pub fn is_duplicate(&mut self, name: &str, args_json: &str) -> bool {
        let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
        matches!(self.lookup(name, &args), DedupOutcome::Hit(_))
    }

    pub fn reset(&mut self) {
        self.recent.clear();
        self.hits = 0;
        self.misses = 0;
        self.soft_warnings = 0;
    }

    pub fn stats(&self) -> DedupStats {
        DedupStats {
            hits: self.hits,
            misses: self.misses,
            soft_warnings: self.soft_warnings,
            window: self.capacity,
        }
    }
}

impl Default for ToolDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DedupStats {
    pub hits: u64,
    pub misses: u64,
    pub soft_warnings: u64,
    pub window: usize,
}

/// Mark a cached result so the model sees it's a hit.
/// Heuristic: is this `bash` command read-only?
///
/// Returns true only if the command is built from read-only utilities
/// (`ls`, `cat`, `pwd`, `find`, `grep`, `rg`, `echo`, `stat`, `wc`,
/// `head`, `tail`, `file`, `which`, `du`, `df`) AND contains no
/// redirection (`>`, `>>`), no obvious mutators (`rm`, `mv`, `cp`,
/// `mkdir`, `touch`, `chmod`, `chown`, `rmdir`), and no shell-state
/// changes (`cd`, `export`, `source`). The command can chain with `&&`
/// / `||` / `|` / `;` — each segment is checked.
pub fn bash_is_read_only(args: &Value) -> bool {
    let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    if cmd.contains('>') {
        return false;
    }
    const READ_ONLY: &[&str] = &[
        "ls", "cat", "pwd", "find", "grep", "rg", "echo", "stat", "wc",
        "head", "tail", "file", "which", "type", "du", "df", "tree",
        "sort", "uniq", "awk", "sed", "cut", "tr", "diff", "git",
        "printenv", "env", "uname", "whoami", "date", "hostname",
        "true", "false", "test", "[",
    ];
    const FORBID: &[&str] = &[
        "rm", "mv", "cp", "mkdir", "rmdir", "touch", "chmod", "chown",
        "ln", "install", "dd", "sync", "kill", "killall", "pkill",
        "cd", "export", "unset", "source", ".", "exec", "eval",
        "tee", "shred",
    ];
    for segment in cmd.split(|c: char| matches!(c, '|' | ';' | '&')) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        // first word of segment
        let head = trimmed.split_whitespace().next().unwrap_or("");
        // strip a leading subshell `(` if any
        let head = head.trim_start_matches('(').trim_start_matches('$');
        // For chained `&&` we just split on `&` which leaves empty
        // segments between `&&`; the empty-check above handles them.
        if FORBID.contains(&head) {
            return false;
        }
        // Special-case `sed` and `awk`: read-only unless `-i` is passed.
        if (head == "sed" || head == "awk") && trimmed.contains("-i") {
            return false;
        }
        // `git` is read-only for status/diff/log/show but mutating for
        // add/commit/checkout/rebase/etc. Be conservative.
        if head == "git" {
            let sub = trimmed.split_whitespace().nth(1).unwrap_or("");
            const GIT_READONLY: &[&str] = &[
                "status", "diff", "log", "show", "branch", "remote",
                "blame", "tag", "config", "ls-files", "ls-tree",
                "rev-parse", "describe", "shortlog", "stash",
                // additional read-only inspection subcommands the model
                // reaches for when investigating history / refs / lost
                // commits — without these, repeat calls bypass dedup.
                "reflog", "cherry", "for-each-ref", "cat-file",
                "rev-list", "name-rev", "whatchanged", "fsck",
                "verify-commit", "verify-tag", "count-objects",
                "ls-remote", "show-ref", "symbolic-ref",
                "check-ignore", "check-attr", "grep",
            ];
            if !GIT_READONLY.contains(&sub) {
                return false;
            }
        }
        if !READ_ONLY.contains(&head) {
            return false;
        }
    }
    true
}

fn mark_cached(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        if let Some(s) = obj.get_mut("result").and_then(|r| r.as_str().map(String::from)) {
            obj.insert(
                "result".to_string(),
                Value::String(format!(
                    "[cached - identical call already executed this turn]\n{s}"
                )),
            );
        }
        obj.insert("_dedup_cached".to_string(), Value::Bool(true));
    }
    v
}

/// Canonical JSON form: keys sorted recursively. Stable across argument-order
/// variations so `{a:1,b:2}` and `{b:2,a:1}` hash identically.
fn canonical_json(v: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(k);
                    out.push_str("\":");
                    walk(&map[*k], out);
                }
                out.push('}');
            }
            Value::Array(arr) => {
                out.push('[');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    walk(item, out);
                }
                out.push(']');
            }
            _ => out.push_str(&v.to_string()),
        }
    }
    let mut s = String::new();
    walk(v, &mut s);
    s
}

/// Jaccard similarity over whitespace/punctuation tokens. Returns 0..=1.
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    fn tokens(s: &str) -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect()
    }
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_duplicate_is_hit() {
        let mut d = ToolDedup::new();
        let args = json!({"path": "src/main.rs"});
        assert!(matches!(d.lookup("read_file", &args), DedupOutcome::Miss));
        d.record("read_file", &args, &json!({"result": "fn main(){}"}));
        let hit = d.lookup("read_file", &args);
        assert!(matches!(hit, DedupOutcome::Hit(_)));
    }

    #[test]
    fn key_order_doesnt_matter() {
        let mut d = ToolDedup::new();
        d.record(
            "search",
            &json!({"pattern": "foo", "path": "."}),
            &json!({"result": "x"}),
        );
        assert!(matches!(
            d.lookup("search", &json!({"path": ".", "pattern": "foo"})),
            DedupOutcome::Hit(_)
        ));
    }

    #[test]
    fn impure_tool_skipped() {
        let mut d = ToolDedup::new();
        let r = d.lookup("write_file", &json!({"path": "x"}));
        assert!(matches!(r, DedupOutcome::Skip));
    }

    #[test]
    fn errors_not_cached() {
        let mut d = ToolDedup::new();
        let args = json!({"path": "a"});
        d.record("read_file", &args, &json!({"error": "boom"}));
        assert!(matches!(d.lookup("read_file", &args), DedupOutcome::Miss));
    }
}

#[cfg(test)]
mod bash_readonly_tests {
    use super::*;
    use serde_json::json;

    fn b(cmd: &str) -> Value {
        json!({"command": cmd})
    }

    #[test]
    fn read_only_commands_are_pure() {
        assert!(bash_is_read_only(&b("ls -la /tmp")));
        assert!(bash_is_read_only(&b("pwd && ls")));
        assert!(bash_is_read_only(&b("find . -name '*.rs' | head -10")));
        assert!(bash_is_read_only(&b("grep -r foo")));
        assert!(bash_is_read_only(&b("cat README.md")));
        assert!(bash_is_read_only(&b("git status")));
        assert!(bash_is_read_only(&b("git diff --stat")));
        assert!(bash_is_read_only(&b("echo hello")));
    }

    #[test]
    fn mutating_commands_are_impure() {
        assert!(!bash_is_read_only(&b("rm -rf foo")));
        assert!(!bash_is_read_only(&b("mkdir new")));
        assert!(!bash_is_read_only(&b("ls > out.txt")));
        assert!(!bash_is_read_only(&b("cd src && ls")));
        assert!(!bash_is_read_only(&b("git commit -m x")));
        assert!(!bash_is_read_only(&b("ls | tee out.txt")));
        assert!(!bash_is_read_only(&b("sed -i s/a/b/ file")));
    }

    #[test]
    fn identical_readonly_bash_dedups() {
        let mut d = ToolDedup::new();
        let args = b("ls -la /tmp");
        let r = json!({"result": "hello"});
        d.record("bash", &args, &r);
        match d.lookup("bash", &args) {
            DedupOutcome::Hit(_) => {}
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn mutating_bash_skips_dedup() {
        let mut d = ToolDedup::new();
        let args = b("rm -rf /tmp/x");
        d.record("bash", &args, &json!({"result": "ok"}));
        assert!(matches!(d.lookup("bash", &args), DedupOutcome::Skip));
    }
}
