//! itsy-bench — live benchmark dashboard for terminal-bench-2
//!
//! Two modes:
//!   itsy-bench run -t fix-git -m <model> [-n 3] [-c 1] [--job-name foo]
//!   itsy-bench watch <job-dir>

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::Deserialize;

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "itsy-bench", about = "Live benchmark dashboard for itsy")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch harbor and watch the job live.
    Run {
        /// Task names to include (repeatable).
        #[arg(short = 't', long = "task")]
        tasks: Vec<String>,

        /// Model name (must match /v1/models on the endpoint).
        #[arg(short = 'm', long)]
        model: String,

        /// Number of attempts per task.
        #[arg(short = 'n', long = "attempts", default_value = "3")]
        attempts: u32,

        /// Concurrent trials (match llama-server --parallel).
        #[arg(short = 'c', long = "concurrent", default_value = "1")]
        concurrent: u32,

        /// Job name (defaults to tasks-Nx-HHMMSS).
        #[arg(short = 'j', long)]
        job_name: Option<String>,

        /// Directory under which the job dir is created.
        #[arg(long, default_value = "jobs")]
        jobs_dir: PathBuf,

        /// Path to the itsy binary (defaults to ITSY_BINARY or release build).
        #[arg(long)]
        itsy_binary: Option<PathBuf>,
    },
    /// Watch an existing job directory.
    Watch {
        /// Path to the job directory (contains result.json + trial dirs).
        job_dir: PathBuf,
    },
}

// ─── Data model ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum TrialStatus {
    Pending,
    Running,
    Passed,
    Failed,
}

#[derive(Clone, Debug)]
struct ContractAssertion {
    _id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct Trial {
    name: String,
    task: String,
    status: TrialStatus,
    reward: Option<f64>,
    chats_dir: PathBuf,
    contract: Vec<ContractAssertion>,
}

#[derive(Clone, Debug, Default)]
struct JobStats {
    total: usize,
    completed: usize,
    errored: usize,
    running: usize,
    mean_reward: Option<f64>,
}

struct App {
    job_dir: PathBuf,
    job_name: String,
    model: String,
    started_at: Option<Instant>,
    wall_start: Option<DateTime<Utc>>,
    trials: Vec<Trial>,
    stats: JobStats,
    list_state: ListState,
    log_scroll: usize,
    log_lines: Vec<LogLine>,
    finished: bool,
    last_poll: Instant,
    // track last chat file count to avoid re-parsing unchanged logs
    last_log_trial: String,
    last_log_file_count: usize,
    status_msg: Option<(String, Instant)>,
    // Keep harbor alive for the lifetime of the TUI; Child::drop doesn't
    // kill the process but holding the handle here makes ownership explicit.
    #[allow(dead_code)]
    harbor_child: Option<std::process::Child>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            job_dir: PathBuf::new(),
            job_name: String::new(),
            model: String::new(),
            started_at: None,
            wall_start: None,
            trials: Vec::new(),
            stats: JobStats::default(),
            list_state: ListState::default(),
            log_scroll: usize::MAX,
            log_lines: Vec::new(),
            finished: false,
            last_poll: Instant::now() - Duration::from_secs(60),
            last_log_trial: String::new(),
            last_log_file_count: 0,
            status_msg: None,
            harbor_child: None,
        }
    }
}

impl App {
    fn selected_trial(&self) -> Option<&Trial> {
        self.list_state.selected().and_then(|i| self.trials.get(i))
    }

    fn scroll_down(&mut self) {
        let n = self.trials.len();
        if n == 0 {
            return;
        }
        let next = self.list_state.selected().map(|i| (i + 1).min(n - 1)).unwrap_or(0);
        self.list_state.select(Some(next));
        self.reset_log();
    }

    fn scroll_up(&mut self) {
        let prev = self.list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
        self.list_state.select(Some(prev));
        self.reset_log();
    }

    fn reset_log(&mut self) {
        self.log_scroll = self.log_lines.len().saturating_sub(1);
        self.last_log_file_count = 0; // force reload
    }

