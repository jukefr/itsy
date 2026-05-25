//! Evaluator-phase helpers lifted from `bin/itsy.rs` so they can be
//! unit-tested. The evaluator is the adversarial second pass that re-checks
//! a generator-closed contract; it runs in a tool-restricted mode (read +
//! bash only), so these helpers are the entire I/O surface.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct BashOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Spawn `bash -c <command>` in `cwd` with a hard timeout. Streams stdout/stderr
/// into memory, returning whatever has been captured at the moment the child
/// either exits or is killed for exceeding `timeout`.
///
/// The previous implementation used `read_to_end` inline in the polling loop,
/// which blocked until the child closed its pipes (i.e. exited) — so the
/// timeout never actually fired. Now reads happen on background threads so
/// the main loop can poll `try_wait` and the timeout independently.
pub fn run_bash_with_timeout(command: &str, cwd: &str, timeout: Duration) -> BashOutput {
    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return BashOutput { exit_code: -1, stdout: String::new(), stderr: format!("error: {e}") },
    };

    // Move the read-ends to threads so neither can block the polling loop.
    let mut child_stdout = child.stdout.take().expect("piped stdout");
    let mut child_stderr = child.stderr.take().expect("piped stderr");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let exit_code: i32;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait(); // reap the killed child
                    exit_code = -1;
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                // Reader threads will see EOF when child handles drop.
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return BashOutput { exit_code: -1, stdout: String::new(), stderr: format!("error: {e}") };
            }
        }
    }

    // Child has either exited or been killed — pipes will close, threads finish.
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    if timed_out {
        BashOutput { exit_code, stdout: String::new(), stderr: "timed out".into() }
    } else {
        BashOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        }
    }
}

/// Run a bash command for the evaluator (read-only intent, 15s timeout).
/// Returns a short single-string summary of the result with sections capped
/// at 2000 chars total so the model never gets a wall of text.
pub fn evaluator_run_bash(command: &str, cwd: &str) -> String {
    let result = run_bash_with_timeout(command, cwd, Duration::from_secs(15));
    let mut s = format!("exit_code={}", result.exit_code);
    if !result.stdout.is_empty() {
        s.push_str(&format!("\nstdout:\n{}", result.stdout.trim_end()));
    }
    if !result.stderr.is_empty() {
        s.push_str(&format!("\nstderr:\n{}", result.stderr.trim_end()));
    }
    if s.len() > 2000 {
        // Use char-boundary-safe truncation so multi-byte UTF-8 doesn't panic.
        let mut end = 2000;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[output truncated]");
    }
    s
}

