//! Detects repetition loops, patch
//! spirals, and greeting regression in streamed model output.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StopSignal {
    pub reason: &'static str,
    pub message: String,
    pub action: &'static str,
    pub injection: String,
}

pub struct EarlyStopDetector {
    pub repetition_threshold: u32,
    pub repetition_window_chars: usize,
    pub max_patch_failures: u32,
    pub max_response_tokens: u32,
    pub enable_greeting_detection: bool,
    /// Consecutive bash non-zero exits before injecting a correction.
    pub max_consecutive_bash_failures: u32,
    patch_failures: HashMap<PathBuf, u32>,
    patch_attempts: HashMap<PathBuf, u32>,
    /// Files blocked from patching after a stuck spiral.
    /// Cleared when the model successfully reads the file — forces
    /// re-read before re-patch rather than a permanent session ban.
    patch_blocked_files: HashSet<PathBuf>,
    consecutive_bash_failures: u32,
}

impl EarlyStopDetector {
    pub fn new() -> Self {
        Self {
            repetition_threshold: 3,
            repetition_window_chars: 200,
            max_patch_failures: 4,
            max_response_tokens: 8192,
            enable_greeting_detection: true,
            max_consecutive_bash_failures: 8,
            patch_failures: HashMap::new(),
            patch_attempts: HashMap::new(),
            patch_blocked_files: HashSet::new(),
            consecutive_bash_failures: 0,
        }
    }

    /// Returns a hard error if this file has been permanently blocked after
    /// a patch spiral. Call this BEFORE executing a patch tool call.
    pub fn check_patch_blocked(&self, file_path: &str) -> Option<StopSignal> {
        let file_path = normalize_path(file_path);
        if !self.patch_blocked_files.contains(&file_path) {
            return None;
        }
        Some(StopSignal {
            reason: "patch_blocked",
            message: format!(
                "Patch on {} blocked — file is in hot-file safe mode.",
                file_path.display()
            ),
            action: "hard_block",
            injection: format!(
                "[LOOP DETECTED] patch on '{}' is blocked. That file is in HOT-FILE SAFE MODE.\n\
                 1. Call read_file on '{}' to refresh the exact current content.\n\
                 2. Then make ONE small edit with a unique old_str / anchor.\n\
                 3. Re-run the narrowest verifier or compiler check before editing again.\n\
                 Do NOT keep patching blindly, and do NOT rewrite the whole file from scratch.",
                file_path.display(),
                file_path.display(),
            ),
        })
    }

    pub fn check_repetition(&self, buffer: &str) -> Option<StopSignal> {
        if buffer.len() < self.repetition_window_chars * 2 {
            return None;
        }
        let tail_start = buffer.len() - self.repetition_window_chars;
        let mut tail_start = tail_start;
        while tail_start > 0 && !buffer.is_char_boundary(tail_start) {
            tail_start -= 1;
        }
        let tail = &buffer[tail_start..];
        for window_size in [50usize, 80, 120] {
            if tail.len() < window_size * self.repetition_threshold as usize {
                continue;
            }
            // Slice last `window_size` chars, respecting char boundaries.
            let mut pat_start = tail.len() - window_size;
            while pat_start > 0 && !tail.is_char_boundary(pat_start) {
                pat_start -= 1;
            }
            let pattern = &tail[pat_start..];
            let mut count = 0u32;
            let mut search_from = 0;
            while let Some(idx) = tail[search_from..].find(pattern) {
                count += 1;
                search_from += idx + 1;
                if count >= self.repetition_threshold {
                    return Some(StopSignal {
                        reason: "repetition_loop",
                        message: format!("Model repeating itself ({}-char pattern {}x). Stopping.", window_size, count),
                        action: "inject_correction",
                        injection: "[SYSTEM] You are repeating the same output in a loop. STOP. Take a different approach or state what is blocking you.".into(),
                    });
                }
            }
        }
        None
    }

    pub fn record_patch_result(&mut self, file_path: &str, success: bool, old_str: &str, new_str: &str) -> Option<StopSignal> {
        let file_path = normalize_path(file_path);
        let attempts = self.patch_attempts.entry(file_path.clone()).or_insert(0);
        *attempts += 1;
        let total_attempts = *attempts;

        let is_noop = success && !old_str.is_empty() && !new_str.is_empty() && old_str == new_str;
        if success && !is_noop {
            if let Some(f) = self.patch_failures.get_mut(&file_path) {
                *f = f.saturating_sub(1);
            }
            return None;
        }
        let fails = self.patch_failures.entry(file_path.clone()).or_insert(0);
        *fails += 1;
        let fail_count = *fails;
        if fail_count >= self.max_patch_failures {
            self.patch_failures.remove(&file_path);
            self.patch_attempts.remove(&file_path);
            self.patch_blocked_files.insert(file_path.clone());
            return Some(StopSignal {
                reason: "patch_spiral",
                message: format!(
                    "Patch stuck on {} ({fail_count} failures, {total_attempts} attempts). Entering hot-file safe mode.",
                    file_path.display()
                ),
                action: "hot_file_safe_mode",
                injection: format!(
                    "[SYSTEM] You have attempted to edit {} {total_attempts} times ({fail_count} failures). \
                     Enter HOT-FILE SAFE MODE:\n\
                     1. Call read_file on {} to refresh the exact current content.\n\
                     2. Make ONE small targeted edit using a unique snippet / anchor.\n\
                     3. Re-run the narrowest verifier or compiler check before editing again.\n\
                     Do NOT rewrite the whole file. Do NOT chain multiple blind edits. \
                     If you cannot identify a unique target, inspect the verifier or failing check first.",
                    file_path.display(),
                    file_path.display(),
                ),
            });
        }
        None
    }