    fn log_scroll_down(&mut self) {
        if self.log_scroll.saturating_add(1) < self.log_lines.len() {
            self.log_scroll = self.log_scroll.saturating_add(1);
        }
    }

    fn log_scroll_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(1);
    }
}

// ─── Log line types ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum LogKind {
    Thinking,   // model reasoning_content
    Text,       // model content text
    ToolCall,   // ⚙ tool_name
    ToolOk,     // ✓ result first line
    ToolErr,    // ✗ error
    ToolBody,   // continuation lines of a tool result
    Nudge,      // system nudge/warning
    #[allow(dead_code)]
    Tokens,     // [tokens] summary
}

#[derive(Clone, Debug)]
struct LogLine {
    kind: LogKind,
    text: String,
}

impl LogLine {
    fn thinking(s: impl Into<String>) -> Self { Self { kind: LogKind::Thinking, text: s.into() } }
    fn text(s: impl Into<String>) -> Self { Self { kind: LogKind::Text, text: s.into() } }
    fn tool_call(s: impl Into<String>) -> Self { Self { kind: LogKind::ToolCall, text: s.into() } }
    fn tool_ok(s: impl Into<String>) -> Self { Self { kind: LogKind::ToolOk, text: s.into() } }
    fn tool_err(s: impl Into<String>) -> Self { Self { kind: LogKind::ToolErr, text: s.into() } }
    fn tool_body(s: impl Into<String>) -> Self { Self { kind: LogKind::ToolBody, text: s.into() } }
    fn nudge(s: impl Into<String>) -> Self { Self { kind: LogKind::Nudge, text: s.into() } }
    #[allow(dead_code)]
    fn tokens(s: impl Into<String>) -> Self { Self { kind: LogKind::Tokens, text: s.into() } }
}

#[derive(Deserialize, Default)]
struct ResultJson {
    started_at: Option<String>,
    finished_at: Option<serde_json::Value>,
    n_total_trials: Option<usize>,
    stats: Option<StatsJson>,
}

#[derive(Deserialize, Default)]
struct StatsJson {
    n_completed_trials: Option<usize>,
    n_errored_trials: Option<usize>,
    n_running_trials: Option<usize>,
    evals: Option<serde_json::Map<String, serde_json::Value>>,
}

fn poll_job_dir(app: &mut App) {
    app.last_poll = Instant::now();

    let result_path = app.job_dir.join("result.json");
    let result: ResultJson = std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if app.wall_start.is_none() {
        if let Some(ref s) = result.started_at {
            app.wall_start = DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc));
        }
    }

    app.finished = result.finished_at.as_ref().map(|v| !v.is_null()).unwrap_or(false);

    let stats = result.stats.unwrap_or_default();
    app.stats.total = result.n_total_trials.unwrap_or(0);
    app.stats.completed = stats.n_completed_trials.unwrap_or(0);
    app.stats.errored = stats.n_errored_trials.unwrap_or(0);
    app.stats.running = stats.n_running_trials.unwrap_or(0);

    if let Some(evals) = stats.evals {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for (_k, eval) in &evals {
            if let Some(metrics) = eval.get("metrics").and_then(|m| m.as_array()) {
                for m in metrics {
                    if let Some(mean) = m.get("mean").and_then(|v| v.as_f64()) {
                        sum += mean;
                        count += 1;
                    }
                }
            }
        }
        app.stats.mean_reward = if count > 0 { Some(sum / count as f64) } else { None };
    }

    let entries = match std::fs::read_dir(&app.job_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut trials: Vec<Trial> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let Some(sep) = dir_name.rfind("__") else { continue };
        let task_name = dir_name[..sep].to_string();

        let reward_path = path.join("verifier").join("reward.txt");
        let chats_dir = path.join("agent").join("chats");

        let (status, reward) = if reward_path.exists() {
            let r = std::fs::read_to_string(&reward_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            let passed = r.map(|v| v > 0.5).unwrap_or(false);
            (if passed { TrialStatus::Passed } else { TrialStatus::Failed }, r)
        } else if chats_dir.exists() {
            (TrialStatus::Running, None)
        } else {
            (TrialStatus::Pending, None)
        };

        // Preserve existing contract if already parsed (avoid re-parsing on every poll)
        let existing_contract = app.trials.iter()
            .find(|t| t.name == dir_name)
            .map(|t| t.contract.clone())
            .unwrap_or_default();

        let contract = if existing_contract.is_empty() {
            parse_contract(&chats_dir)
        } else {
            existing_contract
        };

        trials.push(Trial { name: dir_name, task: task_name, status, reward, chats_dir, contract });
    }

    trials.sort_by(|a, b| a.name.cmp(&b.name));

    let selected_name = app.selected_trial().map(|t| t.name.clone());
    app.trials = trials;

    if app.list_state.selected().is_none() && !app.trials.is_empty() {
        app.list_state.select(Some(0));
    }
    if let Some(name) = selected_name {
        if let Some(idx) = app.trials.iter().position(|t| t.name == name) {
            app.list_state.select(Some(idx));
        }
    }

    refresh_log_if_changed(app);
}

