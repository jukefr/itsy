//! True persistent shell session — holds ONE long-lived bash subprocess and
//! demuxes per-command output via sentinel markers. This is what makes
//! `cd src && ls` behave like a user would expect across sequential tool
//! calls: cwd, exported env, shell variables, sourced scripts all persist.
//!
//! Architecture:
//!   * Spawn `bash --norc --noprofile -i` (or `cmd.exe /Q /K` on Windows) with
//!     piped stdin/stdout/stderr.
//!   * A background tokio task drains stdout + stderr into a shared buffer.
//!   * Each `run()` call writes the user command followed by a printf that
//!     emits `__ITSY_END_<hex>_<exit>_` to mark end-of-output. The dispatcher
//!     scans for this sentinel and returns everything before it.
//!   * Per-command timeout. On timeout we SIGKILL the shell and mark it dead;
//!     the next `run()` call will respawn a fresh process. (Trying to recover
//!     a half-written buffer is hopeless — sentinel may never arrive.)
//!   * Optional cwd containment: parses `cd`/`pushd`/`chdir` and refuses to
//!     escape the project root. Sub-shell escapes (`bash -c ...`) are refused.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rand::RngCore;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Notify};

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
            contain_cwd: crate::settings::get().shell_contain,
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

struct PendingCmd {
    sentinel: String,
    tx: oneshot::Sender<ShellResult>,
}

struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    buffer: String,
    queue: Vec<PendingCmd>,
    dead: bool,
}

pub struct ShellSession {
    options: ShellOptions,
    root_dir: PathBuf,
    inner: Mutex<Inner>,
    notify: Arc<Notify>,
}

