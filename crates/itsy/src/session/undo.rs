//! Per-edit undo stack.
//!
//! Tracks each file modification and allows reverting specific edits
//! instead of just `git checkout -- .`.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Kind of recorded operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UndoKind {
    Write,
    Patch,
    Delete,
    Rename,
}

/// A single reversible file-edit entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub id: u64,
    pub kind: UndoKind,
    pub path: String,
    /// File contents *before* the edit (None = file did not exist).
    pub before: Option<String>,
    /// File contents *after* the edit (Some for write/patch, None for delete).
    pub after: Option<String>,
    /// For `Patch`: the substring that was replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_str: Option<String>,
    /// For `Patch`: the replacement substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_str: Option<String>,
    /// For `Rename`: the original path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Human-readable description of the change.
    pub description: String,
    /// Unix-millisecond timestamp.
    pub timestamp_ms: i64,
}

/// Result of an `undo_*` operation — used by the CLI command handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoResult {
    pub reverted: Option<String>,
    pub action: String,
    pub error: Option<String>,
}

impl UndoResult {
    fn ok(path: &str, action: impl Into<String>) -> Self {
        Self { reverted: Some(path.to_string()), action: action.into(), error: None }
    }
    fn err(path: &str, msg: impl Into<String>) -> Self {
        Self { reverted: None, action: String::new(), error: Some(format!("Failed to revert {}: {}", path, msg.into())) }
    }
}

/// Lightweight summary entry for `/undo list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoSummary {
    pub id: u64,
    pub kind: UndoKind,
    pub path: String,
    pub description: String,
    /// Age in seconds.
    pub age_secs: i64,
}