/// Parse contract assertions from the earliest chat file containing propose_contract.
fn parse_contract(chats_dir: &Path) -> Vec<ContractAssertion> {
    let mut files = chat_files(chats_dir);
    files.sort();
    for path in &files {
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) else { continue };
        let Some(messages) = data.pointer("/request/messages").and_then(|m| m.as_array()) else { continue };
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") { continue }
            let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) else { continue };
            for tc in tool_calls {
                if tc.pointer("/function/name").and_then(|n| n.as_str()) != Some("propose_contract") { continue }
                let Some(args_str) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) else { continue };
                let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) else { continue };
                let Some(assertions) = args.get("assertions").and_then(|a| a.as_array()) else { continue };
                return assertions.iter().map(|a| {
                    ContractAssertion {
                        _id: a.get("id").and_then(|i| i.as_str()).unwrap_or("?").to_string(),
                        text: a.get("text").and_then(|t| t.as_str()).unwrap_or("?").to_string(),
                    }
                }).collect();
            }
        }
    }
    Vec::new()
}

fn chat_files(chats_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(chats_dir)
        .ok()
        .map(|d| d.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect())
        .unwrap_or_default()
}

/// Reload the log for the selected trial only if the chat file count changed.
fn refresh_log_if_changed(app: &mut App) {
    let Some(trial) = app.selected_trial() else { return };
    let chats_dir = trial.chats_dir.clone();
    let trial_name = trial.name.clone();

    let file_count = chat_files(&chats_dir).len();
    let changed = trial_name != app.last_log_trial || file_count != app.last_log_file_count;

    if changed {
        let was_at_bottom = app.log_scroll.saturating_add(5) >= app.log_lines.len().max(1);
        app.log_lines = load_rich_log(&chats_dir);
        app.last_log_trial = trial_name;
        app.last_log_file_count = file_count;
        if was_at_bottom {
            app.log_scroll = app.log_lines.len().saturating_sub(1);
        }
    }
}

