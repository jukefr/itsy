//! Read-before-write guard.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;

pub struct ReadTracker {
    /// Files that have been read and not modified since — the only ones safe to write_file.
    clean_paths: Mutex<HashSet<PathBuf>>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct WriteCheck {
    pub ok: bool,
    pub reason: Option<String>,
    pub warning: bool,
    pub blocked: bool,
}

impl ReadTracker {
    fn new() -> Self {
        Self {
            clean_paths: Mutex::new(HashSet::new()),
            disabled: env::var("ITSY_WRITE_GUARD").ok().as_deref() == Some("false"),
        }
    }

    fn canon(&self, p: &Path, cwd: &Path) -> Option<PathBuf> {
        let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        Some(normalize(&abs))
    }

    /// Call after a successful read_file — marks the file clean for write_file.
    pub fn record_read(&self, file_path: &Path, cwd: &Path) {
        if self.disabled { return; }
        if let Some(c) = self.canon(file_path, cwd) {
            self.clean_paths.lock().insert(c);
        }
    }

    /// Call after write_file — file content changed, model must re-read before next write.
    pub fn record_write(&self, file_path: &Path, cwd: &Path) {
        if self.disabled { return; }
        if let Some(c) = self.canon(file_path, cwd) {
            self.clean_paths.lock().remove(&c);
        }
    }

    /// Call after a successful patch — file content changed, model must re-read before write_file.
    pub fn record_patch(&self, file_path: &Path, cwd: &Path) {
        if self.disabled { return; }
        if let Some(c) = self.canon(file_path, cwd) {
            self.clean_paths.lock().remove(&c);
        }
    }

    /// Returns ok=true only if the file doesn't exist yet, or was read after its last modification.
    pub fn check_write(&self, file_path: &Path, cwd: &Path) -> WriteCheck {
        if self.disabled {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        let Some(c) = self.canon(file_path, cwd) else {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        };
        // Creating a new file is always fine.
        if !fs::metadata(&c).is_ok() {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        if self.clean_paths.lock().contains(&c) {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        let rel = pathdiff(&c, cwd).unwrap_or_else(|| c.display().to_string());
        WriteCheck {
            ok: false,
            blocked: true,
            warning: false,
            reason: Some(format!(
                "write_file rejected: '{rel}' has been modified since you last read it, \
                 or you haven't read it yet. Call read_file first to see the current \
                 content before overwriting."
            )),
        }
    }

    pub fn reset(&self) {
        self.clean_paths.lock().clear();
    }
}

fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn pathdiff(target: &Path, base: &Path) -> Option<String> {
    let target_comps: Vec<_> = target.components().collect();
    let base_comps: Vec<_> = base.components().collect();
    let mut i = 0;
    while i < target_comps.len() && i < base_comps.len() && target_comps[i] == base_comps[i] {
        i += 1;
    }
    if i < base_comps.len() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in &target_comps[i..] {
        out.push(c.as_os_str());
    }
    Some(out.to_string_lossy().into_owned())
}

static INSTANCE: OnceLock<ReadTracker> = OnceLock::new();

pub fn get_read_tracker() -> &'static ReadTracker {
    INSTANCE.get_or_init(ReadTracker::new)
}
