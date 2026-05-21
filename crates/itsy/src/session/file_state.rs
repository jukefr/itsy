//! File State Tracker (Feature #16: diff-based context).
//!
//! Tracks the last-known content of files the model has read this session.
//! When the model reads a file it's already seen, we can return a compact diff
//! instead of the full content — saving significant context on multi-edit
//! sessions.
//!
//! Example: model reads `foo.py` (200 lines), edits it, reads it again.
//!   Without this: 200 lines sent twice = 400 lines of context.
//!   With this:    200 lines + 12-line diff = 212 lines of context.
//!
//! The diff is a (simplified) unified diff. We deliberately keep the diff
//! SMALL: only changed hunks with N lines of context. If the diff is larger
//! than `MAX_RATIO` of the full content we send the full content anyway.
//!
//! Configuration (env vars, mirroring the upstream JS implementation):
//!   `ITSY_DIFF_CONTEXT=true`       — enable diff mode (default: off, opt-in)
//!   `ITSY_DIFF_CONTEXT_LINES=3`    — lines of context around each hunk
//!   `ITSY_DIFF_MAX_RATIO=0.7`      — if diff > X% of full content, send full
//!   `ITSY_DIFF_TTL_MINUTES=30`     — forget fingerprints after N minutes
//!
//! The TTL is a Rust-side addition: agent sessions can be long-lived and a
//! stale fingerprint risks emitting a misleading "unchanged" against a file
//! that was edited out-of-band. After the TTL we drop the entry and behave as
//! if the file had never been seen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

const DEFAULT_CONTEXT_LINES: usize = 3;
const DEFAULT_MAX_RATIO: f64 = 0.7;
const DEFAULT_TTL_MINUTES: u64 = 30;

/// Guard: skip diff for very large files to avoid O(n*m) DP memory use.
const MAX_DIFF_LINES: usize = 2000;

/// Outcome of recording a read. Mirrors the JS `{ mode: ... }` payload.
pub enum RecordResult {
    /// File has not changed since last seen — caller can skip resending content.
    Unchanged,
    /// File changed: caller should emit `diff` instead of the full content.
    /// `full_length` is the total line count of the new content (for headers).
    Diff { diff: String, full_length: usize },
    /// Caller should send the full content (first read, diff too large, disabled, or TTL expired).
    Full,
}

#[derive(Clone)]
struct Entry {
    content: String,
    hash: String,
    read_count: u64,
    recorded_at: Instant,
}

pub struct FileStateTracker {
    known: Mutex<HashMap<PathBuf, Entry>>,
    disabled: bool,
    context_lines: usize,
    max_ratio: f64,
    ttl: Duration,
}

impl FileStateTracker {
    fn new() -> Self {
        let disabled = std::env::var("ITSY_DIFF_CONTEXT").ok().as_deref() != Some("true");
        let context_lines = std::env::var("ITSY_DIFF_CONTEXT_LINES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_CONTEXT_LINES);
        let max_ratio = std::env::var("ITSY_DIFF_MAX_RATIO")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_RATIO);
        let ttl_minutes = std::env::var("ITSY_DIFF_TTL_MINUTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TTL_MINUTES);
        Self {
            known: Mutex::new(HashMap::new()),
            disabled,
            context_lines,
            max_ratio,
            ttl: Duration::from_secs(ttl_minutes * 60),
        }
    }

    /// Record that the model has read a file. See [`RecordResult`].
    pub fn record(&self, path: &Path, content: &str) -> RecordResult {
        let hash = hash_content(content);
        let mut g = self.known.lock();

        if self.disabled {
            // Still track state so writes stay accurate.
            g.insert(
                path.to_path_buf(),
                Entry { content: content.to_string(), hash, read_count: 1, recorded_at: Instant::now() },
            );
            return RecordResult::Full;
        }

        // Expire stale entries.
        let now = Instant::now();
        let expired = g
            .get(path)
            .map(|e| now.saturating_duration_since(e.recorded_at) > self.ttl)
            .unwrap_or(false);
        if expired {
            g.remove(path);
        }

        match g.get_mut(path) {
            None => {
                // First read (or expired) — full content.
                g.insert(
                    path.to_path_buf(),
                    Entry { content: content.to_string(), hash, read_count: 1, recorded_at: now },
                );
                RecordResult::Full
            }
            Some(prior) if prior.hash == hash => {
                prior.read_count += 1;
                prior.recorded_at = now;
                RecordResult::Unchanged
            }
            Some(prior) => {
                // Content changed — compute diff.
                let new_line_count = content.split('\n').count();
                let prior_read_count = prior.read_count;
                let prior_content = prior.content.clone();

                // Guard: skip diff for very large files.
                if new_line_count > MAX_DIFF_LINES {
                    g.insert(
                        path.to_path_buf(),
                        Entry {
                            content: content.to_string(),
                            hash,
                            read_count: prior_read_count + 1,
                            recorded_at: now,
                        },
                    );
                    return RecordResult::Full;
                }

                let diff = compute_unified_diff(&prior_content, content, path, self.context_lines);
                let ratio = if !content.is_empty() {
                    diff.len() as f64 / content.len() as f64
                } else {
                    1.0
                };

                g.insert(
                    path.to_path_buf(),
                    Entry {
                        content: content.to_string(),
                        hash,
                        read_count: prior_read_count + 1,
                        recorded_at: now,
                    },
                );

                if diff.is_empty() || ratio > self.max_ratio {
                    RecordResult::Full
                } else {
                    RecordResult::Diff { diff, full_length: new_line_count }
                }
            }
        }
    }

    /// Record a write so the tracker knows the new state.
    pub fn record_write(&self, path: &Path, content: &str) {
        let hash = hash_content(content);
        let mut g = self.known.lock();
        let read_count = g.get(path).map(|e| e.read_count).unwrap_or(0);
        g.insert(
            path.to_path_buf(),
            Entry { content: content.to_string(), hash, read_count, recorded_at: Instant::now() },
        );
    }

    /// Drop tracked state for `path` (e.g. after deletion).
    pub fn forget(&self, path: &Path) {
        self.known.lock().remove(path);
    }

    /// Clear all state — call between agent runs.
    pub fn reset(&self) {
        self.known.lock().clear();
    }

    /// How many files are being tracked.
    pub fn size(&self) -> usize {
        self.known.lock().len()
    }
}

// ─── Hash ──────────────────────────────────────────────────────────────────

/// Short content fingerprint. JS uses md5[:8]; we use sha256[:16] — both are
/// only used for change detection (not security), and a longer prefix lowers
/// the collision rate at negligible cost.
fn hash_content(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let hex = format!("{:x}", h.finalize());
    hex[..16].to_string()
}

// ─── Unified diff ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Del,
    Ins,
}