/// Build the rich log from the last chat JSON file (full conversation history).
fn load_rich_log(chats_dir: &Path) -> Vec<LogLine> {
    let mut files = chat_files(chats_dir);
    files.sort();
    let Some(last) = files.last() else { return Vec::new() };

    let Ok(content) = std::fs::read_to_string(last) else { return Vec::new() };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) else { return Vec::new() };
    let Some(messages) = data.pointer("/request/messages").and_then(|m| m.as_array()) else { return Vec::new() };

    let mut lines: Vec<LogLine> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "assistant" => {
                // Thinking — show first ~120 chars as a single summary line
                if let Some(thinking) = msg.get("reasoning_content").and_then(|r| r.as_str()) {
                    let trimmed = thinking.trim();
                    if !trimmed.is_empty() {
                        let preview: String = trimmed.lines()
                            .map(|l| l.trim())
                            .filter(|l| !l.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                        let truncated = if preview.chars().count() > 120 {
                            format!("{}…", preview.chars().take(120).collect::<String>())
                        } else {
                            preview
                        };
                        lines.push(LogLine::thinking(format!("~ {}", truncated)));
                    }
                }
                // Text content
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        for line in trimmed.lines() {
                            lines.push(LogLine::text(format!("▷ {}", line)));
                        }
                    }
                }
                // Tool calls
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let name = tc.pointer("/function/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        // Show arg summary for key tools
                        let arg_hint = tool_arg_hint(tc);
                        if arg_hint.is_empty() {
                            lines.push(LogLine::tool_call(format!("⚙ {}", name)));
                        } else {
                            lines.push(LogLine::tool_call(format!("⚙ {} {}", name, arg_hint)));
                        }
                    }
                }
            }
            "tool" => {
                if let Some(raw) = msg.get("content").and_then(|c| c.as_str()) {
                    let result = serde_json::from_str::<serde_json::Value>(raw)
                        .ok()
                        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(|s| s.to_string()))
                        .unwrap_or_else(|| raw.to_string());

                    let is_err = result.starts_with("Error") || result.contains("Exit code")
                        || result.starts_with("[LOOP") || result.starts_with("[SYSTEM]");

                    let result_lines: Vec<&str> = result.lines().collect();
                    let first = result_lines.first().unwrap_or(&"");
                    if is_err {
                        lines.push(LogLine::tool_err(format!("✗ {}", first)));
                    } else {
                        lines.push(LogLine::tool_ok(format!("✓ {}", first)));
                    }
                    // Show up to 3 continuation lines
                    for l in result_lines.iter().skip(1).take(3) {
                        if !l.trim().is_empty() {
                            lines.push(LogLine::tool_body(format!("  {}", l)));
                        }
                    }
                    if result_lines.len() > 4 {
                        lines.push(LogLine::tool_body(format!("  … +{} lines", result_lines.len() - 4)));
                    }
                }
            }
            "user" => {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    // Only show system nudges, not the full task description
                    if text.starts_with("[SYSTEM]") {
                        let short = text.trim_start_matches("[SYSTEM]").trim();
                        let preview: String = short.chars().take(100).collect();
                        lines.push(LogLine::nudge(format!("◆ {}", preview)));
                    }
                }
            }
            _ => {}
        }
    }

    lines
}

/// Short argument hint for tool call display.
fn tool_arg_hint(tc: &serde_json::Value) -> String {
    let args_str = tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}");
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) else { return String::new() };
    let name = tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
    match name {
        "read_file" | "write_file" | "patch_file" | "read_and_patch" => {
            args.get("path").and_then(|p| p.as_str())
                .map(|p| {
                    let short = p.rsplit('/').next().unwrap_or(p);
                    format!("({})", short)
                })
                .unwrap_or_default()
        }
        "bash" => {
            args.get("command").and_then(|c| c.as_str())
                .map(|c| {
                    let first_line = c.lines().next().unwrap_or(c);
                    let truncated: String = first_line.chars().take(50).collect();
                    format!("$ {}", truncated)
                })
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

// ─── Kill ─────────────────────────────────────────────────────────────────────

/// Kill all docker containers whose name starts with `<trial_name>-`.
/// Returns a human-readable status string.
fn kill_trial_containers(trial_name: &str) -> String {
    let output = std::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output();

    let names = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => return format!("docker ps failed: {e}"),
    };

    // Docker lowercases container names, so compare case-insensitively.
    let prefix = format!("{}-", trial_name.to_lowercase());
    let matching: Vec<&str> = names.lines()
        .filter(|n| n.to_lowercase().starts_with(&prefix))
        .collect();

    if matching.is_empty() {
        return format!("no containers found for {trial_name}");
    }

    let mut killed = Vec::new();
    let mut failed = Vec::new();
    for name in &matching {
        let res = std::process::Command::new("docker")
            .args(["rm", "-f", name])
            .output();
        match res {
            Ok(o) if o.status.success() => killed.push(*name),
            _ => failed.push(*name),
        }
    }

    if failed.is_empty() {
        format!("killed: {}", killed.join(", "))
    } else {
        format!("killed: {}  failed: {}", killed.join(", "), failed.join(", "))
    }
}

// ─── Binary freshness check ──────────────────────────────────────────────────

/// Rebuild the musl itsy binary if any source file is newer than the binary.
/// Prints progress to stderr (pre-TUI), runs `cargo build` with inherited I/O.
fn ensure_binary_fresh(binary: &Path) -> Result<()> {
    // Infer workspace root: binary is at <root>/target/…/itsy
    let workspace = binary
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let binary_mtime = binary
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok());

    // Check if any .rs file or Cargo.toml under crates/itsy is newer than binary.
    let src_root = workspace.join("crates").join("itsy");
    let stale = is_source_newer_than(binary_mtime, &src_root)
        || is_source_newer_than(binary_mtime, &workspace.join("Cargo.toml"));

    if !stale {
        return Ok(());
    }

    eprintln!("itsy source is newer than binary — rebuilding musl binary…");
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "x86_64-unknown-linux-musl", "-p", "itsy"])
        .current_dir(&workspace)
        .status()
        .context("failed to run cargo build")?;

    if !status.success() {
        anyhow::bail!("cargo build failed — fix build errors before running benchmark");
    }
    eprintln!("build complete.");
    Ok(())
}