pub struct UndoStack {
    stack: VecDeque<UndoEntry>,
    max_size: usize,
    next_id: u64,
    /// Base directory used for resolving relative paths (defaults to cwd).
    base_dir: PathBuf,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::with_capacity(50)
    }

    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            stack: VecDeque::with_capacity(max_size),
            max_size,
            next_id: 1,
            base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn set_base_dir<P: Into<PathBuf>>(&mut self, dir: P) {
        self.base_dir = dir.into();
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    fn push_entry(&mut self, entry: UndoEntry) -> u64 {
        let id = entry.id;
        self.stack.push_back(entry);
        while self.stack.len() > self.max_size {
            self.stack.pop_front();
        }
        id
    }

    /// Record a whole-file write. `before` is `None` for newly-created files.
    pub fn record_write(
        &mut self,
        file_path: &str,
        before: Option<String>,
        after: String,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let description = if before.is_none() {
            format!("created {}", file_path)
        } else {
            format!("wrote {}", file_path)
        };
        let entry = UndoEntry {
            id,
            kind: UndoKind::Write,
            path: file_path.to_string(),
            before,
            after: Some(after),
            old_str: None,
            new_str: None,
            from: None,
            description,
            timestamp_ms: Utc::now().timestamp_millis(),
        };
        self.push_entry(entry)
    }

    /// Record a string-replacement patch. `full_before` is the entire file
    /// contents prior to the patch — used to restore on undo.
    pub fn record_patch(
        &mut self,
        file_path: &str,
        old_str: &str,
        new_str: &str,
        full_before: String,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let entry = UndoEntry {
            id,
            kind: UndoKind::Patch,
            path: file_path.to_string(),
            before: Some(full_before),
            after: None,
            old_str: Some(old_str.to_string()),
            new_str: Some(new_str.to_string()),
            from: None,
            description: format!("patched {}", file_path),
            timestamp_ms: Utc::now().timestamp_millis(),
        };
        self.push_entry(entry)
    }

    /// Record a file deletion.
    pub fn record_delete(&mut self, file_path: &str, before: Option<String>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let entry = UndoEntry {
            id,
            kind: UndoKind::Delete,
            path: file_path.to_string(),
            before,
            after: None,
            old_str: None,
            new_str: None,
            from: None,
            description: format!("deleted {}", file_path),
            timestamp_ms: Utc::now().timestamp_millis(),
        };
        self.push_entry(entry)
    }

    /// Record a rename/move.
    pub fn record_rename(&mut self, from: &str, to: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let entry = UndoEntry {
            id,
            kind: UndoKind::Rename,
            path: to.to_string(),
            before: None,
            after: None,
            old_str: None,
            new_str: None,
            from: Some(from.to_string()),
            description: format!("renamed {} → {}", from, to),
            timestamp_ms: Utc::now().timestamp_millis(),
        };
        self.push_entry(entry)
    }

    /// Undo the most recent edit (LIFO).
    pub fn undo_last(&mut self) -> Option<UndoResult> {
        let entry = self.stack.pop_back()?;
        Some(self.revert(&entry))
    }

    /// Undo a specific edit by id.
    pub fn undo_by_id(&mut self, id: u64) -> Option<UndoResult> {
        let idx = self.stack.iter().position(|e| e.id == id)?;
        let entry = self.stack.remove(idx)?;
        Some(self.revert(&entry))
    }

    /// Return up to `count` most-recent edits (newest first).
    pub fn list(&self, count: usize) -> Vec<UndoSummary> {
        let now = Utc::now().timestamp_millis();
        let n = self.stack.len();
        let take = count.min(n);
        let start = n - take;
        self.stack
            .iter()
            .skip(start)
            .rev()
            .map(|e| UndoSummary {
                id: e.id,
                kind: e.kind,
                path: e.path.clone(),
                description: e.description.clone(),
                age_secs: ((now - e.timestamp_ms) / 1000).max(0),
            })
            .collect()
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    fn revert(&self, entry: &UndoEntry) -> UndoResult {
        match entry.kind {
            UndoKind::Write | UndoKind::Patch => {
                let full = self.resolve(&entry.path);
                match &entry.before {
                    None => {
                        // File was newly created — delete it.
                        if full.exists() {
                            if let Err(e) = fs::remove_file(&full) {
                                return UndoResult::err(&entry.path, e.to_string());
                            }
                        }
                        UndoResult::ok(&entry.path, "deleted (was new file)")
                    }
                    Some(prev) => {
                        if let Some(parent) = full.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if let Err(e) = fs::write(&full, prev) {
                            return UndoResult::err(&entry.path, e.to_string());
                        }
                        UndoResult::ok(&entry.path, "restored previous content")
                    }
                }
            }
            UndoKind::Delete => {
                let full = self.resolve(&entry.path);
                match &entry.before {
                    Some(prev) => {
                        if let Some(parent) = full.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if let Err(e) = fs::write(&full, prev) {
                            return UndoResult::err(&entry.path, e.to_string());
                        }
                        UndoResult::ok(&entry.path, "restored deleted file")
                    }
                    None => UndoResult::err(&entry.path, "no recorded contents to restore"),
                }
            }
            UndoKind::Rename => {
                let to = self.resolve(&entry.path);
                let from = match &entry.from {
                    Some(f) => self.resolve(f),
                    None => return UndoResult::err(&entry.path, "rename missing source path"),
                };
                if let Err(e) = fs::rename(&to, &from) {
                    return UndoResult::err(&entry.path, e.to_string());
                }
                UndoResult::ok(&entry.path, "rename reverted")
            }
        }
    }
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_undo_restores_previous() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("x.txt");
        fs::write(&f, b"hello").unwrap();

        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        s.record_write("x.txt", Some("hello".into()), "world".into());
        fs::write(&f, b"world").unwrap();

        let r = s.undo_last().unwrap();
        assert!(r.error.is_none(), "{:?}", r);
        assert_eq!(fs::read_to_string(&f).unwrap(), "hello");
    }

    #[test]
    fn write_new_file_undo_deletes() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("new.txt");
        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        fs::write(&f, b"created").unwrap();
        s.record_write("new.txt", None, "created".into());

        let r = s.undo_last().unwrap();
        assert!(r.error.is_none());
        assert!(!f.exists());
    }

    #[test]
    fn list_returns_newest_first() {
        let mut s = UndoStack::new();
        s.record_write("a", Some("".into()), "".into());
        s.record_write("b", Some("".into()), "".into());
        let l = s.list(10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].path, "b");
        assert_eq!(l[1].path, "a");
    }

    /// Empty stack: undo_last and len are sensible defaults.
    #[test]
    fn empty_stack_is_inert() {
        let mut s = UndoStack::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.undo_last().is_none());
        assert!(s.list(10).is_empty());
    }

    /// Undo-by-id picks a specific entry, not necessarily the last.
    #[test]
    fn undo_by_id_targets_specific_entry() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, b"a_v1").unwrap();
        fs::write(&b, b"b_v1").unwrap();
        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        let id_a = s.record_write("a.txt", Some("a_v1".into()), "a_v2".into());
        let _id_b = s.record_write("b.txt", Some("b_v1".into()), "b_v2".into());
        fs::write(&a, b"a_v2").unwrap();
        fs::write(&b, b"b_v2").unwrap();

        // Undo `a` by id — `b` should remain in stack.
        let r = s.undo_by_id(id_a).unwrap();
        assert!(r.error.is_none());
        assert_eq!(fs::read_to_string(&a).unwrap(), "a_v1");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b_v2", "b must be untouched");
        assert_eq!(s.len(), 1, "stack should have 1 entry left (b's undo)");
    }

    /// Undo-by-id with an unknown id returns None.
    #[test]
    fn undo_by_id_unknown_returns_none() {
        let mut s = UndoStack::new();
        s.record_write("x", Some("".into()), "".into());
        assert!(s.undo_by_id(99999).is_none());
        assert_eq!(s.len(), 1, "stack must be unchanged");
    }

    /// list cap is honored.
    #[test]
    fn list_respects_count() {
        let mut s = UndoStack::new();
        for i in 0..5 {
            s.record_write(&format!("f{i}"), Some("".into()), "".into());
        }
        assert_eq!(s.list(2).len(), 2);
        assert_eq!(s.list(100).len(), 5);
    }

    /// record_delete + undo restores the file content.
    #[test]
    fn delete_then_undo_restores_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("gone.txt");
        // File doesn't exist by the time of undo (delete already happened).
        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        s.record_delete("gone.txt", Some("recovered content".into()));
        let r = s.undo_last().unwrap();
        assert!(r.error.is_none(), "got {:?}", r);
        assert_eq!(fs::read_to_string(&f).unwrap(), "recovered content");
    }

    /// record_delete without `before` content: undo fails with a clear error.
    #[test]
    fn delete_without_content_undo_errors() {
        let dir = tempdir().unwrap();
        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        s.record_delete("ghost.txt", None);
        let r = s.undo_last().unwrap();
        assert!(r.error.is_some(),
            "undo with no recorded contents must surface an error");
    }

    /// record_rename + undo moves the file back.
    #[test]
    fn rename_then_undo_moves_back() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("old.txt");
        let to = dir.path().join("new.txt");
        fs::write(&from, b"x").unwrap();
        // Perform the rename, then record it.
        fs::rename(&from, &to).unwrap();
        let mut s = UndoStack::new();
        s.set_base_dir(dir.path());
        s.record_rename("old.txt", "new.txt");

        let r = s.undo_last().unwrap();
        assert!(r.error.is_none(), "{:?}", r);
        assert!(from.exists(), "old path must be restored");
        assert!(!to.exists(), "new path must be gone");
    }

    /// Capacity cap: when the stack exceeds capacity, oldest entries are dropped.
    /// Anti-regression: a sloppy LIFO with no cap would OOM.
    #[test]
    fn capacity_caps_stack_size() {
        let mut s = UndoStack::with_capacity(3);
        for i in 0..10 {
            s.record_write(&format!("f{i}"), Some("".into()), "".into());
        }
        assert!(s.len() <= 3, "stack must not exceed capacity; got {} entries", s.len());
    }
}