struct EditOp<'a> {
    op: Op,
    old_idx: usize, // index into `a` for Eq/Del; current `i` for Ins
    new_idx: usize, // index into `b` for Eq/Ins; current `j` for Del
    text: &'a str,
}

struct Hunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    lines: Vec<String>,
}

/// Compute a simplified unified diff between two texts (line-level LCS).
/// Returns an empty string if no changes.
pub(crate) fn compute_unified_diff(old: &str, new: &str, path: &Path, context_lines: usize) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    if old_lines == new_lines {
        return String::new();
    }

    let ops = lcs_edit_script(&old_lines, &new_lines);
    let hunks = group_hunks(&ops, context_lines);
    if hunks.is_empty() {
        return String::new();
    }

    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or(""));

    let mut out = String::new();
    out.push_str(&format!("--- {name} (before)\n"));
    out.push_str(&format!("+++ {name} (after)\n"));
    for h in &hunks {
        out.push_str(&format_hunk(h));
    }
    out
}

/// Standard O(n*m) LCS DP — adequate for file-size diffs.
fn lcs_edit_script<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<EditOp<'a>> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS length of a[i..] and b[j..]
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for x in (0..n).rev() {
        for y in (0..m).rev() {
            if a[x] == b[y] {
                dp[x][y] = dp[x + 1][y + 1] + 1;
            } else {
                dp[x][y] = dp[x + 1][y].max(dp[x][y + 1]);
            }
        }
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(EditOp { op: Op::Eq, old_idx: i, new_idx: j, text: a[i] });
            i += 1;
            j += 1;
        } else if j < m && (i >= n || dp[i][j + 1] >= dp[i + 1][j]) {
            ops.push(EditOp { op: Op::Ins, old_idx: i, new_idx: j, text: b[j] });
            j += 1;
        } else {
            ops.push(EditOp { op: Op::Del, old_idx: i, new_idx: j, text: a[i] });
            i += 1;
        }
    }
    ops
}