fn is_source_newer_than(binary_mtime: Option<std::time::SystemTime>, path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    if path.is_file() {
        return path.metadata().ok()
            .and_then(|m| m.modified().ok())
            .zip(binary_mtime)
            .map(|(src, bin)| src > bin)
            .unwrap_or(true); // if binary doesn't exist yet, always rebuild
    }
    // Recurse into directory
    std::fs::read_dir(path).ok()
        .map(|entries| entries.flatten().any(|e| is_source_newer_than(binary_mtime, &e.path())))
        .unwrap_or(false)
}

// ─── Harbor subprocess ────────────────────────────────────────────────────────

fn build_harbor_command(
    tasks: &[String],
    model: &str,
    attempts: u32,
    concurrent: u32,
    jobs_dir: &Path,
    job_name: &str,
    itsy_binary: &Path,
) -> std::process::Command {
    let workspace = PathBuf::from("/workspace/itsy");
    let pythonpath = workspace.join(".agents/skills/terminal-bench-2");

    let mut cmd = std::process::Command::new("uv");
    cmd.arg("run")
        .arg("--with").arg("harbor")
        .arg("harbor").arg("run")
        .arg("--dataset").arg("terminal-bench@2.0")
        .arg("--agent-import-path").arg("itsy_agent:ItsyAgent")
        .arg("--model").arg(model)
        .arg("--n-attempts").arg(attempts.to_string())
        .arg("--n-concurrent").arg(concurrent.to_string())
        .arg("--jobs-dir").arg(jobs_dir)
        .arg("--job-name").arg(job_name);

    for task in tasks {
        cmd.arg("--include-task-name").arg(task);
    }

    cmd.env("ITSY_BINARY", itsy_binary)
        .env("PYTHONPATH", pythonpath)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cmd
}

// ─── TUI rendering ───────────────────────────────────────────────────────────

const PASS_COLOR: Color = Color::Rgb(100, 200, 100);
const FAIL_COLOR: Color = Color::Rgb(220, 80, 80);
const RUN_COLOR: Color = Color::Rgb(220, 180, 80);
const DIM_COLOR: Color = Color::Rgb(100, 100, 110);
const THINK_COLOR: Color = Color::Rgb(80, 80, 100);
const TEXT_COLOR: Color = Color::Rgb(170, 200, 240);
const NUDGE_COLOR: Color = Color::Rgb(200, 160, 80);
const BG: Color = Color::Rgb(13, 13, 15);
const FG: Color = Color::Rgb(190, 190, 195);
const BORDER: Color = Color::Rgb(45, 45, 52);
const HEADER_BG: Color = Color::Rgb(20, 20, 24);
const SEL_BG: Color = Color::Rgb(35, 35, 45);

