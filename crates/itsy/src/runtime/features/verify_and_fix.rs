//! Verify-and-fix loop — mirrors upstream JS `features/verify_and_fix.js`.
//!
//! After a file is written/patched, this module:
//!   1. Runs a fast self-critique via [`crate::features_adapter::validate_edit_compiled`].
//!   2. Runs the local compile/lint validator via [`crate::governor::verify_code`].
//!   3. If validation fails, formats a fix-request prompt (with attempt
//!      history) and hands control back to the model.
//!   4. Tracks per-file attempt counters and escalates after N failures.
//!
//! Public surface:
//!   - [`FixAttempt`] / [`should_continue`] — primitives.
//!   - [`VerifyAndFixLoop`] — orchestrator that owns the attempt counters.
//!   - [`VerifyAndFixLoop::run`] — fire-and-forget bounded loop driver.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::features_adapter::validate_edit_compiled;
use crate::governor::{verify_code, VerifyResult};
use crate::loops_adapter::{run_bounded_validation, BoundedValidationResult, ValidationOutcome};

// ─── Primitives ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FixAttempt {
    pub attempt: u32,
    pub errors: Vec<String>,
}

pub fn should_continue(attempt: &FixAttempt, max_attempts: u32) -> bool {
    attempt.attempt < max_attempts && !attempt.errors.is_empty()
}

// ─── Outcome of one verify_and_fix invocation ───────────────────────────────

/// Result of running [`VerifyAndFixLoop::verify_and_fix`] (the JS
/// `verifyAndFixCompiled` analogue).
#[derive(Debug, Clone, Default)]
pub struct VerifyAndFixOutcome {
    /// True when the loop produced a fix prompt that the caller should
    /// feed back to the model. Mirrors the JS `handled` flag.
    pub handled: bool,
    /// True if the loop wants the caller to break out of the agent loop.
    pub should_break: bool,
    /// Self-critique notice to inject (`[SEMANTIC-REVIEW]` line in JS).
    pub critique_notice: Option<String>,
    /// Fix-request prompt to send back to the model.
    pub fix_prompt: Option<String>,
    /// Set when the local validator passed.
    pub passed: bool,
    /// Number of attempts recorded for this file *after* this call.
    pub attempt: u32,
}

// ─── Orchestrator ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct VerifyAndFixLoop {
    inner: Arc<Mutex<Inner>>,
    max_iterations: u32,
}

#[derive(Default)]
struct Inner {
    /// Per-file counter of consecutive failed attempts.
    attempts: HashMap<PathBuf, u32>,
    /// Per-file truncated error history (last 3 errors per attempt).
    history: HashMap<PathBuf, Vec<FixAttempt>>,
    /// Per-file decompose counter (used to decide when to escalate).
    decompose: HashMap<PathBuf, u32>,
}

