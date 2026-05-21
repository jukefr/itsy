//! Auto-rollback checkpoints — records
//! pre-edit file contents during a tool burst so we can revert on failure.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;

pub struct SnapshotManager {
    workdir: PathBuf,
    open: Mutex<Option<HashMap<PathBuf, Option<String>>>>,
}

impl SnapshotManager {
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir, open: Mutex::new(None) }
    }

    pub fn begin(&self) {
        *self.open.lock() = Some(HashMap::new());
    }

    pub fn note(&self, path: &Path, prev: Option<String>) {
        if let Some(map) = self.open.lock().as_mut() {
            map.entry(path.to_path_buf()).or_insert(prev);
        }
    }

    pub fn commit(&self) {
        *self.open.lock() = None;
    }

    pub fn rollback(&self) -> Result<(), std::io::Error> {
        if let Some(map) = self.open.lock().take() {
            for (path, prev) in map {
                match prev {
                    Some(content) => fs::write(&path, content)?,
                    None => {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(())
    }
}

static INSTANCE: OnceLock<SnapshotManager> = OnceLock::new();

pub fn get_snapshot_manager(workdir: PathBuf) -> &'static SnapshotManager {
    INSTANCE.get_or_init(|| SnapshotManager::new(workdir))
}
