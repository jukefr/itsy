//! Compressed file summary. Used by `executor::exec_read_file` when a file
//! exceeds the inline-display threshold and rendering its raw contents would
//! blow up the context window.

use serde_json::json;

use super::prompts::{call_prompt, truncate};

/// Returns a summary string or `None` on any failure / files under 100 lines.
pub async fn summarize_file_compiled(file_path: &str, content: &str, target_tokens: u32) -> Option<String> {
    if content.split('\n').count() < 100 {
        return None;
    }
    let truncated = truncate(content, 8000);
    let r = call_prompt(
        "summarize_file",
        json!({
            "file_path": file_path,
            "content": truncated,
            "target_tokens": target_tokens,
        }),
    )
    .await
    .ok()?;
    Some(r)
}
