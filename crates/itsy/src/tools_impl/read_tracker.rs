//! Read-before-write guard.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;

pub struct ReadTracker {
    read_paths: Mutex<HashSet<PathBuf>>,
    written_paths: Mutex<HashSet<PathBuf>>,
    warned_paths: Mutex<HashSet<PathBuf>>,
    pub disabled: bool,
    pub strict: bool,
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
            read_paths: Mutex::new(HashSet::new()),
            written_paths: Mutex::new(HashSet::new()),
            warned_paths: Mutex::new(HashSet::new()),
            disabled: env::var("ITSY_WRITE_GUARD").ok().as_deref() == Some("false"),
            strict: env::var("ITSY_WRITE_GUARD_STRICT").ok().as_deref() == Some("true"),
        }
    }

    fn canon(&self, p: &Path, cwd: &Path) -> Option<PathBuf> {
        let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        Some(normalize(&abs))
    }

    pub fn record_read(&self, file_path: &Path, cwd: &Path) {
        if self.disabled {
            return;
        }
        if let Some(c) = self.canon(file_path, cwd) {
            self.read_paths.lock().insert(c.clone());
            self.warned_paths.lock().remove(&c);
        }
    }

    pub fn record_write(&self, file_path: &Path, cwd: &Path) {
        if self.disabled {
            return;
        }
        if let Some(c) = self.canon(file_path, cwd) {
            self.written_paths.lock().insert(c.clone());
            self.read_paths.lock().insert(c);
        }
    }

    pub fn check_write(&self, file_path: &Path, cwd: &Path) -> WriteCheck {
        if self.disabled {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        let Some(c) = self.canon(file_path, cwd) else {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        };
        if !fs::metadata(&c).is_ok() {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        if self.read_paths.lock().contains(&c) || self.written_paths.lock().contains(&c) {
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        let rel = pathdiff(&c, cwd).unwrap_or_else(|| c.display().to_string());
        if self.strict {
            return WriteCheck {
                ok: false,
                blocked: true,
                warning: false,
                reason: Some(format!(
                    "Refused: write_file to existing file '{rel}' without prior read_file. Read the file first to see what's there."
                )),
            };
        }
        if self.warned_paths.lock().contains(&c) {
            self.record_write(file_path, cwd);
            return WriteCheck { ok: true, reason: None, warning: false, blocked: false };
        }
        self.warned_paths.lock().insert(c);
        WriteCheck {
            ok: false,
            warning: true,
            blocked: false,
            reason: Some(format!(
                "Refused: write_file would overwrite existing '{rel}' you haven't read. Call read_file first to see its current content, OR if you intend to fully replace it, retry — second attempt is allowed."
            )),
        }
    }

    pub fn reset(&self) {
        self.read_paths.lock().clear();
        self.written_paths.lock().clear();
        self.warned_paths.lock().clear();
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
