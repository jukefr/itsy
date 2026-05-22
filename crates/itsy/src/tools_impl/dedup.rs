//! Tool-call deduplication.
//!
//! Behavioural 1:1 with upstream `src/tools/dedup.js` at
//! `1db07104af9df709cec086dffdfc4bf65cceae8d`. Small models often loop
//! — calling `read_file` on the same file twice in a row, or `search`
//! for the same pattern. Dedup short-circuits identical consecutive
//! tool calls within a sliding window, returning the cached result
//! instead of re-executing. Only applies to read-only / pure tools.
//!
//! Configuration:
//!   `features.dedup_enabled` — enable (default true)
//!   `features.dedup_window`  — number of recent calls considered (default 5)
//!
//! ### Upstream vs port — INTENTIONAL deviations
//!
//! 1. **Return type `Option<Value>` instead of `null | Value`** — direct
//!    Rust idiom for "maybe a cached result". Behaviour identical.
//! 2. **SHA256 truncated to 16 hex chars** instead of SHA1. Cache keys
//!    never leave the process so collision properties are equivalent;
//!    `sha2` is already a workspace dep, `sha1` isn't.
//! 3. **`Arc<Mutex<ToolDedup>>` in the session** instead of JS's module
//!    singleton (`getDedup()` / `resetDedup()`). Rust ownership requires
//!    explicit sharing; behaviour identical (one instance per session).
//! 4. **`bash_is_read_only()` free helper** — Rust-only utility used by
//!    the contract gate in `bin/itsy.rs` to classify mutating-vs-readonly
//!    bash commands. Does NOT participate in dedup lookup/record (bash
//!    is treated as impure exactly like upstream — the bench evidence
//!    showed that caching read-only bash makes spam *cheaper* without
//!    actually preventing the spiral, so it was not a real improvement).
//! 5. **Unit tests** — JS has none in-file. Pure regression coverage.
//!
//! Previous Rust additions that have been REMOVED because they made
//! behaviour worse than upstream:
//!   * TTL expiry — forces re-execution that JS would cache.
//!   * Jaccard semantic similarity — false-positive cache hits for
//!     non-identical calls.
//!   * NOISY_TOOLS bypass (`list_files`) — misses real cache hits.
//!   * Routing read-only bash through dedup — caching made spam
//!     cheaper without preventing the actual spiral; spiral defense
//!     belongs in the agent loop's `BREAK_ON_REPEAT` path.
//!   * Soft-warn mode — kept the warning but dropped the toggle.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Tools considered safe to dedup. Anything that mutates state
/// (write_file, patch, bash, mcp__*) is excluded — even if "the same"
/// command was just run, the world may have changed.
///
/// MATCHES upstream src/tools/dedup.js PURE_TOOLS exactly.
pub static PURE_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
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
    ]
    .iter()
    .copied()
    .collect()
});

#[derive(Debug, Clone)]
struct Entry {
    hash: String,
    #[allow(dead_code)]
    name: String,
    result: Value,
    #[allow(dead_code)]
    ts: u64, // ms since UNIX epoch — mirrors JS Date.now(); not read, kept for parity
}

#[derive(Debug, Clone, Copy)]
pub struct DedupStats {
    pub hits: u64,
    pub misses: u64,
    pub window_size: usize,
}

/// Per-session tool-call dedup. Behavioural 1:1 with upstream
/// `class ToolDedup`.
#[derive(Debug)]
pub struct ToolDedup {
    window_size: usize,
    disabled: bool,
    /// Recent entries, oldest first. `Vec` mirrors JS Array semantics
    /// including `.shift()` on overflow; window is tiny so O(n) shift
    /// is fine.
    recent: Vec<Entry>,
    pub hits: u64,
    pub misses: u64,
}

