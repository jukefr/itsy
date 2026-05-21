//! File Snapshot & Auto-Rollback.
//!
//! Sits on top of [`super::undo::UndoStack`] to provide checkpoint-style
//! rollback of file edits when verification fails. UndoStack alone is
//! per-edit; this layer groups edits into checkpoints that can be rolled
//! back as a unit.
//!
//! Workflow:
//!   1. Before a turn that may make multiple edits, the agent calls
//!      [`SnapshotManager::begin`] to mark a checkpoint.
//!   2. Each subsequent `write_file` / `patch` records its pre-edit
//!      content against the active checkpoint via
//!      [`SnapshotManager::note`].
//!   3. If validation fails permanently, the agent calls
//!      [`SnapshotManager::rollback`] which restores every file to its
//!      pre-checkpoint content. New files created during the checkpoint
//!      are deleted.
//!   4. On success, the agent calls [`SnapshotManager::commit`] which
//!      discards the checkpoint state without modifying files.
//!
//! Snapshot metadata is persisted to `.itsy/snapshots/<id>.json` so the
//! user can inspect or manually inspect outcomes even after a crash.
//!
//! Configuration (environment variables):
//!   * `ITSY_SNAPSHOT=false`              — disable entirely
//!   * `ITSY_SNAPSHOT_AUTO_ROLLBACK=true` — auto-rollback on hard fail
//!   * `ITSY_SNAPSHOT_DIR`                — override persistence dir

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use rand::RngCore;
use serde_json::json;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;
const DEFAULT_MAX_FILE_SIZE: u64 = 5 * 1024 * 1024; // 5 MiB

/// Per-file snapshot record inside an active checkpoint.
#[derive(Debug, Clone)]
enum FileSnap {
    /// File existed pre-checkpoint with this content.
    Content(String),
    /// File did not exist pre-checkpoint.
    Absent,
    /// File existed but was too large to capture.
    TooLarge,
    /// File existed but read failed for some other reason.
    Skipped,
}

#[derive(Debug, Clone)]
struct Checkpoint {
    id: String,
    label: String,
    started_at_ms: u128,
    /// First-snapshot-wins map of absolute path -> recorded state.
    files: HashMap<PathBuf, FileSnap>,
}

#[derive(Debug, Default, Clone)]
pub struct RollbackSummary {
    pub checkpoint_id: String,
    pub label: String,
    pub restored: usize,
    pub deleted: usize,
    pub errors: Vec<(PathBuf, String)>,
    pub skipped: Vec<(PathBuf, &'static str)>,
    pub reason: String,
}

pub struct SnapshotManager {
    workdir: PathBuf,
    snapshot_dir: PathBuf,
    disabled: bool,
    #[allow(dead_code)]
    auto_rollback: bool,
    max_file_size: u64,
    /// Only one checkpoint is active at a time; nesting is not supported.
    /// A new `begin()` while one is active commits the prior one.
    active: Mutex<Option<Checkpoint>>,
}

impl SnapshotManager {
    pub fn new(workdir: PathBuf) -> Self {
        let snapshot_dir = std::env::var_os("ITSY_SNAPSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::paths::snapshots_dir(&workdir));
        let disabled = std::env::var("ITSY_SNAPSHOT")
            .map(|v| v == "false")
            .unwrap_or(false);
        let auto_rollback = std::env::var("ITSY_SNAPSHOT_AUTO_ROLLBACK")
            .map(|v| v == "true")
            .unwrap_or(false);
        Self {
            workdir,
            snapshot_dir,
            disabled,
            auto_rollback,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            active: Mutex::new(None),
        }
    }

    /// Open a new checkpoint. Returns the checkpoint ID, or `None` if disabled.
    /// If a checkpoint is already open, it is committed first.
    pub fn begin(&self) -> Option<String> {
        self.begin_with("checkpoint")
    }

    /// Open a new checkpoint with a label. Label is truncated to 80 chars.
    pub fn begin_with(&self, label: &str) -> Option<String> {
        if self.disabled {
            return None;
        }
        // Close any prior checkpoint (idempotent commit).
        if self.active.lock().is_some() {
            self.commit();
        }
        let mut bytes = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut bytes);
        let id = bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>();
        let label_owned: String = label.chars().take(80).collect();
        let cp = Checkpoint {
            id: id.clone(),
            label: label_owned,
            started_at_ms: now_ms(),
            files: HashMap::new(),
        };
        *self.active.lock() = Some(cp);
        Some(id)
    }