fn render(f: &mut Frame, app: &App) {
    let size = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BG)), size);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(size);

    render_header(f, app, rows[0]);
    render_body(f, app, rows[1]);
    render_footer(f, app, rows[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let elapsed = app.wall_start
        .map(|ws| {
            let secs = (Utc::now() - ws).num_seconds().max(0) as u64;
            format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        })
        .or_else(|| app.started_at.map(|s| {
            let secs = s.elapsed().as_secs();
            format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        }))
        .unwrap_or_else(|| "00:00:00".to_string());

    let status_icon = if app.finished { "✓ done" } else { "⏳ running" };
    let model_short = app.model.rsplit_once('/').map(|(_, s)| s).unwrap_or(&app.model);

    let spans = vec![
        Span::styled("  itsy-bench  ", Style::default().fg(Color::Rgb(200, 200, 210)).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(BORDER)),
        Span::styled(&app.job_name, Style::default().fg(Color::Rgb(160, 160, 200))),
        Span::styled("  │ ", Style::default().fg(BORDER)),
        Span::styled(model_short, Style::default().fg(DIM_COLOR)),
        Span::styled("  │ ", Style::default().fg(BORDER)),
        Span::styled(&elapsed, Style::default().fg(Color::Rgb(150, 200, 180))),
        Span::styled("  │ ", Style::default().fg(BORDER)),
        Span::styled(status_icon, Style::default().fg(if app.finished { PASS_COLOR } else { RUN_COLOR })),
    ];

    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default().borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(HEADER_BG)),
        ),
        area,
    );
}

fn render_body(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(area);

    render_left_panel(f, app, cols[0]);
    render_agent_log(f, app, cols[1]);
}

fn render_left_panel(f: &mut Frame, app: &App, area: Rect) {
    let contract = app.selected_trial()
        .map(|t| t.contract.as_slice())
        .unwrap_or(&[]);

    let contract_height = if contract.is_empty() {
        0u16
    } else {
        (contract.len() as u16 + 3).min(area.height / 2)
    };

    let trial_height = area.height.saturating_sub(contract_height);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(trial_height), Constraint::Length(contract_height)])
        .split(area);

    render_trial_list(f, app, rows[0]);
    if contract_height > 0 {
        render_contract(f, app, rows[1], contract);
    }
}

fn render_trial_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.trials.iter().map(|t| {
        let (icon, color) = match t.status {
            TrialStatus::Passed  => ("✅", PASS_COLOR),
            TrialStatus::Failed  => ("❌", FAIL_COLOR),
            TrialStatus::Running => ("⏳", RUN_COLOR),
            TrialStatus::Pending => ("○ ", DIM_COLOR),
        };
        let reward_str = match t.reward {
            Some(r) => format!(" {:.1}", r),
            None    => String::new(),
        };
        let max_name = (area.width as usize).saturating_sub(8);
        let name = if t.task.len() > max_name {
            format!("{}…", &t.task[..max_name.saturating_sub(1)])
        } else {
            t.task.clone()
        };

        ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(name, Style::default().fg(FG)),
            Span::styled(reward_str, Style::default().fg(DIM_COLOR)),
        ]))
    }).collect();

    let title = format!(" Trials ({}/{}) ", app.stats.completed, app.stats.total);
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(BORDER)))
        .highlight_style(Style::default().bg(SEL_BG).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = app.list_state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn render_contract(f: &mut Frame, app: &App, area: Rect, assertions: &[ContractAssertion]) {
    let trial_status = app.selected_trial().map(|t| &t.status);
    let items: Vec<ListItem> = assertions.iter().map(|a| {
        let (icon, color) = match trial_status {
            Some(TrialStatus::Passed) => ("✓", PASS_COLOR),
            Some(TrialStatus::Failed) => ("✗", FAIL_COLOR),
            _ => ("○", DIM_COLOR),
        };
        let max_text = (area.width as usize).saturating_sub(6);
        let text = if a.text.len() > max_text {
            format!("{}…", &a.text[..max_text.saturating_sub(1)])
        } else {
            a.text.clone()
        };
        ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(text, Style::default().fg(Color::Rgb(170, 170, 180))),
        ]))
    }).collect();

    let list = List::new(items)
        .block(Block::default().title(" Contract ").borders(Borders::ALL).border_style(Style::default().fg(Color::Rgb(80, 80, 110))));
    f.render_widget(list, area);
}