impl ToolDedup {
    /// Construct with config from `settings`. Mirrors JS `constructor`.
    pub fn new() -> Self {
        let s = crate::settings::get();
        Self {
            window_size: s.dedup_window.max(1),
            disabled: !s.dedup_enabled,
            recent: Vec::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Compute a stable hash for (name, args). Sorted top-level JSON
    /// keys mirror JS `JSON.stringify(args, Object.keys(args).sort())`.
    fn hash(name: &str, args: &Value) -> String {
        let norm = sorted_keys_json(args);
        let mut h = Sha256::new();
        h.update(name.as_bytes());
        h.update(b"|");
        h.update(norm.as_bytes());
        let hex = format!("{:x}", h.finalize());
        hex.chars().take(16).collect()
    }

    /// Check whether (name, args) was just executed. Returns the cached
    /// result or `None`. Only deduplicates pure tools.
    ///
    /// Mirrors upstream `lookup(name, args)` behaviour:
    ///   - if disabled → null
    ///   - if !PURE_TOOLS → null
    ///   - exact hash match in recent window → cloned result
    ///   - otherwise → null
    pub fn lookup(&mut self, name: &str, args: &Value) -> Option<Value> {
        if self.disabled {
            return None;
        }
        if !PURE_TOOLS.contains(name) {
            return None;
        }
        let h = Self::hash(name, args);
        for entry in self.recent.iter().rev() {
            if entry.hash == h {
                self.hits += 1;
                // Shallow copy so callers can't mutate the cached entry.
                return Some(entry.result.clone());
            }
        }
        self.misses += 1;
        None
    }

    /// Record (name, args, result) of a just-executed call. Mirrors
    /// upstream `record()` behaviour:
    ///   - if disabled → no-op
    ///   - if !PURE_TOOLS → no-op
    ///   - if result has `error` → no-op (errors aren't cached)
    ///   - move-to-front then trim to window_size
    pub fn record(&mut self, name: &str, args: &Value, result: &Value) {
        if self.disabled {
            return;
        }
        if !PURE_TOOLS.contains(name) {
            return;
        }
        if result.get("error").is_some() {
            return;
        }
        let h = Self::hash(name, args);
        // Move-to-front: drop existing entry with same hash, push fresh.
        self.recent.retain(|r| r.hash != h);
        self.recent.push(Entry {
            hash: h,
            name: name.to_string(),
            result: result.clone(),
            ts: now_ms(),
        });
        while self.recent.len() > self.window_size {
            self.recent.remove(0); // mirrors JS Array.shift()
        }
    }

    /// Reset all state. Call between agent runs.
    pub fn reset(&mut self) {
        self.recent.clear();
        self.hits = 0;
        self.misses = 0;
    }

    /// Snapshot stats for logging.
    pub fn stats(&self) -> DedupStats {
        DedupStats {
            hits: self.hits,
            misses: self.misses,
            window_size: self.window_size,
        }
    }
}

impl Default for ToolDedup {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap a cached result with the `[cached]` marker the model sees.
/// Mirrors upstream `static markCached(result)`.
pub fn mark_cached(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        if let Some(s) = obj.get("result").and_then(|x| x.as_str()) {
            let new = format!("[cached - identical call already executed this turn]\n{s}");
            obj.insert("result".to_string(), Value::String(new));
        }
        obj.insert("_dedup_cached".to_string(), Value::Bool(true));
    }
    v
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable JSON with sorted top-level keys. Equivalent to
/// `JSON.stringify(args || {}, Object.keys(args || {}).sort())`.
/// Nested object key order is whatever serde_json emits — JS's
/// stringify with key array only sorts the top level too.
fn sorted_keys_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&k.replace('"', "\\\""));
                out.push_str("\":");
                out.push_str(&serde_json::to_string(&map[*k]).unwrap_or_default());
            }
            out.push('}');
            out
        }
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

