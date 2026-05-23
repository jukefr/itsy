//! Detects repetition loops, patch
//! spirals, and greeting regression in streamed model output.

use std::collections::{HashMap, HashSet};

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
    patch_failures: HashMap<String, u32>,
    patch_attempts: HashMap<String, u32>,
    /// Files blocked from patching after a stuck spiral.
    /// Cleared when the model successfully reads the file — forces
    /// re-read before re-patch rather than a permanent session ban.
    patch_blocked_files: HashSet<String>,
}

impl EarlyStopDetector {
    pub fn new() -> Self {
        Self {
            repetition_threshold: 3,
            repetition_window_chars: 200,
            max_patch_failures: 4,
            max_response_tokens: 8192,
            enable_greeting_detection: true,
            patch_failures: HashMap::new(),
            patch_attempts: HashMap::new(),
            patch_blocked_files: HashSet::new(),
        }
    }

    /// Returns a hard error if this file has been permanently blocked after
    /// a patch spiral. Call this BEFORE executing a patch tool call.
    pub fn check_patch_blocked(&self, file_path: &str) -> Option<StopSignal> {
        if !self.patch_blocked_files.contains(file_path) {
            return None;
        }
        Some(StopSignal {
            reason: "patch_blocked",
            message: format!("Patch on {file_path} blocked — file was stuck-patched earlier."),
            action: "hard_block",
            injection: format!(
                "[LOOP DETECTED] patch on '{file_path}' is blocked. You already failed repeatedly \
                 on this file and were told to stop. Do NOT call patch on this file again. \
                 Use a completely different tool or target a different file."
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
        let attempts = self.patch_attempts.entry(file_path.to_string()).or_insert(0);
        *attempts += 1;
        let total_attempts = *attempts;

        let is_noop = success && !old_str.is_empty() && !new_str.is_empty() && old_str == new_str;
        if success && !is_noop {
            if let Some(f) = self.patch_failures.get_mut(file_path) {
                *f = f.saturating_sub(1);
            }
            return None;
        }
        let fails = self.patch_failures.entry(file_path.to_string()).or_insert(0);
        *fails += 1;
        let fail_count = *fails;
        if fail_count >= self.max_patch_failures || total_attempts >= 6 {
            self.patch_failures.remove(file_path);
            self.patch_attempts.remove(file_path);
            self.patch_blocked_files.insert(file_path.to_string());
            return Some(StopSignal {
                reason: "patch_spiral",
                message: format!("Patch stuck on {file_path} ({fail_count} failures, {total_attempts} attempts). Switching to rewrite."),
                action: "rewrite_file",
                injection: format!(
                    "[SYSTEM] You have attempted to patch {file_path} {total_attempts} times ({fail_count} failures). The file is likely corrupted or your patches don't match. STOP using patch. Instead:\n\
                     1. Use read_file to see the current state\n\
                     2. Decide what the ENTIRE file should contain\n\
                     3. Use write_file to rewrite it completely from scratch\n\
                     Do NOT attempt another patch on this file."
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
        self.patch_blocked_files.remove(file_path);
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
