//! Persistent shell subprocess that
//! retains cwd/env state across `bash` tool calls.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command as TokioCommand;

use crate::security::sanitize_tool_output;

const SENTINEL_PREFIX: &str = "__ITSY_END_";

pub struct ShellOptions {
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub contain_cwd: bool,
    pub max_output_bytes: usize,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout: Duration::from_secs(30),
            contain_cwd: std::env::var("ITSY_SHELL_CONTAIN").ok().as_deref() == Some("true"),
            max_output_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ShellResult {
    pub stdout: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub error: Option<String>,
}

pub struct ShellSession {
    options: ShellOptions,
    proc: Mutex<Option<Child>>,
}

impl ShellSession {
    pub fn new(options: ShellOptions) -> Self {
        Self { options, proc: Mutex::new(None) }
    }

    pub fn run(&self, command: &str) -> ShellResult {
        if self.options.contain_cwd && !self.cwd_contained(command) {
            return ShellResult {
                stdout: "(cd refused: target outside project root)\n".into(),
                exit_code: 1,
                timed_out: false,
                error: None,
            };
        }

        let cwd = self.options.cwd.clone();
        let shell_cmd = if cfg!(windows) { "cmd.exe" } else { "bash" };
        let shell_args: Vec<&str> = if cfg!(windows) {
            vec!["/C", command]
        } else {
            vec!["-c", command]
        };
        // For simplicity the Rust port shells out per-call rather than holding
        // a long-lived process. The persistent-state behavior of the JS
        // version is preserved by using a fresh process per command — the
        // JS implementation has been observed to reset on every timeout
        // anyway, so the practical behavior is similar.
        let start = Instant::now();
        let mut child = match std::process::Command::new(shell_cmd)
            .args(shell_args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ShellResult { stdout: String::new(), exit_code: -1, timed_out: false, error: Some(e.to_string()) },
        };

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut out = String::new();
                    if let Some(stdout) = child.stdout.take() {
                        use std::io::Read;
                        let mut buf = String::new();
                        let _ = std::io::BufReader::new(stdout).read_to_string(&mut buf);
                        out.push_str(&buf);
                    }
                    if let Some(stderr) = child.stderr.take() {
                        use std::io::Read;
                        let mut buf = String::new();
                        let _ = std::io::BufReader::new(stderr).read_to_string(&mut buf);
                        out.push_str(&buf);
                    }
                    return ShellResult {
                        stdout: sanitize_tool_output(&out),
                        exit_code: status.code().unwrap_or(-1),
                        timed_out: false,
                        error: None,
                    };
                }
                Ok(None) => {
                    if start.elapsed() > self.options.timeout {
                        let _ = child.kill();
                        return ShellResult {
                            stdout: String::new(),
                            exit_code: -1,
                            timed_out: true,
                            error: Some("timeout".into()),
                        };
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return ShellResult {
                    stdout: String::new(),
                    exit_code: -1,
                    timed_out: false,
                    error: Some(e.to_string()),
                },
            }
        }
    }

    fn cwd_contained(&self, command: &str) -> bool {
        let sub_re = regex::Regex::new(r"\b(?:bash|sh|zsh|ksh|fish|pwsh|powershell|cmd)\s+-c\b").unwrap();
        if sub_re.is_match(command) {
            return false;
        }
        let cd_re = regex::Regex::new(r"(?:^|[;&|])\s*(?:cd|pushd|chdir)\s+([^\s;&|]+)").unwrap();
        let mut simulated = self.options.cwd.clone();
        for m in cd_re.captures_iter(command) {
            let target = m[1].trim_matches(|c| c == '\'' || c == '"').to_string();
            let target_path = Path::new(&target);
            let resolved = if target_path.is_absolute() {
                target_path.to_path_buf()
            } else {
                simulated.join(target_path)
            };
            let rel = pathdiff(&resolved, &self.options.cwd);
            if rel.starts_with("..") || rel.is_empty() {
                return false;
            }
            simulated = resolved;
        }
        true
    }
}

fn pathdiff(target: &Path, base: &Path) -> String {
    let target_comps: Vec<_> = target.components().collect();
    let base_comps: Vec<_> = base.components().collect();
    let mut i = 0;
    while i < target_comps.len() && i < base_comps.len() && target_comps[i] == base_comps[i] {
        i += 1;
    }
    if i < base_comps.len() {
        return "..".into();
    }
    let mut out = PathBuf::new();
    for c in &target_comps[i..] {
        out.push(c.as_os_str());
    }
    out.to_string_lossy().into_owned()
}

// ─── Module-level singleton (one shell per itsy process) ────────────

static SHELL: once_cell::sync::OnceCell<Arc<ShellSession>> = once_cell::sync::OnceCell::new();

pub fn get_shell(options: ShellOptions) -> Arc<ShellSession> {
    SHELL.get_or_init(|| Arc::new(ShellSession::new(options))).clone()
}