    /// Snapshot a file's pre-edit content. Idempotent per path within a
    /// checkpoint — the FIRST snapshot wins (we want to revert to the state
    /// at checkpoint start, not after an intermediate edit).
    ///
    /// Pass `None` for `prev` to let us auto-detect (read from disk or
    /// record as absent). Pass `Some(content)` if the caller already has it.
    pub fn note(&self, file_path: &Path, prev: Option<String>) {
        if self.disabled {
            return;
        }
        let mut guard = self.active.lock();
        let Some(cp) = guard.as_mut() else {
            return;
        };

        // Resolve to absolute path inside workdir.
        let abs = if file_path.is_absolute() {
            file_path.to_path_buf()
        } else {
            self.workdir.join(file_path)
        };

        // Containment — only snapshot files inside the workspace root.
        if !path_contained(&self.workdir, &abs) {
            return;
        }
        // First-snapshot-wins.
        if cp.files.contains_key(&abs) {
            return;
        }

        match prev {
            Some(content) => {
                cp.files.insert(abs, FileSnap::Content(content));
            }
            None => {
                // Auto-detect from disk.
                match fs::metadata(&abs) {
                    Ok(meta) => {
                        if meta.len() > self.max_file_size {
                            cp.files.insert(abs, FileSnap::TooLarge);
                            return;
                        }
                        match fs::read_to_string(&abs) {
                            Ok(content) => {
                                cp.files.insert(abs, FileSnap::Content(content));
                            }
                            Err(_) => {
                                cp.files.insert(abs, FileSnap::Skipped);
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        cp.files.insert(abs, FileSnap::Absent);
                    }
                    Err(_) => {
                        cp.files.insert(abs, FileSnap::Skipped);
                    }
                }
            }
        }
    }

    /// Roll back every file recorded since the last `begin()`. Returns a
    /// summary. Files snapshotted as nonexistent are deleted; files with
    /// stored content are restored. `TooLarge` / `Skipped` entries are
    /// reported in `skipped` and left untouched.
    pub fn rollback(&self) -> RollbackSummary {
        self.rollback_with("verification failed")
    }

    pub fn rollback_with(&self, reason: &str) -> RollbackSummary {
        if self.disabled {
            return RollbackSummary {
                reason: reason.to_string(),
                ..Default::default()
            };
        }
        let Some(cp) = self.active.lock().take() else {
            return RollbackSummary {
                reason: reason.to_string(),
                ..Default::default()
            };
        };

        let mut restored: Vec<PathBuf> = Vec::new();
        let mut deleted: Vec<PathBuf> = Vec::new();
        let mut errors: Vec<(PathBuf, String)> = Vec::new();
        let mut skipped: Vec<(PathBuf, &'static str)> = Vec::new();

        for (abs, snap) in cp.files.iter() {
            match snap {
                FileSnap::TooLarge => {
                    skipped.push((abs.clone(), "file too large to snapshot"));
                }
                FileSnap::Skipped => {
                    skipped.push((abs.clone(), "snapshot read failed"));
                }
                FileSnap::Absent => {
                    if abs.exists() {
                        if let Err(e) = fs::remove_file(abs) {
                            errors.push((abs.clone(), e.to_string()));
                        } else {
                            deleted.push(abs.clone());
                        }
                    }
                }
                FileSnap::Content(content) => {
                    if let Some(parent) = abs.parent() {
                        if !parent.as_os_str().is_empty() && !parent.exists() {
                            if let Err(e) = fs::create_dir_all(parent) {
                                errors.push((abs.clone(), e.to_string()));
                                continue;
                            }
                        }
                    }
                    if let Err(e) = fs::write(abs, content) {
                        errors.push((abs.clone(), e.to_string()));
                    } else {
                        restored.push(abs.clone());
                    }
                }
            }
        }

        let summary = RollbackSummary {
            checkpoint_id: cp.id.clone(),
            label: cp.label.clone(),
            restored: restored.len(),
            deleted: deleted.len(),
            errors: errors.clone(),
            skipped: skipped.clone(),
            reason: reason.to_string(),
        };

        // Persist a record (best-effort).
        self.persist(&cp, json!({
            "reason": reason,
            "restored": restored.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "deleted": deleted.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "errors": errors.iter().map(|(p, e)| json!({ "path": p.to_string_lossy(), "error": e })).collect::<Vec<_>>(),
            "skipped": skipped.iter().map(|(p, r)| json!({ "path": p.to_string_lossy(), "reason": r })).collect::<Vec<_>>(),
            "rolledBack": true,
        }));

        summary
    }

    /// Discard the active checkpoint without restoring anything. Returns the
    /// committed checkpoint ID, or `None` if no checkpoint was open.
    pub fn commit(&self) -> Option<String> {
        if self.disabled {
            return None;
        }
        let cp = self.active.lock().take()?;
        let id = cp.id.clone();
        self.persist(&cp, json!({ "rolledBack": false, "committed": true }));
        Some(id)
    }

    /// Whether a checkpoint is currently open.
    pub fn is_open(&self) -> bool {
        self.active.lock().is_some()
    }

    /// Number of files snapshotted in the active checkpoint (0 if none).
    pub fn size(&self) -> usize {
        self.active
            .lock()
            .as_ref()
            .map(|cp| cp.files.len())
            .unwrap_or(0)
    }

    /// All absolute paths currently snapshotted in the active checkpoint.
    pub fn paths_in_flight(&self) -> Vec<PathBuf> {
        self.active
            .lock()
            .as_ref()
            .map(|cp| cp.files.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Reset everything — used between agent runs.
    pub fn reset(&self) {
        *self.active.lock() = None;
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    // ─── Internal ─────────────────────────────────────────────────────────

    fn persist(&self, cp: &Checkpoint, outcome: serde_json::Value) {
        if self.disabled {
            return;
        }
        // Best-effort — never fail the agent loop on persistence errors.
        let _ = self.persist_inner(cp, outcome);
    }

    fn persist_inner(
        &self,
        cp: &Checkpoint,
        outcome: serde_json::Value,
    ) -> std::io::Result<()> {
        if !self.snapshot_dir.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(DIR_MODE);
            builder.create(&self.snapshot_dir)?;
        }

        // Sanitize id — strip anything that isn't [A-Za-z0-9_-].
        let id_clean: String = cp
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if id_clean.is_empty() {
            return Ok(());
        }
        let file_path = self.snapshot_dir.join(format!("{id_clean}.json"));

        // Containment — must stay inside snapshotDir.
        if !path_contained(&self.snapshot_dir, &file_path) {
            return Ok(());
        }

        let started_at = ms_to_iso(cp.started_at_ms);
        let ended_at = ms_to_iso(now_ms());
        let paths: Vec<String> = cp
            .files
            .keys()
            .take(50)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        let summary = json!({
            "id": cp.id,
            "label": cp.label,
            "startedAt": started_at,
            "endedAt": ended_at,
            "fileCount": cp.files.len(),
            "files": paths,
            "outcome": outcome,
        });

        let pretty = serde_json::to_string_pretty(&summary)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let pid = std::process::id();
        let tmp = self.snapshot_dir.join(format!("{id_clean}.json.tmp.{pid}"));

        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(FILE_MODE);
            use std::io::Write;
            let mut f = opts.open(&tmp)?;
            f.write_all(pretty.as_bytes())?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &file_path)?;
        Ok(())
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn path_contained(root: &Path, candidate: &Path) -> bool {
    // Use canonical-free containment: candidate must start with root and
    // must not escape via "..". We rely on the fact that callers pass
    // pre-resolved absolute paths.
    let mut comps_root = root.components();
    let mut comps_cand = candidate.components();
    loop {
        match (comps_root.next(), comps_cand.next()) {
            (Some(r), Some(c)) => {
                if r != c {
                    return false;
                }
            }
            (None, _) => break,
            (Some(_), None) => return false,
        }
    }
    // Remaining components on candidate must not include "..".
    for comp in comps_cand {
        if matches!(comp, std::path::Component::ParentDir) {
            return false;
        }
    }
    true
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Minimal ISO-8601 (UTC, second precision) — avoids pulling chrono in here.
fn ms_to_iso(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    // Use chrono if available — it's already a workspace dep.
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{secs}"))
}

// ─── Singleton ─────────────────────────────────────────────────────────────
// The singleton is lazy — first call builds it. Subsequent calls reuse it IF
// the workdir matches. If cwd changed (e.g. bench tasks in temp dirs), a
// fresh instance is created (the old one is abandoned, not committed).

static INSTANCE: OnceCell<Mutex<Arc<SnapshotManager>>> = OnceCell::new();

pub fn get_snapshot_manager(workdir: PathBuf) -> Arc<SnapshotManager> {
    let cell = INSTANCE.get_or_init(|| Mutex::new(Arc::new(SnapshotManager::new(workdir.clone()))));
    let mut guard = cell.lock();
    if guard.workdir() != workdir.as_path() {
        *guard = Arc::new(SnapshotManager::new(workdir));
    }
    Arc::clone(&guard)
}

pub fn reset_snapshot_manager() {
    if let Some(cell) = INSTANCE.get() {
        cell.lock().reset();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rollback_restores_existing_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        fs::write(&p, "original").unwrap();

        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&p, Some("original".to_string()));
        fs::write(&p, "modified").unwrap();
        let summary = sm.rollback();

        assert_eq!(summary.restored, 1);
        assert_eq!(fs::read_to_string(&p).unwrap(), "original");
    }

    #[test]
    fn rollback_deletes_new_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("new.txt");

        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&p, None); // didn't exist
        fs::write(&p, "created").unwrap();
        let summary = sm.rollback();

        assert_eq!(summary.deleted, 1);
        assert!(!p.exists());
    }

    #[test]
    fn commit_preserves_changes() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("b.txt");
        fs::write(&p, "v1").unwrap();

        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&p, Some("v1".to_string()));
        fs::write(&p, "v2").unwrap();
        sm.commit();

        assert_eq!(fs::read_to_string(&p).unwrap(), "v2");
        assert!(!sm.is_open());
    }

    #[test]
    fn first_snapshot_wins() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("c.txt");
        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&p, Some("first".to_string()));
        sm.note(&p, Some("second".to_string()));
        fs::write(&p, "modified").unwrap();
        sm.rollback();
        assert_eq!(fs::read_to_string(&p).unwrap(), "first");
    }

    #[test]
    fn containment_rejects_outside_workdir() {
        let dir = tempdir().unwrap();
        let outside = std::env::temp_dir().join("itsy-snap-outside-test.txt");
        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&outside, Some("x".to_string()));
        assert_eq!(sm.size(), 0);
    }

    #[test]
    fn paths_in_flight_lists_noted() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("x.txt");
        let p2 = dir.path().join("y.txt");
        let sm = SnapshotManager::new(dir.path().to_path_buf());
        sm.begin();
        sm.note(&p1, Some(String::new()));
        sm.note(&p2, Some(String::new()));
        let paths = sm.paths_in_flight();
        assert_eq!(paths.len(), 2);
    }
}