impl ShellSession {
    pub fn new(options: ShellOptions) -> Self {
        let root_dir = std::fs::canonicalize(&options.cwd).unwrap_or_else(|_| options.cwd.clone());
        Self {
            options,
            root_dir,
            inner: Mutex::new(Inner {
                proc: None,
                stdin: None,
                buffer: String::new(),
                queue: Vec::new(),
                dead: true,
            }),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Spawn (or respawn) the persistent shell. Idempotent.
    pub async fn start(self: &Arc<Self>) -> bool {
        {
            let inner = self.inner.lock();
            if inner.proc.is_some() && !inner.dead {
                return true;
            }
        }

        let (cmd, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd.exe", vec!["/Q", "/K", "echo off & prompt $G"])
        } else {
            ("bash", vec!["--norc", "--noprofile", "-i"])
        };

        let mut child = match Command::new(cmd)
            .args(&args)
            .current_dir(&self.options.cwd)
            .env("PS1", "")
            .env("PROMPT_COMMAND", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                self.inner.lock().dead = true;
                return false;
            }
        };

        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                self.inner.lock().dead = true;
                return false;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        {
            let mut inner = self.inner.lock();
            inner.proc = Some(child);
            inner.stdin = Some(stdin);
            inner.buffer.clear();
            inner.dead = false;
        }

        // Background readers — one per stream — push into shared buffer
        // and ping the dispatcher to re-check for sentinels.
        if let Some(out) = stdout {
            self.spawn_reader(out);
        }
        if let Some(err) = stderr {
            self.spawn_reader(err);
        }
        true
    }

    fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(self: &Arc<Self>, reader: R) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut buf = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let n = match buf.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                let _ = n;
                let Some(strong) = weak.upgrade() else { break };
                {
                    let mut inner = strong.inner.lock();
                    inner.buffer.push_str(&line);
                    // Hard cap on buffered output (4x the configured max).
                    // Trim from the start, keep the tail intact so we don't
                    // slice through an in-flight sentinel.
                    let cap = strong.options.max_output_bytes.saturating_mul(4);
                    if inner.buffer.len() > cap {
                        let keep = strong.options.max_output_bytes.saturating_mul(2);
                        let start = inner.buffer.len() - keep;
                        inner.buffer = inner.buffer.split_off(start);
                    }
                }
                strong.drain();
                strong.notify.notify_waiters();
            }
            if let Some(strong) = weak.upgrade() {
                strong.inner.lock().dead = true;
                strong.notify.notify_waiters();
            }
        });
    }

    /// Run a command in the persistent shell. Output is sanitized
    /// (ANSI stripped, secrets redacted) before returning.
    pub async fn run(self: &Arc<Self>, command: &str) -> ShellResult {
        // Containment check first — never even writes to the shell.
        if self.options.contain_cwd {
            if let Err(msg) = self.check_containment(command) {
                return ShellResult { stdout: msg, exit_code: 1, ..Default::default() };
            }
        }

        // Lazy / auto-restart.
        let need_start = {
            let inner = self.inner.lock();
            inner.dead || inner.proc.is_none() || inner.stdin.is_none()
        };
        if need_start && !self.start().await {
            return ShellResult {
                exit_code: -1,
                error: Some("shell unavailable".into()),
                ..Default::default()
            };
        }

        let sentinel = make_sentinel();
        let wrapped = if cfg!(windows) {
            format!("{command}\r\n@echo {sentinel}_%errorlevel%_\r\n")
        } else {
            format!("{command}\nprintf '\\n{sentinel}_%d_\\n' $?\n")
        };

        let (tx, rx) = oneshot::channel();
        let pending = PendingCmd { sentinel: sentinel.clone(), tx };
        self.inner.lock().queue.push(pending);

        // Write to stdin (briefly take it across await).
        let mut stdin_taken = self.inner.lock().stdin.take();
        if let Some(mut s) = stdin_taken.take() {
            let write_res = s.write_all(wrapped.as_bytes()).await;
            let flush_res = s.flush().await;
            self.inner.lock().stdin = Some(s);
            if write_res.is_err() || flush_res.is_err() {
                self.evict_sentinel(&sentinel);
                return ShellResult {
                    exit_code: -1,
                    error: Some("write failed".into()),
                    ..Default::default()
                };
            }
        } else {
            self.evict_sentinel(&sentinel);
            return ShellResult {
                exit_code: -1,
                error: Some("no stdin".into()),
                ..Default::default()
            };
        }

        // Sentinel may have arrived between push and write — drain once more.
        self.drain();

        match tokio::time::timeout(self.options.timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => ShellResult {
                exit_code: -1,
                error: Some("shell closed before reply".into()),
                ..Default::default()
            },
            Err(_) => {
                // Timeout — nuke the shell and fail all pending commands.
                self.evict_sentinel(&sentinel);
                self.kill_and_reset("timeout — shell reset");
                ShellResult {
                    exit_code: -1,
                    timed_out: true,
                    error: Some("timeout — shell reset".into()),
                    ..Default::default()
                }
            }
        }
    }

    /// Reset the shell — kill and prepare for a fresh spawn on next `run`.
    pub fn reset(self: &Arc<Self>) {
        self.kill_and_reset("shell reset");
    }

    /// Tear down the shell. Idempotent.
    pub fn stop(self: &Arc<Self>) {
        self.kill_and_reset("shell stopped");
    }

    // ── Internals ────────────────────────────────────────────────────

    fn drain(self: &Arc<Self>) {
        loop {
            let mut inner = self.inner.lock();
            if inner.queue.is_empty() {
                return;
            }
            let head_sentinel = inner.queue[0].sentinel.clone();
            // Look for `<sentinel>_<exit>_` anywhere in the buffer.
            let needle = format!("{head_sentinel}_");
            let Some(start) = inner.buffer.find(&needle) else { return };
            // Parse the exit code after the second underscore.
            let after = &inner.buffer[start + needle.len()..];
            let Some(end_underscore) = after.find('_') else { return };
            let exit_str = &after[..end_underscore];
            let Ok(exit_code) = exit_str.parse::<i32>() else {
                // Malformed — drop the sentinel from the buffer to avoid looping.
                let drop_to = start + needle.len() + end_underscore + 1;
                let buf_len = inner.buffer.len();
                inner.buffer = inner.buffer.split_off(drop_to.min(buf_len));
                continue;
            };

            let stdout_raw = inner.buffer[..start].trim_end_matches(['\n', '\r']).to_string();
            let drop_to = start + needle.len() + end_underscore + 1;
            let buf_len = inner.buffer.len();
            inner.buffer = inner.buffer.split_off(drop_to.min(buf_len));
            // Strip leading newline that the printf prelude added.
            while inner.buffer.starts_with('\n') || inner.buffer.starts_with('\r') {
                inner.buffer.remove(0);
            }

            let pending = inner.queue.remove(0);
            drop(inner);

            let clean = sanitize_tool_output(&stdout_raw);
            let _ = pending.tx.send(ShellResult {
                stdout: clean,
                exit_code,
                timed_out: false,
                error: None,
            });
        }
    }

    fn evict_sentinel(&self, sentinel: &str) {
        let mut inner = self.inner.lock();
        if let Some(pos) = inner.queue.iter().position(|q| q.sentinel == sentinel) {
            inner.queue.remove(pos);
        }
    }

    fn kill_and_reset(&self, reason: &str) {
        let mut inner = self.inner.lock();
        if let Some(mut child) = inner.proc.take() {
            let _ = child.start_kill();
        }
        inner.stdin = None;
        inner.buffer.clear();
        inner.dead = true;
        let queue = std::mem::take(&mut inner.queue);
        drop(inner);
        for pending in queue {
            let _ = pending.tx.send(ShellResult {
                exit_code: -1,
                error: Some(reason.into()),
                ..Default::default()
            });
        }
    }

    fn check_containment(&self, command: &str) -> Result<(), String> {
        let sub_re = regex::Regex::new(r"\b(?:bash|sh|zsh|ksh|fish|pwsh|powershell|cmd)\s+-c\b").unwrap();
        if sub_re.is_match(command) {
            return Err("(refused: -c sub-shells bypass cwd containment)\n".into());
        }
        let cd_re = regex::Regex::new(r"(?:^|[;&|])\s*(?:cd|pushd|chdir)\s+([^\s;&|]+)").unwrap();
        let mut simulated = self.options.cwd.clone();
        for cap in cd_re.captures_iter(command) {
            let target = cap[1].trim_matches(|c| c == '\'' || c == '"').to_string();
            let target_path = Path::new(&target);
            let resolved = if target_path.is_absolute() {
                target_path.to_path_buf()
            } else {
                simulated.join(target_path)
            };
            let resolved_canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved.clone());
            if !resolved_canonical.starts_with(&self.root_dir) {
                return Err("(cd refused: target outside project root)\n".into());
            }
            simulated = resolved;
        }
        Ok(())
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(mut child) = inner.proc.take() {
            let _ = child.start_kill();
        }
    }
}

fn make_sentinel() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::from(SENTINEL_PREFIX);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ─── Module-level singleton (one shell per itsy process) ─────────────────

static SHELL: once_cell::sync::OnceCell<Arc<ShellSession>> = once_cell::sync::OnceCell::new();

pub fn get_shell(options: ShellOptions) -> Arc<ShellSession> {
    SHELL.get_or_init(|| Arc::new(ShellSession::new(options))).clone()
}

pub fn reset_shell() {
    if let Some(s) = SHELL.get() {
        s.reset();
    }
}