impl VerifyAndFixLoop {
    pub fn new(max_iterations: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            max_iterations: max_iterations.max(1),
        }
    }

    pub fn max_iterations(&self) -> u32 {
        self.max_iterations
    }

    /// Reset all per-file state.
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.attempts.clear();
        g.history.clear();
        g.decompose.clear();
    }

    /// Reset just the attempt counter for a single file. Mirrors the
    /// `improvementAttempts[filePath] = 0` path on success.
    pub fn reset_file(&self, path: &Path) {
        let mut g = self.inner.lock().unwrap();
        g.attempts.remove(path);
        g.history.remove(path);
    }

    /// Returns the current attempt counter for a file.
    pub fn current_attempt(&self, path: &Path) -> u32 {
        self.inner.lock().unwrap().attempts.get(path).copied().unwrap_or(0)
    }

    /// Build the fix-request prompt for a single failed validation.
    /// Mirrors the JS `fixPrompt` branches (short for attempts 1–2,
    /// includes the full file from attempt 3 onward).
    pub fn format_fix_prompt(
        &self,
        file_path: &str,
        attempt: u32,
        errors: &[String],
        history: &[FixAttempt],
        file_content: Option<&str>,
        test_hint: Option<&str>,
    ) -> String {
        let history_str = if history.len() > 1 {
            let mut s = format!(
                "\n\nPrevious attempts ({} failed):",
                history.len() - 1
            );
            for (i, h) in history[..history.len() - 1].iter().enumerate() {
                let first = h.errors.first().cloned().unwrap_or_else(|| "unknown error".to_string());
                s.push_str(&format!("\n  Attempt {}: {}", i + 1, first));
            }
            s
        } else {
            String::new()
        };

        if attempt <= 2 {
            let hint = test_hint.unwrap_or("");
            format!(
                "[AUTO-VALIDATE] Errors in {file} (attempt {att}/{max}):\n{errs}{hist}{hint}\n\nFix these errors. Do NOT repeat the same approach that failed before.",
                file = file_path,
                att = attempt,
                max = self.max_iterations,
                errs = errors.join("\n"),
                hist = history_str,
                hint = hint,
            )
        } else {
            let content = file_content.unwrap_or("");
            let max_chars = 8000usize;
            let capped = if content.len() > max_chars {
                let mut end = max_chars;
                while end > 0 && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated_tokens = (content.len() - end + 3) / 4;
                format!(
                    "{}\n... ({} more tokens truncated)",
                    &content[..end],
                    truncated_tokens
                )
            } else {
                content.to_string()
            };
            format!(
                "[AUTO-VALIDATE] After {att} attempts, {file} still has errors.{hist}\n\nFULL FILE CONTENT:\n```\n{body}\n```\n\nERRORS:\n{errs}\n\nRead the FULL file above carefully. Fix ALL errors. Do NOT repeat previous failed approaches.",
                att = attempt,
                file = file_path,
                hist = history_str,
                body = capped,
                errs = errors.join("\n"),
            )
        }
    }

    /// Self-critique via the model. Returns the `[SEMANTIC-REVIEW]` notice
    /// to inject, or `None`. Always swallows errors — never blocks.
    pub async fn self_critique(
        &self,
        file_path: &str,
        user_message: &str,
    ) -> Option<String> {
        let cwd = std::env::current_dir().ok()?;
        let full = cwd.join(file_path);
        let written = std::fs::read_to_string(&full).unwrap_or_default();
        let res = validate_edit_compiled(file_path, &written, user_message).await;
        if !res.ok {
            if let Some(first) = res.issues.first() {
                return Some(format!(
                    "[SEMANTIC-REVIEW] Potential issue in {file_path}: {first}"
                ));
            }
        }
        None
    }

    /// One pass of the JS `verifyAndFixCompiled` orchestration. Runs
    /// self-critique, then local validation; on failure, increments the
    /// per-file counter and returns a formatted fix-request prompt.
    pub async fn verify_and_fix(
        &self,
        file_path: &str,
        user_message: &str,
    ) -> VerifyAndFixOutcome {
        let path_buf = PathBuf::from(file_path);
        let critique_notice = self.self_critique(file_path, user_message).await;

        let validation: VerifyResult = verify_code(file_path);

        if validation.passed {
            // Passed — reset counter if we had been retrying.
            let prior = {
                let mut g = self.inner.lock().unwrap();
                let p = g.attempts.get(&path_buf).copied().unwrap_or(0);
                g.attempts.remove(&path_buf);
                g.history.remove(&path_buf);
                p
            };
            return VerifyAndFixOutcome {
                handled: false,
                should_break: false,
                critique_notice,
                fix_prompt: None,
                passed: true,
                attempt: prior,
            };
        }

        // Validation failed — increment counter and snapshot history.
        let (attempt, history_snapshot) = {
            let mut g = self.inner.lock().unwrap();
            let counter = g.attempts.entry(path_buf.clone()).or_insert(0);
            *counter += 1;
            let attempt = *counter;
            let hist = g.history.entry(path_buf.clone()).or_default();
            let errs: Vec<String> = validation.errors.iter().take(3).cloned().collect();
            hist.push(FixAttempt {
                attempt,
                errors: errs,
            });
            (attempt, hist.clone())
        };

        if attempt <= self.max_iterations {
            let file_content = if attempt > 2 {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                std::fs::read_to_string(cwd.join(file_path)).ok()
            } else {
                None
            };
            let prompt = self.format_fix_prompt(
                file_path,
                attempt,
                &validation.errors,
                &history_snapshot,
                file_content.as_deref(),
                None,
            );
            return VerifyAndFixOutcome {
                handled: true,
                should_break: false,
                critique_notice,
                fix_prompt: Some(prompt),
                passed: false,
                attempt,
            };
        }

        // Exhausted — emit a DECOMPOSE prompt and reset the counter.
        {
            let mut g = self.inner.lock().unwrap();
            g.attempts.insert(path_buf.clone(), 0);
            let dc = g.decompose.entry(path_buf.clone()).or_insert(0);
            *dc += 1;
        }
        let decompose_prompt = format!(
            "[DECOMPOSE] After {} failed fix attempts, changing strategy.\n\nFix {} from scratch. Errors:\n{}",
            self.max_iterations,
            file_path,
            validation.errors.join("\n"),
        );
        VerifyAndFixOutcome {
            handled: true,
            should_break: false,
            critique_notice,
            fix_prompt: Some(decompose_prompt),
            passed: false,
            attempt,
        }
    }

    /// Bounded validate → fix loop. Calls `validate_fn` (default:
    /// [`verify_code`]) up to `max_attempts` times; whenever it fails,
    /// invokes `fix_fn` with the current error list so the caller can
    /// regenerate the file. Returns `Ok(())` on success, `Err(errors)`
    /// when the budget is exhausted. Mirrors the iterative top half of
    /// `verifyAndFixCompiled`.
    pub async fn run<V, VFut, F, FFut>(
        &self,
        file_path: &str,
        mut validate_fn: V,
        mut fix_fn: F,
        max_attempts: u32,
    ) -> Result<(), Vec<String>>
    where
        V: FnMut(String) -> VFut,
        VFut: std::future::Future<Output = Option<ValidationOutcome>>,
        F: FnMut(FixAttempt) -> FFut,
        FFut: std::future::Future<Output = ()>,
    {
        let path_buf = PathBuf::from(file_path);
        let max = max_attempts.max(1);

        // Run the bounded validation helper from loops_adapter.
        let outcome: BoundedValidationResult =
            run_bounded_validation(&mut validate_fn, file_path, max).await;

        if outcome.passed {
            self.reset_file(&path_buf);
            return Ok(());
        }

        // Replay attempts so the caller can fix once per failure.
        // We call fix_fn for each failed attempt up to the budget, then
        // give the validator one more chance on each subsequent loop turn.
        let mut current_errors = outcome.last_errors.clone();
        for attempt in 1..=max {
            // Record into our history.
            {
                let mut g = self.inner.lock().unwrap();
                let counter = g.attempts.entry(path_buf.clone()).or_insert(0);
                *counter = attempt;
                let hist = g.history.entry(path_buf.clone()).or_default();
                let errs: Vec<String> = current_errors.iter().take(3).cloned().collect();
                hist.push(FixAttempt { attempt, errors: errs });
            }

            let fa = FixAttempt {
                attempt,
                errors: current_errors.clone(),
            };
            if !should_continue(&fa, max) {
                break;
            }
            fix_fn(fa).await;

            // Re-validate after the fix.
            match validate_fn(file_path.to_string()).await {
                None => {
                    self.reset_file(&path_buf);
                    return Ok(());
                }
                Some(o) if o.passed => {
                    self.reset_file(&path_buf);
                    return Ok(());
                }
                Some(o) => {
                    current_errors = o.errors;
                }
            }
        }
        Err(current_errors)
    }
}

