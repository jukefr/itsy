//! Vague-message check. Returns `true` if the user prompt is too ambiguous
//! to act on. Falls back to the regex-based check in
//! [`crate::session::clarify`] on any model failure.

use serde_json::json;

use super::prompts::call_prompt;

pub async fn check_needs_clarification(user_message: &str) -> bool {
    let r = match call_prompt("intent_clarifier", json!({ "user_message": user_message })).await {
        Ok(s) => s,
        Err(_) => return crate::session::clarify::needs_clarification(user_message).is_some(),
    };
    r.trim().to_lowercase().starts_with("vague")
}
