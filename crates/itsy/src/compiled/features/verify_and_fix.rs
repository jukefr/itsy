//! Verification loop
//! helper used by the executor when a write returns errors.

#[derive(Debug, Clone)]
pub struct FixAttempt {
    pub attempt: u32,
    pub errors: Vec<String>,
}

pub fn should_continue(attempt: &FixAttempt, max_attempts: u32) -> bool {
    attempt.attempt < max_attempts && !attempt.errors.is_empty()
}