impl Default for VerifyAndFixLoop {
    fn default() -> Self {
        Self::new(3)
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Validator adapter that runs the local [`verify_code`] governor and
/// returns it as a [`ValidationOutcome`] suitable for
/// [`VerifyAndFixLoop::run`].
pub async fn local_validator(file_path: String) -> Option<ValidationOutcome> {
    let r = verify_code(&file_path);
    Some(ValidationOutcome {
        passed: r.passed,
        errors: r.errors,
    })
}

/// Pack a fix-request into the `serde_json::Value` envelope used by the
/// rest of the features layer (so callers can stash it on a conversation
/// history entry).
pub fn fix_request_args(file_path: &str, prompt: &str) -> Value {
    serde_json::json!({
        "role": "user",
        "file_path": file_path,
        "content": prompt,
    })
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_continue_stops_when_no_errors() {
        let a = FixAttempt { attempt: 0, errors: vec![] };
        assert!(!should_continue(&a, 3));
    }

    #[test]
    fn should_continue_stops_at_max() {
        let a = FixAttempt { attempt: 3, errors: vec!["e".into()] };
        assert!(!should_continue(&a, 3));
    }

    #[test]
    fn format_fix_prompt_short_form() {
        let l = VerifyAndFixLoop::new(3);
        let p = l.format_fix_prompt(
            "src/foo.rs",
            1,
            &["E0001: bad".to_string()],
            &[FixAttempt { attempt: 1, errors: vec!["E0001: bad".into()] }],
            None,
            None,
        );
        assert!(p.contains("[AUTO-VALIDATE]"));
        assert!(p.contains("src/foo.rs"));
        assert!(p.contains("attempt 1/3"));
        assert!(p.contains("E0001: bad"));
    }

    #[test]
    fn format_fix_prompt_long_form_includes_file() {
        let l = VerifyAndFixLoop::new(3);
        let p = l.format_fix_prompt(
            "src/foo.rs",
            3,
            &["E0001".to_string()],
            &[
                FixAttempt { attempt: 1, errors: vec!["E1".into()] },
                FixAttempt { attempt: 2, errors: vec!["E2".into()] },
                FixAttempt { attempt: 3, errors: vec!["E3".into()] },
            ],
            Some("fn main() {}"),
            None,
        );
        assert!(p.contains("FULL FILE CONTENT"));
        assert!(p.contains("fn main() {}"));
        assert!(p.contains("Previous attempts (2 failed)"));
    }

    #[test]
    fn reset_file_clears_state() {
        let l = VerifyAndFixLoop::new(3);
        let path = PathBuf::from("x.rs");
        {
            let mut g = l.inner.lock().unwrap();
            g.attempts.insert(path.clone(), 2);
            g.history.insert(path.clone(), vec![FixAttempt { attempt: 1, errors: vec!["e".into()] }]);
        }
        assert_eq!(l.current_attempt(&path), 2);
        l.reset_file(&path);
        assert_eq!(l.current_attempt(&path), 0);
    }

    #[tokio::test]
    async fn run_returns_ok_when_validator_passes_first_try() {
        let l = VerifyAndFixLoop::new(3);
        let mut fix_calls = 0u32;
        let res = l
            .run(
                "no-such-file.rs",
                |_p| async move {
                    Some(ValidationOutcome { passed: true, errors: vec![] })
                },
                |_a| {
                    fix_calls += 1;
                    async {}
                },
                3,
            )
            .await;
        assert!(res.is_ok());
        assert_eq!(fix_calls, 0);
    }

    #[tokio::test]
    async fn run_returns_err_when_exhausted() {
        let l = VerifyAndFixLoop::new(2);
        let res = l
            .run(
                "no-such-file.rs",
                |_p| async move {
                    Some(ValidationOutcome {
                        passed: false,
                        errors: vec!["nope".into()],
                    })
                },
                |_a| async {},
                2,
            )
            .await;
        assert!(res.is_err());
        let errs = res.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("nope")));
    }
}