    /// Record a bash tool result. Returns a StopSignal after
    /// `max_consecutive_bash_failures` consecutive non-zero exits.
    /// A successful exit resets the counter.
    pub fn record_bash_result(&mut self, exit_code: i32, command: &str) -> Option<StopSignal> {
        if exit_code == 0 {
            self.consecutive_bash_failures = 0;
            return None;
        }
        self.consecutive_bash_failures += 1;
        if self.consecutive_bash_failures >= self.max_consecutive_bash_failures {
            self.consecutive_bash_failures = 0;
            let short_cmd: String = command.chars().take(80).collect();
            return Some(StopSignal {
                reason: "bash_failure_loop",
                message: format!(
                    "{} consecutive bash failures (last: `{short_cmd}`). Injecting correction.",
                    self.max_consecutive_bash_failures
                ),
                action: "inject_correction",
                injection: format!(
                    "[SYSTEM] You have run {n} bash commands in a row and every one failed. \
                     This approach is not working. STOP and reconsider:\n\
                     1. Read the error output above carefully — what exactly is failing?\n\
                     2. Try a completely different approach or simpler command.\n\
                     3. If you are stuck on a compilation error, check the exact error message \
                        and fix only that specific issue.\n\
                     Do NOT keep retrying the same failing command.",
                    n = self.max_consecutive_bash_failures
                ),
            });
        }
        None
    }

    pub fn check_greeting(&self, content: &str, has_tool_calls_this_turn: bool) -> Option<StopSignal> {
        if !self.enable_greeting_detection || !has_tool_calls_this_turn {
            return None;
        }
        let lc = content.to_lowercase();
        let patterns = [
            "how can i help",
            "what would you like",
            "what can i do for you",
            "how can i assist",
            "hello! i'm ready",
            "hi there! what",
        ];
        if !patterns.iter().any(|p| lc.contains(p)) {
            return None;
        }
        Some(StopSignal {
            reason: "greeting_regression",
            message: "Model output a greeting mid-task (lost context).".into(),
            action: "inject_correction",
            injection: "[SYSTEM] You output a greeting instead of completing the task. Look at the conversation above — there is still work to do. Continue where you left off. Do NOT restart the conversation.".into(),
        })
    }

    /// Call after a successful read_file — unblocks the file for patching.
    pub fn record_read(&mut self, file_path: &str) {
        self.patch_blocked_files.remove(&normalize_path(file_path));
    }

    pub fn new_turn(&mut self) {
        self.patch_failures.clear();
        self.patch_attempts.clear();
        // patch_blocked_files intentionally NOT cleared across turns — the
        // block lifts only after a successful read_file on the same file.
    }
}

