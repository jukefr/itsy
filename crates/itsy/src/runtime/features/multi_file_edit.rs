//! Multi-file edit coordination.
//!
//! Coordination layer for agent turns that edit 3+ files simultaneously.
//! Injects a structured coordination header into the conversation history so
//! the model tracks which files still need editing and doesn't drift.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Plan / serializable representation of a multi-file edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPlan {
    pub edits: Vec<PlannedEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedEdit {
    pub path: String,
    pub summary: String,
}

/// Outcome of a coordination attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationResult {
    pub plan: Vec<String>,
    pub injected: bool,
}

/// Optional snapshot manager hook: callers that maintain snapshot checkpoints
/// can opt in by implementing this trait.
pub trait SnapshotManager {
    fn begin(&mut self, label: &str);
}

const HEADER_MARKER: &str = "[MULTI-FILE-EDIT]";

/// Coordinate a multi-file edit. Mirrors `coordinateMultiFileEdit` in the JS
/// source: opens a snapshot checkpoint (when provided), then optionally
/// appends a coordination header onto the conversation history.
///
/// * `task`  - the user's task description (kept on the signature for parity).
/// * `files`  - files being edited this turn.
/// * `conversation_history`  - mutated in place; must be an array of
///   `{role, content}` JSON objects.
/// * `snapshot`  - optional snapshot manager that receives a
///   `multi-file-<unix_ms>` label.
pub fn coordinate_multi_file_edit(
    _task: &str,
    files: &[String],
    conversation_history: &mut Vec<Value>,
    snapshot: Option<&mut dyn SnapshotManager>,
) -> CoordinationResult {
    if files.len() < 3 {
        return CoordinationResult { plan: Vec::new(), injected: false };
    }

    // Best-effort snapshot checkpoint open.
    if let Some(sm) = snapshot {
        let label = format!(
            "multi-file-{}",
            Utc::now().timestamp_millis()
        );
        sm.begin(&label);
    }

    // Build the plan.
    let plan: Vec<String> = files
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}. Edit {f}", i + 1))
        .collect();

    // Only inject if no recent system message already carries the marker.
    let recent_start = conversation_history.len().saturating_sub(6);
    let already_injected = conversation_history[recent_start..]
        .iter()
        .any(|m| {
            m.get("content")
                .and_then(|v| v.as_str())
                .map(|c| c.contains(HEADER_MARKER))
                .unwrap_or(false)
        });

    if already_injected {
        return CoordinationResult { plan, injected: false };
    }

    let header_lines: Vec<String> = std::iter::once(format!(
        "{HEADER_MARKER} This turn requires coordinated changes to {} files.",
        files.len()
    ))
    .chain(std::iter::once(String::new()))
    .chain(std::iter::once("Files to edit:".into()))
    .chain(plan.iter().cloned())
    .chain(std::iter::once(String::new()))
    .chain(std::iter::once(
        "Complete ALL files before responding. Do not skip any. Check each file for cross-file consistency (imports, exports, shared types).".into(),
    ))
    .collect();

    let header = header_lines.join("\n");
    conversation_history.push(json!({
        "role": "system",
        "content": header,
    }));

    CoordinationResult { plan, injected: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// <3 files → no coordination, no plan, no injection.
    /// Anti-regression: small edits shouldn't get the expensive header treatment.
    #[test]
    fn does_not_coordinate_small_edits() {
        let mut hist = Vec::new();
        let result = coordinate_multi_file_edit(
            "task",
            &["a.rs".to_string(), "b.rs".to_string()],
            &mut hist,
            None,
        );
        assert!(!result.injected);
        assert!(result.plan.is_empty());
        assert!(hist.is_empty(), "no message must be appended for <3 files");
    }

    /// ≥3 files → coordination header is injected with the file list.
    #[test]
    fn coordinates_three_or_more_files() {
        let mut hist = Vec::new();
        let files: Vec<String> = vec!["a.rs", "b.rs", "c.rs"].into_iter().map(String::from).collect();
        let result = coordinate_multi_file_edit("task", &files, &mut hist, None);
        assert!(result.injected);
        assert_eq!(result.plan.len(), 3);
        assert_eq!(hist.len(), 1, "exactly one system message must be appended");
        let content = hist[0]["content"].as_str().unwrap();
        assert!(content.contains("MULTI-FILE-EDIT"));
        assert!(content.contains("a.rs"));
        assert!(content.contains("c.rs"));
    }

    /// If a recent message already has the marker, don't re-inject.
    /// Anti-regression: header bloat after multi-turn editing.
    #[test]
    fn does_not_re_inject_when_marker_present() {
        let mut hist: Vec<Value> = vec![
            json!({"role": "system", "content": "[MULTI-FILE-EDIT] previous header"}),
        ];
        let files: Vec<String> = vec!["x.rs", "y.rs", "z.rs"].into_iter().map(String::from).collect();
        let result = coordinate_multi_file_edit("task", &files, &mut hist, None);
        assert!(!result.injected, "must not re-inject when marker already present");
        assert_eq!(hist.len(), 1, "no new message appended");
        // Plan is still computed for caller.
        assert_eq!(result.plan.len(), 3);
    }

    /// Marker check only inspects the last 6 messages — older markers shouldn't
    /// suppress new headers (since the model has likely lost context by then).
    #[test]
    fn injects_when_marker_is_far_in_past() {
        let mut hist: Vec<Value> = vec![
            json!({"role": "system", "content": "[MULTI-FILE-EDIT] old"}),
        ];
        // Fill with 6+ messages of other content so the old marker is outside the window.
        for _ in 0..7 {
            hist.push(json!({"role": "user", "content": "x"}));
        }
        let files: Vec<String> = vec!["a.rs", "b.rs", "c.rs"].into_iter().map(String::from).collect();
        let result = coordinate_multi_file_edit("t", &files, &mut hist, None);
        assert!(result.injected,
            "marker outside the 6-msg window must NOT suppress new header injection");
    }

    /// Snapshot hook is called for ≥3 file edits.
    #[test]
    fn snapshot_manager_invoked_on_large_edit() {
        struct Counter { calls: u32, last_label: String }
        impl SnapshotManager for Counter {
            fn begin(&mut self, label: &str) {
                self.calls += 1;
                self.last_label = label.to_string();
            }
        }
        let mut snap = Counter { calls: 0, last_label: String::new() };
        let mut hist = Vec::new();
        let files: Vec<String> = vec!["a.rs", "b.rs", "c.rs"].into_iter().map(String::from).collect();
        coordinate_multi_file_edit("t", &files, &mut hist, Some(&mut snap));
        assert_eq!(snap.calls, 1, "snapshot.begin must be called once");
        assert!(snap.last_label.starts_with("multi-file-"),
            "label must start with multi-file-, got {}", snap.last_label);
    }

    /// Snapshot hook is NOT called for <3 file edits.
    #[test]
    fn snapshot_manager_not_invoked_on_small_edit() {
        struct Counter { calls: u32 }
        impl SnapshotManager for Counter {
            fn begin(&mut self, _label: &str) { self.calls += 1; }
        }
        let mut snap = Counter { calls: 0 };
        let mut hist = Vec::new();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        coordinate_multi_file_edit("t", &files, &mut hist, Some(&mut snap));
        assert_eq!(snap.calls, 0, "snapshot.begin must NOT fire for <3 files");
    }
}
