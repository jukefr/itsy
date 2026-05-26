//! Semantic-merge fallback. When a string-substitution patch fails because
//! the `old_str` no longer matches verbatim, hand the file + the intended
//! change to the compiled `semantic_merge` prompt and return the corrected
//! full-file content.

use serde_json::json;

use super::prompts::{call_prompt, strip_fences};

/// Recover from a patch failure where `old_str` no longer appears. Returns
/// the full corrected file content or `None` on failure.
pub async fn semantic_merge(file_path: &str, intended_change: &str, current_content: &str) -> Option<String> {
    let r = call_prompt(
        "semantic_merge",
        json!({
            "file": file_path,
            "intended_change": intended_change,
            "current_content": current_content,
        }),
    )
    .await
    .ok()?;
    let stripped = strip_fences(&r);
    if stripped.is_empty() { None } else { Some(stripped) }
}