// ─── Rust-only utility (not used by dedup) ────────────────────────────
// Used by the contract gate in `bin/itsy.rs` to classify bash commands
// as read-only or mutating. Doesn't participate in dedup behaviour —
// bash is still impure for lookup/record exactly like upstream. Lives
// here because it answers the same "is this a side-effect-free call?"
// question.

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
        let head = trimmed.split_whitespace().next().unwrap_or("");
        let head = head.trim_start_matches('(').trim_start_matches('$');
        if head == "cd" {
            let rest: Vec<&str> = trimmed.split_whitespace().collect();
            let cd_safe = rest.len() == 2
                && !rest[1].contains('*')
                && !rest[1].contains('$')
                && !rest[1].contains('`');
            if cd_safe {
                continue;
            }
            return false;
        }
        if FORBID.contains(&head) {
            return false;
        }
        if (head == "sed" || head == "awk") && trimmed.contains("-i") {
            return false;
        }
        if head == "git" {
            let sub = trimmed.split_whitespace().nth(1).unwrap_or("");
            const GIT_READONLY: &[&str] = &[
                "status", "diff", "log", "show", "branch", "remote",
                "blame", "tag", "config", "ls-files", "ls-tree",
                "rev-parse", "describe", "shortlog", "stash",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d() -> ToolDedup {
        ToolDedup {
            window_size: 5,
            disabled: false,
            recent: Vec::new(),
            hits: 0,
            misses: 0,
        }
    }

    #[test]
    fn exact_duplicate_is_hit() {
        let mut x = d();
        let r = json!({"result": "OK"});
        assert!(x.lookup("read_file", &json!({"path": "/a"})).is_none());
        x.record("read_file", &json!({"path": "/a"}), &r);
        assert_eq!(x.lookup("read_file", &json!({"path": "/a"})), Some(r));
    }

    #[test]
    fn key_order_doesnt_matter() {
        let mut x = d();
        let r = json!({"result": "OK"});
        x.record("read_file", &json!({"path": "/a", "n": 1}), &r);
        assert!(x.lookup("read_file", &json!({"n": 1, "path": "/a"})).is_some());
    }

    #[test]
    fn impure_tool_skipped() {
        let mut x = d();
        let r = json!({"result": "wrote"});
        x.record("write_file", &json!({"path": "/a"}), &r);
        assert!(x.lookup("write_file", &json!({"path": "/a"})).is_none());
    }

    #[test]
    fn errors_not_cached() {
        let mut x = d();
        x.record(
            "read_file",
            &json!({"path": "/a"}),
            &json!({"error": "nope"}),
        );
        assert!(x.lookup("read_file", &json!({"path": "/a"})).is_none());
    }

    #[test]
    fn bash_is_impure_for_dedup() {
        // Critical behavioural parity check: even read-only bash
        // calls do NOT participate in dedup, exactly like upstream.
        let mut x = d();
        x.record(
            "bash",
            &json!({"command": "ls /tmp"}),
            &json!({"result": "a b c"}),
        );
        assert!(x.lookup("bash", &json!({"command": "ls /tmp"})).is_none());
    }

    #[test]
    fn window_size_evicts_oldest() {
        let mut x = d();
        for i in 0..7 {
            x.record(
                "read_file",
                &json!({"path": format!("/{i}")}),
                &json!({"result": format!("c{i}")}),
            );
        }
        assert!(x.lookup("read_file", &json!({"path": "/0"})).is_none());
        assert!(x.lookup("read_file", &json!({"path": "/1"})).is_none());
        assert!(x.lookup("read_file", &json!({"path": "/6"})).is_some());
    }

    fn b(cmd: &str) -> Value {
        json!({ "command": cmd })
    }

    #[test]
    fn read_only_commands_are_pure() {
        assert!(bash_is_read_only(&b("ls /tmp")));
        assert!(bash_is_read_only(&b("cat /etc/passwd")));
        assert!(bash_is_read_only(&b("git status")));
        assert!(bash_is_read_only(&b("git log --oneline")));
    }

    #[test]
    fn mutating_commands_are_impure() {
        assert!(!bash_is_read_only(&b("rm -rf /tmp/x")));
        assert!(!bash_is_read_only(&b("echo hi > /tmp/x")));
        assert!(!bash_is_read_only(&b("git commit -m foo")));
        assert!(!bash_is_read_only(&b("sed -i s/a/b/ /tmp/x")));
    }

    #[test]
    fn cd_then_readonly_is_pure() {
        assert!(bash_is_read_only(&b("cd /app && ls")));
        assert!(bash_is_read_only(&b("cd /app && git log -5")));
    }

    #[test]
    fn cd_with_glob_or_flags_is_impure() {
        assert!(!bash_is_read_only(&b("cd /app/*/src && ls")));
        assert!(!bash_is_read_only(&b("cd $HOME && ls")));
    }

    #[test]
    fn cd_then_mutating_is_impure() {
        assert!(!bash_is_read_only(&b("cd /app && rm foo")));
    }
}