impl Default for EarlyStopDetector {
    fn default() -> Self {
        Self::new()
    }
}
fn normalize_path(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in Path::new(path).components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::EarlyStopDetector;

    #[test]
    fn patch_spiral_enters_hot_file_safe_mode() {
        let mut detector = EarlyStopDetector::new();
        let mut signal = None;
        for _ in 0..detector.max_patch_failures {
            signal = detector.record_patch_result("src/lib.rs", false, "old", "new");
        }
        let signal = signal.expect("expected hot-file signal");
        assert_eq!(signal.reason, "patch_spiral");
        assert_eq!(signal.action, "hot_file_safe_mode");
        assert!(signal.injection.contains("HOT-FILE SAFE MODE"));
        assert!(signal.injection.contains("Do NOT rewrite the whole file"));
    }

    #[test]
    fn read_unblocks_hot_file_safe_mode() {
        let mut detector = EarlyStopDetector::new();
        for _ in 0..detector.max_patch_failures {
            let _ = detector.record_patch_result("src/lib.rs", false, "old", "new");
        }
        assert!(detector.check_patch_blocked("src/lib.rs").is_some());
        detector.record_read("./src/lib.rs");
        assert!(detector.check_patch_blocked("src/lib.rs").is_none());
    }

    /// A successful patch decrements the failure count — recovery should be
    /// possible without hitting the spiral threshold.
    #[test]
    fn successful_patch_decrements_failure_count() {
        let mut detector = EarlyStopDetector::new();
        // 3 fails — short of the 4 threshold.
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        // A successful patch lowers fail count by 1.
        assert!(detector.record_patch_result("a.rs", true, "x", "y").is_none(),
            "successful patch must not produce a stop signal");
        // Now another failure shouldn't trigger spiral yet — fail count is back at 3.
        assert!(detector.record_patch_result("a.rs", false, "x", "y").is_none(),
            "after recovery, one more failure must not yet trigger spiral");
    }

    /// A no-op patch (old_str == new_str) does NOT count as success, so it
    /// shouldn't decrement the failure counter. Anti-regression for the
    /// case where the model patches text → identical text in a loop.
    #[test]
    fn noop_patch_does_not_recover_failures() {
        let mut detector = EarlyStopDetector::new();
        // 3 fails.
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        let _ = detector.record_patch_result("a.rs", false, "x", "y");
        // No-op patch: success=true but old_str==new_str → counts as failure.
        let signal = detector.record_patch_result("a.rs", true, "same", "same");
        assert!(signal.is_some(),
            "no-op patch on top of 3 failures must trigger spiral (still 4 effective failures)");
    }

    /// Bash failure loop: N consecutive non-zero exits triggers the
    /// inject_correction signal. A successful exit resets the counter.
    #[test]
    fn bash_failure_loop_triggers_correction() {
        let mut d = EarlyStopDetector::new();
        let n = d.max_consecutive_bash_failures;
        for _ in 0..(n - 1) {
            let s = d.record_bash_result(1, "false");
            assert!(s.is_none(), "{} failures not yet enough to trigger", n - 1);
        }
        let s = d.record_bash_result(1, "false").expect("Nth failure must trigger");
        assert_eq!(s.reason, "bash_failure_loop");
    }

    /// A successful bash exit resets the consecutive-failure counter.
    #[test]
    fn bash_success_resets_failure_counter() {
        let mut d = EarlyStopDetector::new();
        let n = d.max_consecutive_bash_failures;
        for _ in 0..(n - 1) {
            let _ = d.record_bash_result(1, "false");
        }
        // Success resets.
        let _ = d.record_bash_result(0, "true");
        // Now the (n-1)+1 isn't a loop — one fresh failure must NOT trigger.
        assert!(d.record_bash_result(1, "false").is_none(),
            "after a success, one fresh failure must not re-trigger immediately");
    }

    /// Greeting detection only fires when there are tool calls this turn
    /// (otherwise a user "hi" → assistant greeting is expected behavior).
    #[test]
    fn greeting_only_detected_during_active_turn() {
        let d = EarlyStopDetector::new();
        // Without active tool calls — greeting is fine.
        assert!(d.check_greeting("hello! how can i help", false).is_none(),
            "greeting must NOT fire without tool calls this turn");
        // With active tool calls — same greeting is suspicious.
        let s = d.check_greeting("hello! how can i help", true);
        assert!(s.is_some(), "greeting mid-task must fire");
        assert_eq!(s.unwrap().reason, "greeting_regression");
    }

    /// Greeting detection ignores irrelevant content.
    #[test]
    fn greeting_detection_does_not_false_positive() {
        let d = EarlyStopDetector::new();
        let s = d.check_greeting("Editing the auth module to add input validation.", true);
        assert!(s.is_none(), "task narration must not be flagged as greeting");
    }

    /// Disabled greeting detection short-circuits even with a clear greeting.
    #[test]
    fn disabled_greeting_detection_short_circuits() {
        let mut d = EarlyStopDetector::new();
        d.enable_greeting_detection = false;
        assert!(d.check_greeting("hello! how can i help", true).is_none());
    }

    /// `new_turn` resets per-turn counters but PRESERVES patch_blocked_files
    /// — the lock only lifts on a successful read.
    #[test]
    fn new_turn_preserves_patch_block() {
        let mut d = EarlyStopDetector::new();
        for _ in 0..d.max_patch_failures {
            let _ = d.record_patch_result("locked.rs", false, "x", "y");
        }
        assert!(d.check_patch_blocked("locked.rs").is_some(),
            "file should be locked after spiral");
        d.new_turn();
        assert!(d.check_patch_blocked("locked.rs").is_some(),
            "new_turn must NOT lift the block — only successful read does");
    }

    /// Repetition detection fires when the buffer's tail contains many
    /// instances of the trailing window — e.g. a model stuck emitting the
    /// same short phrase repeatedly.
    #[test]
    fn repetition_loop_detected_in_long_buffer() {
        let d = EarlyStopDetector::new();
        // 500 chars; tail = last 200; pattern = last 50; "abcd" repeats so
        // the 50-char pattern is itself a repeating sequence found ≥3 times
        // when stepping by 1 in the tail (self-overlap is permitted).
        let buf = "abcd".repeat(125); // 500 chars
        let s = d.check_repetition(&buf);
        assert!(s.is_some(), "long buffer of self-similar text must trigger");
        assert_eq!(s.unwrap().reason, "repetition_loop");
    }

    /// Short buffers never trigger repetition detection (under threshold).
    #[test]
    fn repetition_not_detected_in_short_buffer() {
        let d = EarlyStopDetector::new();
        let buf = "hello".repeat(5); // ~25 chars, far below the 400-char min
        assert!(d.check_repetition(&buf).is_none());
    }
}