/// Group edit operations into hunks with `context_lines` of surrounding context.
fn group_hunks(ops: &[EditOp<'_>], context_lines: usize) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut i = 0usize;
    while i < ops.len() {
        if ops[i].op == Op::Eq {
            i += 1;
            continue;
        }
        // Found start of a change region. Expand `end` to include changes plus
        // context; merge adjacent changes within `context_lines * 2`.
        let start = i;
        let mut end = i;
        while end < ops.len() && (ops[end].op != Op::Eq || end - i < context_lines) {
            end += 1;
        }
        loop {
            // Find next change at or after `end`.
            let next_change = ops[end..].iter().position(|o| o.op != Op::Eq);
            match next_change {
                Some(d) if d <= context_lines * 2 => {
                    end += d + 1;
                    while end < ops.len() && (ops[end].op != Op::Eq || end - start < context_lines) {
                        end += 1;
                    }
                }
                _ => break,
            }
        }

        let lo = start.saturating_sub(context_lines);
        let hi = (end + context_lines).min(ops.len());
        if let Some(h) = build_hunk(&ops[lo..hi]) {
            hunks.push(h);
        }
        i = end + context_lines;
    }
    hunks
}

fn build_hunk(ops: &[EditOp<'_>]) -> Option<Hunk> {
    if ops.is_empty() {
        return None;
    }
    let mut old_start: Option<usize> = None;
    let mut new_start: Option<usize> = None;
    let mut old_len = 0usize;
    let mut new_len = 0usize;
    let mut lines = Vec::with_capacity(ops.len());

    for op in ops {
        match op.op {
            Op::Eq => {
                if old_start.is_none() {
                    old_start = Some(op.old_idx + 1);
                    new_start = Some(op.new_idx + 1);
                }
                lines.push(format!(" {}", op.text));
                old_len += 1;
                new_len += 1;
            }
            Op::Del => {
                if old_start.is_none() {
                    old_start = Some(op.old_idx + 1);
                    new_start = Some(op.new_idx + 1);
                }
                lines.push(format!("-{}", op.text));
                old_len += 1;
            }
            Op::Ins => {
                if old_start.is_none() {
                    old_start = Some(op.old_idx + 1);
                    new_start = Some(op.new_idx + 1);
                }
                lines.push(format!("+{}", op.text));
                new_len += 1;
            }
        }
    }

    Some(Hunk {
        old_start: old_start?,
        old_len,
        new_start: new_start?,
        new_len,
        lines,
    })
}

fn format_hunk(h: &Hunk) -> String {
    let mut s = format!(
        "@@ -{},{} +{},{} @@\n",
        h.old_start, h.old_len, h.new_start, h.new_len
    );
    s.push_str(&h.lines.join("\n"));
    s.push('\n');
    s
}

// ─── Singleton ─────────────────────────────────────────────────────────────

static INSTANCE: OnceLock<FileStateTracker> = OnceLock::new();

pub fn get_file_state_tracker() -> &'static FileStateTracker {
    INSTANCE.get_or_init(FileStateTracker::new)
}

/// Reset the singleton's in-memory state. Useful between agent runs in tests.
pub fn reset_file_state_tracker() {
    if let Some(t) = INSTANCE.get() {
        t.reset();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_enabled() -> FileStateTracker {
        FileStateTracker {
            known: Mutex::new(HashMap::new()),
            disabled: false,
            context_lines: DEFAULT_CONTEXT_LINES,
            max_ratio: DEFAULT_MAX_RATIO,
            ttl: Duration::from_secs(DEFAULT_TTL_MINUTES * 60),
        }
    }

    #[test]
    fn first_read_is_full() {
        let t = tracker_enabled();
        let p = PathBuf::from("/x/foo.txt");
        assert!(matches!(t.record(&p, "hello\nworld\n"), RecordResult::Full));
    }

    #[test]
    fn identical_read_is_unchanged() {
        let t = tracker_enabled();
        let p = PathBuf::from("/x/foo.txt");
        let _ = t.record(&p, "a\nb\nc\n");
        assert!(matches!(t.record(&p, "a\nb\nc\n"), RecordResult::Unchanged));
    }

    #[test]
    fn changed_read_produces_diff() {
        // Use a large enough body that the diff stays well under max_ratio (0.7).
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..40 {
            let line = format!("line number {i:02} with some filler text to bulk up the file body\n");
            old.push_str(&line);
            new.push_str(&line);
        }
        // Mutate one line in the middle.
        let mutated_new = new.replace(
            "line number 20 with some filler text to bulk up the file body",
            "line number 20 WAS CHANGED here",
        );
        let t = tracker_enabled();
        let p = PathBuf::from("/x/foo.txt");
        let _ = t.record(&p, &old);
        match t.record(&p, &mutated_new) {
            RecordResult::Diff { diff, full_length } => {
                assert!(diff.contains("@@"), "missing hunk header: {diff}");
                assert!(diff.contains("-line number 20"), "missing deletion: {diff}");
                assert!(diff.contains("+line number 20 WAS CHANGED"), "missing insertion: {diff}");
                assert_eq!(full_length, mutated_new.split('\n').count());
            }
            _ => panic!("expected Diff variant"),
        }
    }

    #[test]
    fn record_write_updates_state() {
        let t = tracker_enabled();
        let p = PathBuf::from("/x/foo.txt");
        t.record_write(&p, "a\nb\n");
        assert!(matches!(t.record(&p, "a\nb\n"), RecordResult::Unchanged));
    }

    #[test]
    fn forget_drops_entry() {
        let t = tracker_enabled();
        let p = PathBuf::from("/x/foo.txt");
        let _ = t.record(&p, "a\n");
        t.forget(&p);
        assert!(matches!(t.record(&p, "a\n"), RecordResult::Full));
    }

    #[test]
    fn unified_diff_no_changes_is_empty() {
        let s = compute_unified_diff("a\nb\n", "a\nb\n", Path::new("/x/foo"), 3);
        assert!(s.is_empty());
    }

    #[test]
    fn diff_size_threshold_falls_back_to_full() {
        // Tracker with absurdly low max_ratio so any diff exceeds it.
        let t = FileStateTracker {
            known: Mutex::new(HashMap::new()),
            disabled: false,
            context_lines: 3,
            max_ratio: 0.0001,
            ttl: Duration::from_secs(60),
        };
        let p = PathBuf::from("/x/foo.txt");
        let _ = t.record(&p, "a\nb\n");
        assert!(matches!(t.record(&p, "a\nZ\n"), RecordResult::Full));
    }
}