fn render_agent_log(f: &mut Frame, app: &App, area: Rect) {
    let title = if let Some(t) = app.selected_trial() {
        format!(" {} ", t.name)
    } else {
        " Select a trial ".to_string()
    };

    let log_height = area.height.saturating_sub(2) as usize;
    let total = app.log_lines.len();
    let end = app.log_scroll.saturating_add(1).min(total);
    let start = end.saturating_sub(log_height);

    let lines: Vec<Line> = app.log_lines[start..end].iter().map(|l| {
        let (color, style) = match l.kind {
            LogKind::Thinking  => (THINK_COLOR, Style::default().fg(THINK_COLOR).add_modifier(Modifier::ITALIC)),
            LogKind::Text      => (TEXT_COLOR,  Style::default().fg(TEXT_COLOR)),
            LogKind::ToolCall  => (Color::Rgb(140, 160, 220), Style::default().fg(Color::Rgb(140, 160, 220))),
            LogKind::ToolOk    => (Color::Rgb(120, 190, 120), Style::default().fg(Color::Rgb(120, 190, 120))),
            LogKind::ToolErr   => (FAIL_COLOR,  Style::default().fg(FAIL_COLOR)),
            LogKind::ToolBody  => (DIM_COLOR,   Style::default().fg(DIM_COLOR)),
            LogKind::Nudge     => (NUDGE_COLOR, Style::default().fg(NUDGE_COLOR)),
            LogKind::Tokens    => (THINK_COLOR, Style::default().fg(THINK_COLOR)),
        };
        let _ = color; // used via style
        Line::from(Span::styled(&l.text, style))
    }).collect();

    let scroll_indicator = if total > log_height {
        format!(" {}/{} ", end, total)
    } else {
        String::new()
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default()
                .title(title)
                .title_bottom(scroll_indicator)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let pass = app.trials.iter().filter(|t| t.status == TrialStatus::Passed).count();
    let fail = app.trials.iter().filter(|t| t.status == TrialStatus::Failed).count();
    let run  = app.trials.iter().filter(|t| t.status == TrialStatus::Running).count();
    let mean_str = app.stats.mean_reward.map(|m| format!("mean {:.3}", m)).unwrap_or_default();

    // Show transient status message for 4s after a kill, otherwise show stats.
    let show_status = app.status_msg.as_ref()
        .map(|(_, t)| t.elapsed() < Duration::from_secs(4))
        .unwrap_or(false);

    let stats_spans = if show_status {
        let msg = app.status_msg.as_ref().map(|(m, _)| m.as_str()).unwrap_or("");
        vec![
            Span::raw("  "),
            Span::styled(msg, Style::default().fg(NUDGE_COLOR)),
        ]
    } else {
        vec![
            Span::raw("  "),
            Span::styled(format!("{}/{} done", app.stats.completed, app.stats.total), Style::default().fg(FG)),
            Span::styled("  │  ", Style::default().fg(BORDER)),
            Span::styled(format!("{}✅", pass), Style::default().fg(PASS_COLOR)),
            Span::raw(" "),
            Span::styled(format!("{}❌", fail), Style::default().fg(FAIL_COLOR)),
            Span::raw(" "),
            Span::styled(format!("{}⏳", run), Style::default().fg(RUN_COLOR)),
            if !mean_str.is_empty() {
                Span::styled(format!("  │  {}", mean_str), Style::default().fg(Color::Rgb(160, 200, 180)))
            } else {
                Span::raw("")
            },
        ]
    };

    let keys_text = "[q]uit  [↑↓]trial  [PgUp/Dn]log  [g/G]top/bot  [K]ill  [r]eload";
    let keys_width = keys_text.len() as u16 + 2;
    let stats_width = area.width.saturating_sub(keys_width);

    let footer_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(stats_width), Constraint::Length(keys_width)])
        .split(area);

    f.render_widget(
        Paragraph::new(Line::from(stats_spans))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(BORDER))),
        footer_cols[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", keys_text),
            Style::default().fg(DIM_COLOR),
        ))).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(BORDER))),
        footer_cols[1],
    );
}

