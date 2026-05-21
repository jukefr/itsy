//! Tracks per-file content fingerprints
//! so subsequent reads can return a diff rather than the full content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

pub enum RecordResult {
    Unchanged,
    Diff { diff: String, full_length: usize },
    Full,
}

pub struct FileStateTracker {
    known: Mutex<HashMap<PathBuf, (String, String)>>, // path → (hash, content)
}

impl FileStateTracker {
    fn new() -> Self {
        Self { known: Mutex::new(HashMap::new()) }
    }

    pub fn record(&self, path: &Path, content: &str) -> RecordResult {
        let hash = sha256(content);
        let mut g = self.known.lock();
        if let Some((prev_hash, prev_content)) = g.get(path) {
            if *prev_hash == hash {
                return RecordResult::Unchanged;
            }
            let diff = quick_diff(prev_content, content);
            let lines = content.split('\n').count();
            g.insert(path.to_path_buf(), (hash, content.to_string()));
            return RecordResult::Diff { diff, full_length: lines };
        }
        g.insert(path.to_path_buf(), (hash, content.to_string()));
        RecordResult::Full
    }

    pub fn record_write(&self, path: &Path, content: &str) {
        let hash = sha256(content);
        self.known.lock().insert(path.to_path_buf(), (hash, content.to_string()));
    }
}

fn sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn quick_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut out = String::new();
    let common_prefix = old_lines.iter().zip(new_lines.iter()).take_while(|(a, b)| a == b).count();
    let common_suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let from = common_prefix;
    let to_old = old_lines.len().saturating_sub(common_suffix);
    let to_new = new_lines.len().saturating_sub(common_suffix);
    for line in &old_lines[from..to_old] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &new_lines[from..to_new] {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

static INSTANCE: OnceLock<FileStateTracker> = OnceLock::new();

pub fn get_file_state_tracker() -> &'static FileStateTracker {
    INSTANCE.get_or_init(FileStateTracker::new)
}