/// Read a file for the evaluator. Caps at 3000 chars so a 50MB file
/// can't blow context.
pub fn evaluator_read_file(path: &str, cwd: &str) -> String {
    let p = if std::path::Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::path::Path::new(cwd).join(path)
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            if s.len() > 3000 {
                // Char-boundary-safe truncation.
                let mut end = 3000;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}\n[file truncated at 3000 chars]", &s[..end])
            } else {
                s
            }
        }
        Err(e) => format!("error reading {}: {e}", p.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run_bash_with_timeout ─────────────────────────────────────────────

    #[test]
    fn bash_success_returns_zero_exit_code() {
        let out = run_bash_with_timeout("true", ".", Duration::from_secs(5));
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty() || out.stdout.trim().is_empty());
    }

    #[test]
    fn bash_failure_returns_nonzero_exit_code() {
        let out = run_bash_with_timeout("false", ".", Duration::from_secs(5));
        assert_ne!(out.exit_code, 0);
    }

    #[test]
    fn bash_stdout_is_captured() {
        let out = run_bash_with_timeout("echo hello world", ".", Duration::from_secs(5));
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello world"), "got stdout={:?}", out.stdout);
    }

    #[test]
    fn bash_stderr_is_captured_separately() {
        let out = run_bash_with_timeout("echo err >&2", ".", Duration::from_secs(5));
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.contains("err"), "stderr should capture redirected output");
        assert!(out.stdout.is_empty() || !out.stdout.contains("err"),
            "stdout must NOT see what went to stderr");
    }

    /// A command that exceeds the timeout returns exit_code=-1 and the
    /// "timed out" marker — anti-regression for blocking-forever bugs.
    #[test]
    fn bash_timeout_kills_long_running_command() {
        let started = Instant::now();
        let out = run_bash_with_timeout("sleep 5", ".", Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert_eq!(out.exit_code, -1);
        assert!(out.stderr.contains("timed out"));
        assert!(elapsed < Duration::from_secs(2),
            "must kill quickly; took {elapsed:?}");
    }

    /// `cwd` is respected — pwd from inside the command sees it.
    #[test]
    fn bash_respects_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let out = run_bash_with_timeout("pwd", &cwd, Duration::from_secs(5));
        assert_eq!(out.exit_code, 0);
        // pwd may resolve symlinks (e.g. /tmp -> /private/tmp on macOS).
        // Just check the trailing component matches.
        let last = std::path::Path::new(&cwd).file_name().unwrap().to_string_lossy().to_string();
        assert!(out.stdout.contains(&last), "pwd must reflect cwd; got {:?}", out.stdout);
    }

    // ── evaluator_run_bash ─────────────────────────────────────────────────

    /// `evaluator_run_bash` formats output with the exit_code prefix
    /// and labelled stdout/stderr sections.
    #[test]
    fn evaluator_bash_formats_output_with_labels() {
        let s = evaluator_run_bash("echo hi && echo bye >&2", ".");
        assert!(s.starts_with("exit_code=0"), "must start with exit_code; got {s}");
        assert!(s.contains("stdout:"), "must label stdout; got {s}");
        assert!(s.contains("stderr:"), "must label stderr; got {s}");
        assert!(s.contains("hi"));
        assert!(s.contains("bye"));
    }

    /// Empty stdout/stderr sections are omitted from the output (not "stdout:\n").
    #[test]
    fn evaluator_bash_omits_empty_sections() {
        let s = evaluator_run_bash("true", ".");
        assert!(s.starts_with("exit_code=0"));
        // No stray "stdout:" or "stderr:" labels when the streams were empty.
        assert!(!s.contains("stdout:\n\n"));
        assert!(!s.contains("stderr:\n\n"));
    }

    /// Output above 2000 chars is truncated with a marker.
    #[test]
    fn evaluator_bash_truncates_at_2000_chars() {
        // Generate a large stdout via `seq` — 10k lines of digits.
        let s = evaluator_run_bash("seq 1 10000", ".");
        assert!(s.len() <= 2000 + "\n[output truncated]".len() + 8,
            "must cap at 2000 chars + marker; got {} chars", s.len());
        assert!(s.contains("output truncated"));
    }

    /// Exit code is propagated from the child to the formatted output.
    #[test]
    fn evaluator_bash_propagates_exit_code() {
        let s = evaluator_run_bash("exit 7", ".");
        assert!(s.contains("exit_code=7"), "got {s}");
    }

    /// Multi-byte UTF-8 truncation doesn't panic (char-boundary-safe).
    /// Anti-regression: a naive `truncate(2000)` would panic on a UTF-8 boundary.
    #[test]
    fn evaluator_bash_handles_multibyte_truncation() {
        let s = evaluator_run_bash("for i in $(seq 1 500); do echo 'éééééééééé'; done", ".");
        // Must not panic; must be valid UTF-8 (otherwise format! / String ops fail).
        assert!(!s.is_empty());
        assert!(s.is_char_boundary(0));
    }

    // ── evaluator_read_file ────────────────────────────────────────────────

    #[test]
    fn evaluator_read_resolves_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("rel.txt"), "hello").unwrap();
        let s = evaluator_read_file("rel.txt", &tmp.path().to_string_lossy());
        assert_eq!(s, "hello");
    }

    #[test]
    fn evaluator_read_passes_absolute_paths_through() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("abs.txt");
        std::fs::write(&path, "world").unwrap();
        let s = evaluator_read_file(&path.to_string_lossy(), "/nonexistent/cwd");
        assert_eq!(s, "world", "absolute path must NOT be joined with cwd");
    }

    /// Missing file produces a `error reading <path>:` message — not a panic.
    #[test]
    fn evaluator_read_missing_file_returns_error_message() {
        let s = evaluator_read_file("missing.txt", "/nonexistent");
        assert!(s.starts_with("error reading"), "got: {s}");
    }

    /// Files larger than 3000 chars are truncated with a marker.
    #[test]
    fn evaluator_read_truncates_at_3000_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        std::fs::write(&path, "x".repeat(10_000)).unwrap();
        let s = evaluator_read_file(&path.to_string_lossy(), "/");
        assert!(s.contains("[file truncated"), "must mark truncation");
        // 3000 chars + the truncation marker (~30 chars).
        assert!(s.len() <= 3100, "got {} chars total", s.len());
    }

    /// Multi-byte UTF-8 truncation is char-boundary-safe.
    #[test]
    fn evaluator_read_handles_multibyte_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("utf8.txt");
        std::fs::write(&path, "éé".repeat(2000)).unwrap();
        let s = evaluator_read_file(&path.to_string_lossy(), "/");
        // Must not panic; must be valid UTF-8.
        assert!(s.contains("[file truncated"));
        // Verify char_boundary safety: re-read indices.
        let _ = s.chars().count();
    }
}