// ─── Main event loop ─────────────────────────────────────────────────────────

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mut app: App) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, &app))?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(KeyEvent { code, kind: KeyEventKind::Press, modifiers, .. }) = event::read()? {
                match (code, modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.scroll_down(),
                    (KeyCode::Up, _)   | (KeyCode::Char('k'), _) => app.scroll_up(),
                    (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        for _ in 0..10 { app.log_scroll_down(); }
                    }
                    (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        for _ in 0..10 { app.log_scroll_up(); }
                    }
                    (KeyCode::Char('G'), _) => app.log_scroll = app.log_lines.len().saturating_sub(1),
                    (KeyCode::Char('g'), _) => app.log_scroll = 0,
                    (KeyCode::Char('r'), _) => {
                        app.last_log_file_count = 0;
                        poll_job_dir(&mut app);
                    }
                    (KeyCode::Char('K'), _) => {
                        if let Some(trial) = app.selected_trial() {
                            let name = trial.name.clone();
                            let msg = kill_trial_containers(&name);
                            app.status_msg = Some((msg, Instant::now()));
                            app.last_log_file_count = 0;
                        }
                    }
                    _ => {}
                }
            }
        }

        if app.last_poll.elapsed() >= Duration::from_secs(1) {
            poll_job_dir(&mut app);
        }
    }
    Ok(())
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Watch { job_dir } => {
            let abs = job_dir.canonicalize()
                .with_context(|| format!("job dir not found: {}", job_dir.display()))?;
            let job_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
            let model = detect_model(&abs);

            let mut app = App {
                job_dir: abs,
                job_name,
                model,
                started_at: Some(Instant::now()),
                ..Default::default()
            };
            poll_job_dir(&mut app);

            let mut terminal = setup_terminal()?;
            let result = run_app(&mut terminal, app);
            restore_terminal(&mut terminal)?;
            result
        }

        Cmd::Run { tasks, model, attempts, concurrent, job_name, jobs_dir, itsy_binary } => {
            let binary = itsy_binary.unwrap_or_else(|| {
                std::env::var("ITSY_BINARY").map(PathBuf::from).unwrap_or_else(|_| {
                    PathBuf::from("/workspace/itsy/target/x86_64-unknown-linux-musl/release/itsy")
                })
            });

            let name = job_name.unwrap_or_else(|| {
                let task_part = if tasks.is_empty() { "all".to_string() }
                    else if tasks.len() == 1 { tasks[0].clone() }
                    else { format!("{}-tasks", tasks.len()) };
                format!("{}-{}x-{}", task_part, attempts, chrono::Local::now().format("%H%M%S"))
            });

            let abs_jobs_dir = if jobs_dir.is_absolute() { jobs_dir.clone() }
                else { std::env::current_dir()?.join(&jobs_dir) };
            std::fs::create_dir_all(&abs_jobs_dir)?;

            let job_dir = abs_jobs_dir.join(&name);

            ensure_binary_fresh(&binary)?;

            eprintln!("Launching harbor — job: {}  dir: {}", name, job_dir.display());

            let harbor_child = build_harbor_command(&tasks, &model, attempts, concurrent, &abs_jobs_dir, &name, &binary)
                .spawn()
                .context("failed to spawn harbor")?;

            // Wait for harbor to create the job directory (up to 30s for image build)
            let wait_start = Instant::now();
            while !job_dir.exists() && wait_start.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(500));
            }
            if !job_dir.exists() {
                std::fs::create_dir_all(&job_dir)?;
            }

            let mut app = App {
                job_dir,
                job_name: name,
                model,
                started_at: Some(Instant::now()),
                harbor_child: Some(harbor_child),
                ..Default::default()
            };
            poll_job_dir(&mut app);

            let mut terminal = setup_terminal()?;
            let result = run_app(&mut terminal, app);
            restore_terminal(&mut terminal)?;
            result
        }
    }
}

fn detect_model(job_dir: &Path) -> String {
    if let Ok(s) = std::fs::read_to_string(job_dir.join("result.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(evals) = v.pointer("/stats/evals").and_then(|e| e.as_object()) {
                if let Some(key) = evals.keys().next() {
                    let parts: Vec<&str> = key.splitn(3, "__").collect();
                    if parts.len() >= 2 { return parts[1].to_string(); }
                }
            }
        }
    }
    "unknown".to_string()
}
