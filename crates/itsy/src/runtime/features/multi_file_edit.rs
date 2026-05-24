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
