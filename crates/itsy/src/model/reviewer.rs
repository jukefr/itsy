//! Lightweight post-hoc reviewer that
//! double-checks model output looks "sane" before returning to the user.

pub struct ReviewResult {
    pub ok: bool,
    pub reason: Option<String>,
}

pub fn quick_review(content: &str) -> ReviewResult {
    if content.trim().is_empty() {
        return ReviewResult { ok: false, reason: Some("empty response".into()) };
    }
    if content.len() < 8 && !content.chars().any(|c| c.is_alphabetic()) {
        return ReviewResult { ok: false, reason: Some("nonsense response".into()) };
    }
    ReviewResult { ok: true, reason: None }
}
