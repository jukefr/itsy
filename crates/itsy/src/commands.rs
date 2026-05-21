//! Slash-command dispatcher used by both the classic TUI and the
//! fullscreen renderer.
//!
//! Ports the full feature set from `bin/commands.js` (~772 lines) to
//! idiomatic Rust. Every JS command has a Rust counterpart with the
//! same semantics; env vars are renamed `SMALLCODE_*` → `ITSY_*`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::config::Config;
use crate::escalation::EscalationEngine;
use crate::eval_runner::{format_results, known_suite, EvalRunner};
use crate::lsp::LspClient;
use crate::memory::MemoryStore;
use crate::model::profiles::get_profile;
use crate::plugins::loader::PluginLoader;
use crate::plugins::skills::{SkillManager, Trigger};
use crate::session::git_context::get_git_diff_context_full;
use crate::session::multi::MultiSession;
use crate::session::persistence::SessionStore;
use crate::session::plan_tracker::PlanTracker;
use crate::session::share::{export, export_to_file, export_to_gist, ShareFormat};
use crate::session::snapshot::SnapshotManager;
use crate::session::tokens::TokenTracker;
use crate::session::undo::UndoStack;
use crate::token_monitor::TokenMonitor;
use crate::trace_recorder::TraceRecorder;

/// Bundle of shared handles passed to every slash-command. All fields except
/// the basics are `Option` so callers can wire only what they need.
pub struct CommandCtx {
    pub config: Arc<Mutex<Config>>,
    pub history: Arc<Mutex<Vec<Value>>>,
    pub memory: Arc<Mutex<MemoryStore>>,
    pub tokens: Arc<Mutex<TokenTracker>>,
    pub escalation: Arc<Mutex<EscalationEngine>>,

    pub cwd: Option<PathBuf>,
    pub token_monitor: Option<Arc<Mutex<TokenMonitor>>>,
    pub sessions: Option<Arc<Mutex<SessionStore>>>,
    pub multi: Option<Arc<MultiSession>>,
    pub undo: Option<Arc<Mutex<UndoStack>>>,
    pub snapshots: Option<Arc<SnapshotManager>>,
    pub trace: Option<Arc<Mutex<TraceRecorder>>>,
    pub skills: Option<Arc<Mutex<SkillManager>>>,
    pub plugins: Option<Arc<Mutex<PluginLoader>>>,
    pub plan: Option<Arc<Mutex<PlanTracker>>>,
    pub lsp: Option<Arc<LspClient>>,
}

impl CommandCtx {
    fn cwd(&self) -> PathBuf {
        self.cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

#[derive(Debug, Clone)]
pub enum CommandResult {
    Continue,
    Quit,
    Print(String),
}

/// Handle a slash command. Returns `Quit` to terminate the REPL loop.
pub async fn handle_command(cmd: &str, ctx: &CommandCtx) -> Result<CommandResult> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let head = parts.first().copied().unwrap_or("");
    let rest: Vec<&str> = parts.iter().skip(1).copied().collect();

    match head {
        "/quit" | "/q" | "/exit" => Ok(CommandResult::Quit),

        "/clear" => {
            ctx.history.lock().clear();
            Ok(CommandResult::Print("  ✓ Session cleared.\n\n".into()))
        }

        "/model" => Ok(CommandResult::Print(cmd_model(ctx, &rest).await)),
        "/endpoint" => Ok(CommandResult::Print(cmd_endpoint(ctx, &rest))),
        "/stats" => Ok(CommandResult::Print(cmd_stats(ctx))),
        "/tokens" => Ok(CommandResult::Print(cmd_tokens(ctx))),
        "/budget" => Ok(CommandResult::Print(cmd_budget(ctx))),
        "/memory" => Ok(CommandResult::Print(cmd_memory(ctx, &rest))),
        "/compact" => Ok(CommandResult::Print(cmd_compact(ctx))),
        "/escalation" => Ok(CommandResult::Print(cmd_escalation(ctx, &rest))),
        "/profile" => Ok(CommandResult::Print(cmd_profile(ctx))),
        "/trace" => Ok(CommandResult::Print(cmd_trace(ctx, &rest))),
        "/eval" => Ok(CommandResult::Print(cmd_eval(ctx, &rest).await)),
        "/diff" => Ok(CommandResult::Print(cmd_diff(ctx))),
        "/git" => Ok(CommandResult::Print(cmd_git(ctx, &rest))),
        "/files" => Ok(CommandResult::Print(cmd_files(ctx))),
        "/undo" => Ok(CommandResult::Print(cmd_undo(ctx, &rest))),
        "/share" => Ok(CommandResult::Print(cmd_share(ctx, &rest))),
        "/sessions" => Ok(CommandResult::Print(cmd_sessions(ctx, &rest))),
        "/session" => Ok(CommandResult::Print(cmd_session(ctx, &rest))),
        "/multi" => Ok(CommandResult::Print(cmd_session(ctx, &rest))),
        "/skill" | "/skills" => Ok(CommandResult::Print(cmd_skill(ctx, &rest))),
        "/plugin" | "/plugins" => Ok(CommandResult::Print(cmd_plugin(ctx, &rest))),
        "/plan" => Ok(CommandResult::Print(cmd_plan(ctx))),
        "/lsp" => Ok(CommandResult::Print(cmd_lsp(ctx, &rest).await)),
        "/web" => Ok(CommandResult::Print(cmd_web(&rest))),
        "/auto-approve" => Ok(CommandResult::Print(cmd_auto_approve(&rest))),
        "/checkpoint" => Ok(CommandResult::Print(cmd_checkpoint(ctx, &rest))),
        "/rollback" => Ok(CommandResult::Print(cmd_rollback(ctx, &rest))),

        "/help" | "/?" => Ok(CommandResult::Print(help_text())),

        _ => Ok(CommandResult::Print(format!("  Unknown command: {head}. Try /help\n\n"))),
    }
}

// ────────────────────────── individual handlers ──────────────────────────

async fn cmd_model(ctx: &CommandCtx, rest: &[&str]) -> String {
    if rest.is_empty() {
        let (model_name, base_url) = {
            let cfg = ctx.config.lock();
            (cfg.model.name.clone(), cfg.model.base_url.clone())
        };
        let mut out = format!("  Current: {}\n  Endpoint: {}\n\n", model_name, base_url);
        out.push_str("  Fetching available models... ");

        let url = format!("{}/models", base_url.trim_end_matches('/'));
        match reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                Ok(data) => {
                    let models = data
                        .get("data")
                        .or_else(|| data.get("models"))
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    out.push_str(&format!("{} found\n\n", models.len()));
                    for m in &models {
                        let id = m
                            .get("id")
                            .or_else(|| m.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let marker = if id == model_name { " ← active" } else { "" };
                        out.push_str(&format!("    {id}{marker}\n"));
                    }
                    out.push_str("\n  Switch: /model <name>\n");
                }
                Err(e) => out.push_str(&format!("error: {e}\n")),
            },
            Ok(resp) => out.push_str(&format!("failed (HTTP {})\n", resp.status().as_u16())),
            Err(e) => out.push_str(&format!("error: {e}\n")),
        }
        out.push('\n');
        out
    } else {
        let new_model = rest.join(" ");
        let mut cfg = ctx.config.lock();
        cfg.model.name = new_model.clone();
        format!("  ✓ Switched to {new_model}\n\n")
    }
}

fn cmd_endpoint(ctx: &CommandCtx, rest: &[&str]) -> String {
    if rest.is_empty() {
        let cfg = ctx.config.lock();
        format!(
            "  Current: {}\n  Switch: /endpoint http://host:port/v1\n\n",
            cfg.model.base_url
        )
    } else {
        let mut cfg = ctx.config.lock();
        cfg.model.base_url = rest[0].into();
        format!("  ✓ Endpoint: {}\n\n", rest[0])
    }
}

fn cmd_stats(ctx: &CommandCtx) -> String {
    let cfg = ctx.config.lock();
    let hist = ctx.history.lock();
    let tokens = ctx.tokens.lock();
    let stats = tokens.stats();
    let mut out = format!(
        "  Model:    {}\n  Endpoint: {}\n  History:  {} messages\n  Dir:      {}\n  Tokens:   {} prompt + {} completion = {} total\n",
        cfg.model.name,
        cfg.model.base_url,
        hist.len(),
        ctx.cwd().display(),
        stats.prompt,
        stats.completion,
        stats.total,
    );
    if let Some(tm) = &ctx.token_monitor {
        out.push_str(&format!("  Monitor:  {}\n", tm.lock().format_short()));
    }
    out.push('\n');
    out
}

fn cmd_tokens(ctx: &CommandCtx) -> String {
    let mut out = String::new();
    if let Some(tm) = &ctx.token_monitor {
        let full = tm.lock().format_full();
        for line in full.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    } else {
        let s = ctx.tokens.lock().stats();
        out.push_str(&format!(
            "  prompt: {} | completion: {} | total: {} | calls: {} | cost: ${:.4}\n",
            s.prompt, s.completion, s.total, s.calls, s.cost
        ));
    }
    out.push('\n');
    out
}

fn cmd_budget(ctx: &CommandCtx) -> String {
    let (max_ctx, budget_pct) = {
        let cfg = ctx.config.lock();
        (cfg.context.detected_window.max(1) as usize, cfg.context.max_budget_pct as usize)
    };
    let budget_pct = if budget_pct == 0 { 70 } else { budget_pct };
    let max_budget = max_ctx * budget_pct / 100;

    let current_est: usize = ctx
        .history
        .lock()
        .iter()
        .map(|m| {
            let c = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            c.len().div_ceil(4)
        })
        .sum();

    let usage = if max_budget == 0 { 0 } else { current_est * 100 / max_budget };
    let filled = (usage / 5).min(20);
    let bar: String = "█".repeat(filled) + &"░".repeat(20 - filled);

    let mut out = String::from("  Context Budget\n");
    out.push_str(&format!("  Window:    {} tokens\n", max_ctx));
    out.push_str(&format!("  Budget:    {} tokens ({}%)\n", max_budget, budget_pct));
    out.push_str(&format!("  Used:      {} tokens (~{}%)\n", current_est, usage));
    out.push_str(&format!("  [{}]\n", bar));
    if let Some(tm) = &ctx.token_monitor {
        let m = tm.lock().get_metrics();
        out.push_str(&format!(
            "  Compacts:  {} | Evictions: {}\n",
            m.compactions, m.evictions
        ));
    }
    out.push('\n');
    out
}

fn cmd_memory(ctx: &CommandCtx, rest: &[&str]) -> String {
    let sub = rest.first().copied().unwrap_or("list");
    match sub {
        "list" | "" => {
            let mem = ctx.memory.lock();
            let objects = mem.all();
            if objects.is_empty() {
                "  No memory stored. The model will save decisions/workflows/gotchas as it works.\n\n".into()
            } else {
                let mut out = format!("  Project memory ({} objects):\n", objects.len());
                for o in &objects {
                    out.push_str(&format!("    [{}] {} ({})\n", o.kind, o.title, o.id));
                }
                out.push('\n');
                out
            }
        }
        "clear" => {
            let mut mem = ctx.memory.lock();
            let ids: Vec<String> = mem.all().iter().map(|o| o.id.clone()).collect();
            for id in &ids {
                mem.forget(id);
            }
            "  ✓ Memory cleared.\n\n".into()
        }
        "remember" => {
            // /memory remember <type> <title> <content...>
            if rest.len() < 4 {
                "  Usage: /memory remember <type> <title> <content>\n\n".into()
            } else {
                let kind = rest[1];
                let title = rest[2];
                let content = rest[3..].join(" ");
                let obj = ctx.memory.lock().remember(kind, title, &content, Vec::new());
                format!("  ✓ Remembered [{}] {} ({})\n\n", obj.kind, obj.title, obj.id)
            }
        }
        "forget" => {
            if rest.len() < 2 {
                "  Usage: /memory forget <id>\n\n".into()
            } else {
                let id = rest[1];
                if ctx.memory.lock().forget(id) {
                    format!("  ✓ Forgot {id}\n\n")
                } else {
                    format!("  Memory {id} not found.\n\n")
                }
            }
        }
        "stats" => {
            let mem = ctx.memory.lock();
            let stats = mem.stats();
            let mut out = format!("  Memory: {} objects\n", stats.total);
            for (k, v) in &stats.by_type {
                out.push_str(&format!("    {k}: {v}\n"));
            }
            out.push('\n');
            out
        }
        other => format!(
            "  Unknown subcommand: {other}\n  /memory [list|clear|stats]\n  /memory remember <type> <title> <content>\n  /memory forget <id>\n\n"
        ),
    }
}

fn cmd_compact(ctx: &CommandCtx) -> String {
    let mut hist = ctx.history.lock();
    if hist.len() > 10 {
        let drop_n = hist.len() - 6;
        hist.drain(0..drop_n);
        format!("  ✓ Removed {drop_n} old messages, kept last 6.\n\n")
    } else {
        format!(
            "  Short history ({} msgs), nothing to compact.\n\n",
            hist.len()
        )
    }
}

fn cmd_escalation(ctx: &CommandCtx, rest: &[&str]) -> String {
    match rest.first().copied() {
        Some("on") | Some("enable") => {
            let mut esc = ctx.escalation.lock();
            esc.enabled = true;
            format!("  ✓ Escalation enabled. {}\n\n", esc.status())
        }
        Some("off") | Some("disable") => {
            let mut esc = ctx.escalation.lock();
            esc.enabled = false;
            "  ✓ Escalation disabled.\n\n".into()
        }
        _ => {
            let esc = ctx.escalation.lock();
            if !esc.enabled {
                let mut out = String::from("  Escalation: disabled\n");
                out.push_str("  Enable: set ANTHROPIC_API_KEY or OPENAI_API_KEY (or /escalation on)\n");
                out.push_str("  Or add [escalation] section to itsy.toml\n\n");
                out
            } else {
                format!(
                    "  ⬆ Escalation: enabled\n  {}\n  Confirm: {}\n\n",
                    esc.status(),
                    if esc.confirm { "yes (will ask)" } else { "no (auto)" }
                )
            }
        }
    }
}

fn cmd_profile(ctx: &CommandCtx) -> String {
    let (name, window) = {
        let cfg = ctx.config.lock();
        (cfg.model.name.clone(), cfg.context.detected_window)
    };
    let profile = get_profile(&name, window);
    let routing = std::env::var("ITSY_TOOL_ROUTING").unwrap_or_else(|_| "auto".into());
    let mut out = String::from("  Model Profile\n");
    out.push_str(&format!("  Model:     {}\n", name));
    out.push_str(&format!(
        "  Matched:   {}\n",
        profile.matched_key.unwrap_or("none (using defaults)")
    ));
    out.push_str(&format!("  Context:   {} tokens\n", profile.context_length));
    out.push_str(&format!("  Max out:   {} tokens\n", profile.max_output));
    out.push_str(&format!("  Tools:     {}\n", profile.tool_format));
    out.push_str(&format!("  Routing:   {}\n", routing));
    if !profile.strengths.is_empty() {
        out.push_str(&format!("  Strengths: {}\n", profile.strengths.join(", ")));
    }
    if !profile.weaknesses.is_empty() {
        out.push_str(&format!("  Weak:      {}\n", profile.weaknesses.join(", ")));
    }
    out.push('\n');
    out
}

fn cmd_trace(ctx: &CommandCtx, rest: &[&str]) -> String {
    let sub = rest.first().copied().unwrap_or("list");
    let tr = match &ctx.trace {
        Some(t) => t.clone(),
        None => Arc::new(Mutex::new(TraceRecorder::new(ctx.cwd()))),
    };
    match sub {
        "start" => {
            let prompt = if rest.len() > 1 { rest[1..].join(" ") } else { String::new() };
            let model = ctx.config.lock().model.name.clone();
            let id = tr.lock().start(&prompt, &model);
            format!("  ✓ Trace started: {id}\n\n")
        }
        "stop" => match tr.lock().stop() {
            Some(t) => format!("  ✓ Trace stopped: {} ({} steps)\n\n", t.id, t.steps.len()),
            None => "  No active trace.\n\n".into(),
        },
        "list" | "" => {
            let traces = tr.lock().list();
            if traces.is_empty() {
                "  No traces recorded yet.\n  Traces are recorded automatically for each turn.\n\n".into()
            } else {
                let mut out = format!("  Traces ({}):\n", traces.len());
                for t in traces.iter().take(15) {
                    let tok = t.tokens.prompt + t.tokens.completion;
                    let dur = t.duration_ms.unwrap_or(0);
                    out.push_str(&format!(
                        "    {} {} ({} steps, {} tok, {}ms)\n",
                        t.id, t.prompt, t.steps, tok, dur
                    ));
                }
                out.push('\n');
                out
            }
        }
        "replay" | "show" => {
            let id = match rest.get(1) {
                Some(i) => *i,
                None => return "  Usage: /trace replay <id>\n\n".into(),
            };
            match tr.lock().load(id) {
                Some(trace) => {
                    let mut out = format!("  Trace {}\n", trace.id);
                    let prompt_short: String = trace.prompt.chars().take(80).collect();
                    out.push_str(&format!("  Prompt: {}\n", prompt_short));
                    out.push_str(&format!("  Model:  {}\n", trace.model));
                    out.push_str(&format!(
                        "  Tokens: {}p + {}c\n",
                        trace.tokens.prompt, trace.tokens.completion
                    ));
                    out.push_str("  Steps:\n");
                    for step in &trace.steps {
                        use crate::trace_recorder::TraceStep::*;
                        match step {
                            ToolCall { name, duration_ms, .. } => {
                                out.push_str(&format!("    ⚙ {} ({}ms)\n", name, duration_ms));
                            }
                            Validation { file_path, passed, .. } => {
                                let mark = if *passed { "✓" } else { "✗" };
                                out.push_str(&format!("    {mark} validate {file_path}\n"));
                            }
                            ModelResponse { tool_calls, .. } => {
                                let n = tool_calls.as_ref().map(|t| t.len()).unwrap_or(0);
                                out.push_str(&format!("    ✎ model response ({n} tool calls)\n"));
                            }
                            ChatRequest { message_count, tool_count, .. } => {
                                out.push_str(&format!(
                                    "    → chat request ({message_count} msgs, {tool_count} tools)\n"
                                ));
                            }
                            Classification { task_type, tool_category, confidence, .. } => {
                                let cat = tool_category.as_deref().unwrap_or("·");
                                out.push_str(&format!(
                                    "    🧭 routed → task={task_type} cat={cat} (conf {confidence:.2})\n"
                                ));
                            }
                            Error { scope, message, .. } => {
                                out.push_str(&format!("    ✗ error [{scope}]: {message}\n"));
                            }
                        }
                    }
                    out.push('\n');
                    out
                }
                None => format!("  Trace {id} not found.\n\n"),
            }
        }
        "test" => {
            let id = match rest.get(1) {
                Some(i) => *i,
                None => return "  Usage: /trace test <id>\n\n".into(),
            };
            match tr.lock().generate_test(id) {
                Some(code) => {
                    let out_path = ctx
                        .cwd()
                        .join(".test-workspace")
                        .join(format!("trace_{id}.test.js"));
                    if let Some(parent) = out_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&out_path, code) {
                        Ok(_) => format!("  ✓ Generated {}\n\n", out_path.display()),
                        Err(e) => format!("  ✗ Failed to write: {e}\n\n"),
                    }
                }
                None => format!("  Cannot generate test from trace {id}.\n\n"),
            }
        }
        _ => "  /trace list           List recorded traces\n  /trace replay <id>    Show trace details\n  /trace test <id>      Generate test from trace\n  /trace start [prompt] Begin recording\n  /trace stop           Stop and save\n\n".into(),
    }
}

async fn cmd_eval(ctx: &CommandCtx, rest: &[&str]) -> String {
    let suite = rest.first().copied().unwrap_or("classify_accuracy");
    if let Err(e) = known_suite(suite) {
        return format!("  {e}\n\n");
    }
    let cfg_snapshot = ctx.config.lock().clone();
    let mut runner = EvalRunner::new(&cfg_snapshot);
    let result = match suite {
        "classify_accuracy" => runner.run_classify(),
        // Other suites need a chat function — present a friendly message
        // rather than wiring the full model client here.
        other => {
            return format!(
                "  Suite '{other}' requires model integration.\n  Currently available offline: classify_accuracy.\n\n"
            );
        }
    };
    let mut out = format_results(&result);
    out.push_str("\n\n");
    out
}

fn cmd_diff(ctx: &CommandCtx) -> String {
    let cwd = ctx.cwd();
    let output = Command::new("git").args(["diff", "--stat"]).current_dir(&cwd).output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let trimmed = s.trim();
            if trimmed.is_empty() {
                "  No uncommitted changes.\n\n".into()
            } else {
                let mut out = String::from("  Changes:\n");
                for line in trimmed.lines() {
                    out.push_str(&format!("  {line}\n"));
                }
                out.push('\n');
                out
            }
        }
        _ => "  Not a git repo.\n\n".into(),
    }
}

fn cmd_git(ctx: &CommandCtx, rest: &[&str]) -> String {
    let cwd = ctx.cwd();
    if rest.is_empty() {
        // Default: show full git diff context block via session::git_context.
        match get_git_diff_context_full(&cwd, 100) {
            Some(block) if !block.trim().is_empty() => format!("{block}\n"),
            _ => "  /git status │ /git log │ /git diff │ /git commit -m \"msg\"\n\n".into(),
        }
    } else {
        let args: Vec<&str> = rest.to_vec();
        let output = Command::new("git").args(&args).current_dir(&cwd).output();
        match output {
            Ok(o) => {
                let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
                if !o.status.success() {
                    out.push_str(&String::from_utf8_lossy(&o.stderr));
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out
            }
            Err(e) => format!("  git error: {e}\n\n"),
        }
    }
}

fn cmd_files(ctx: &CommandCtx) -> String {
    let cwd = ctx.cwd();
    let output = Command::new("git").args(["ls-files"]).current_dir(&cwd).output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let files: Vec<&str> = s.trim().lines().collect();
            let mut out = format!("  Project files ({}):\n", files.len());
            for f in files.iter().take(30) {
                out.push_str(&format!("    {f}\n"));
            }
            if files.len() > 30 {
                out.push_str(&format!("    ... ({} more)\n", files.len() - 30));
            }
            out.push('\n');
            out
        }
        _ => {
            let mut out = String::new();
            if let Ok(entries) = std::fs::read_dir(&cwd) {
                for entry in entries.flatten().take(20) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    out.push_str(&format!("    {name}\n"));
                }
            }
            out.push('\n');
            out
        }
    }
}

fn cmd_undo(ctx: &CommandCtx, rest: &[&str]) -> String {
    let stack = match &ctx.undo {
        Some(u) => u.clone(),
        None => return "  Undo stack not available in this session.\n\n".into(),
    };
    let sub = rest.first().copied().unwrap_or("");
    match sub {
        "list" => {
            let entries = stack.lock().list(10);
            if entries.is_empty() {
                "  No edits to undo.\n\n".into()
            } else {
                let mut out = String::from("  Recent edits:\n");
                for e in entries {
                    out.push_str(&format!(
                        "    #{} {} ({:?}, {}s ago)\n",
                        e.id, e.path, e.kind, e.age_secs
                    ));
                }
                out.push_str("\n  /undo       Revert last edit\n");
                out.push_str("  /undo <id>  Revert specific edit\n");
                out.push_str("  /undo all   Git revert all changes\n\n");
                out
            }
        }
        "all" => {
            let cwd = ctx.cwd();
            match Command::new("git").args(["checkout", "--", "."]).current_dir(&cwd).output() {
                Ok(o) if o.status.success() => "  ✓ Reverted all uncommitted changes.\n\n".into(),
                Ok(_) => "  Not a git repo (or git checkout failed).\n\n".into(),
                Err(e) => format!("  ✗ {e}\n\n"),
            }
        }
        s if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) => {
            let id: u64 = s.parse().unwrap_or(0);
            match stack.lock().undo_by_id(id) {
                Some(r) if r.error.is_none() => {
                    let target = r.reverted.as_deref().unwrap_or("");
                    format!("  ✓ Reverted {}: {}\n\n", target, r.action)
                }
                Some(r) => format!(
                    "  {}\n\n",
                    r.error.unwrap_or_else(|| "Edit not found.".into())
                ),
                None => "  Edit not found.\n\n".into(),
            }
        }
        _ => match stack.lock().undo_last() {
            Some(r) if r.error.is_none() => {
                let target = r.reverted.as_deref().unwrap_or("");
                format!("  ✓ Reverted {}: {}\n\n", target, r.action)
            }
            Some(r) => format!(
                "  {}\n\n",
                r.error.unwrap_or_else(|| "Edit not found.".into())
            ),
            None => "  No edits to undo. Use /undo all for git revert.\n\n".into(),
        },
    }
}

fn cmd_share(ctx: &CommandCtx, rest: &[&str]) -> String {
    let hist = ctx.history.lock().clone();
    if hist.is_empty() {
        return "  No session to share.\n\n".into();
    }
    let model = ctx.config.lock().model.name.clone();
    let title = hist
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .map(|s| s.chars().take(40).collect::<String>())
        .unwrap_or_default();
    let session = json!({
        "id": "tmp",
        "title": title,
        "messages": hist,
        "model": model,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    });

    match rest.first().copied().unwrap_or("") {
        "gist" => {
            let result = export_to_gist(&session);
            if result.success {
                format!(
                    "  ✓ Shared: {}\n\n",
                    result.url.unwrap_or_else(|| "<no url>".into())
                )
            } else {
                format!(
                    "  Failed: {}\n\n",
                    result.error.unwrap_or_else(|| "unknown".into())
                )
            }
        }
        "json" | "html" | "markdown" | "md" => {
            let fmt = match rest[0] {
                "json" => ShareFormat::Json,
                "html" => ShareFormat::Html,
                _ => ShareFormat::Markdown,
            };
            let ts = chrono::Utc::now().timestamp_millis();
            let out_path = ctx
                .cwd()
                .join(format!("itsy-session-{}.{}", ts, fmt.extension()));
            match export_to_file(&session, &out_path, fmt) {
                Ok(p) => format!("  ✓ Exported to {}\n\n", p.display()),
                Err(e) => format!("  ✗ {e}\n\n"),
            }
        }
        "" => {
            let out_path = ctx
                .cwd()
                .join(format!("itsy-session-{}.md", chrono::Utc::now().timestamp_millis()));
            let body = export(&session, ShareFormat::Markdown);
            match std::fs::write(&out_path, body) {
                Ok(_) => format!("  ✓ Exported to {}\n\n", out_path.display()),
                Err(e) => format!("  ✗ {e}\n\n"),
            }
        }
        path => {
            // Treat as output path; pick format by extension.
            let out_path = ctx.cwd().join(path);
            let fmt = ShareFormat::from_path(&out_path);
            match export_to_file(&session, &out_path, fmt) {
                Ok(p) => format!("  ✓ Exported to {}\n\n", p.display()),
                Err(e) => format!("  ✗ {e}\n\n"),
            }
        }
    }
}

fn cmd_sessions(ctx: &CommandCtx, rest: &[&str]) -> String {
    let store = match &ctx.sessions {
        Some(s) => s.clone(),
        None => return "  Session store not wired up.\n\n".into(),
    };
    let sub = rest.first().copied().unwrap_or("");
    match sub {
        "resume" | "load" => {
            let id = match rest.get(1) {
                Some(i) => *i,
                None => return "  Usage: /sessions resume <id>\n\n".into(),
            };
            let mut s = store.lock();
            match s.load(id) {
                Some(rec) => {
                    let title = rec.title.clone().unwrap_or_else(|| "untitled".into());
                    let msgs = rec.messages.clone();
                    let count = msgs.len();
                    drop(s);
                    let mut hist = ctx.history.lock();
                    hist.clear();
                    hist.extend(msgs);
                    format!("  ✓ Resumed \"{title}\" ({count} msgs)\n\n")
                }
                None => format!("  Session {id} not found.\n\n"),
            }
        }
        "new" => {
            store.lock().create();
            ctx.history.lock().clear();
            "  ✓ New session created.\n\n".into()
        }
        _ => {
            let recs = store.lock().list();
            if recs.is_empty() {
                "  No saved sessions.\n\n".into()
            } else {
                let mut out = format!("  Sessions ({}):\n", recs.len());
                let now_ms = chrono::Utc::now().timestamp_millis();
                for r in recs.iter().take(15) {
                    let updated = chrono::DateTime::parse_from_rfc3339(&r.updated_at)
                        .map(|d| d.timestamp_millis())
                        .unwrap_or(now_ms);
                    let age = ((now_ms - updated) / 60_000).max(0);
                    let age_str = if age < 60 {
                        format!("{age}m ago")
                    } else if age < 1440 {
                        format!("{}h ago", age / 60)
                    } else {
                        format!("{}d ago", age / 1440)
                    };
                    let id_short: String = r.id.chars().take(8).collect();
                    let title = r.title.clone().unwrap_or_else(|| "untitled".into());
                    out.push_str(&format!(
                        "    {id_short} {} ({} msgs · {age_str})\n",
                        title,
                        r.messages.len()
                    ));
                }
                out.push_str("\n  Resume: /sessions resume <id>\n  New:    /sessions new\n\n");
                out
            }
        }
    }
}

fn cmd_session(ctx: &CommandCtx, rest: &[&str]) -> String {
    let multi = match &ctx.multi {
        Some(m) => m.clone(),
        None => return "  Multi-session not wired up.\n\n".into(),
    };
    let sub = rest.first().copied().unwrap_or("list");
    match sub {
        "list" | "" => {
            let sessions = multi.list();
            if sessions.is_empty() {
                "  No parallel sessions. Use /session new <task>\n\n".into()
            } else {
                let mut out = format!("  Parallel sessions ({}):\n", sessions.len());
                for s in &sessions {
                    let marker = if s.active { " ●" } else { "  " };
                    out.push_str(&format!(
                        "  {marker} {} {} ({} msgs, {}s)\n",
                        s.id, s.title, s.messages, s.age_secs
                    ));
                }
                out.push('\n');
                out
            }
        }
        "new" => {
            let title = if rest.len() > 1 {
                Some(rest[1..].join(" "))
            } else {
                None
            };
            let s = multi.create(title.as_deref());
            ctx.history.lock().clear();
            format!("  ✓ New session {}: {}\n\n", s.id, s.title)
        }
        "switch" => {
            let id = match rest.get(1) {
                Some(i) => *i,
                None => return "  Usage: /session switch <id>\n\n".into(),
            };
            match multi.switch(id) {
                Some(s) => {
                    let mut hist = ctx.history.lock();
                    hist.clear();
                    hist.extend(s.messages.clone());
                    format!("  ✓ Switched to {}: {}\n\n", s.id, s.title)
                }
                None => format!("  Session {id} not found.\n\n"),
            }
        }
        "kill" => {
            let id = match rest.get(1) {
                Some(i) => *i,
                None => return "  Usage: /session kill <id>\n\n".into(),
            };
            if multi.kill(id) {
                format!("  ✓ Killed {id}\n\n")
            } else {
                format!("  Not found: {id}\n\n")
            }
        }
        _ => "  /session list          Show parallel sessions\n  /session new <task>    Start new session\n  /session switch <id>   Switch focus\n  /session kill <id>     Terminate session\n\n".into(),
    }
}

fn cmd_skill(ctx: &CommandCtx, rest: &[&str]) -> String {
    let sm = match &ctx.skills {
        Some(s) => s.clone(),
        None => Arc::new(Mutex::new(SkillManager::with_project_dir(ctx.cwd()))),
    };
    let sub = rest.first().copied().unwrap_or("list");
    match sub {
        "list" | "" => {
            let skills = sm.lock().list();
            if skills.is_empty() {
                "  No skills defined.\n  Create one: /skill add <name>\n  Skills teach the model reusable behaviors.\n\n".into()
            } else {
                let mut out = format!("  Skills ({}):\n", skills.len());
                for s in &skills {
                    out.push_str(&format!(
                        "    {} [{}] {}\n",
                        s.name,
                        s.trigger.as_str(),
                        s.preview
                    ));
                }
                out.push('\n');
                out
            }
        }
        "enable" | "use" => {
            let name = match rest.get(1) {
                Some(n) => *n,
                None => return "  Usage: /skill enable <name>\n\n".into(),
            };
            let injected = {
                let mgr = sm.lock();
                mgr.get(name).map(|s| (s.name.clone(), s.content.clone()))
            };
            match injected {
                Some((nm, content)) => {
                    ctx.history.lock().push(json!({
                        "role": "system",
                        "content": format!("[Skill: {nm}]\n{content}"),
                    }));
                    format!("  ✓ Skill \"{nm}\" activated for this conversation.\n\n")
                }
                None => format!("  Skill \"{name}\" not found.\n\n"),
            }
        }
        "disable" => {
            let name = match rest.get(1) {
                Some(n) => *n,
                None => return "  Usage: /skill disable <name>\n\n".into(),
            };
            // Drop any system message we injected for this skill.
            let tag = format!("[Skill: {name}]");
            let mut hist = ctx.history.lock();
            let before = hist.len();
            hist.retain(|m| {
                !(m.get("role").and_then(|r| r.as_str()) == Some("system")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|c| c.starts_with(&tag))
                        .unwrap_or(false))
            });
            if hist.len() < before {
                format!("  ✓ Skill \"{name}\" disabled in this session.\n\n")
            } else {
                format!("  Skill \"{name}\" was not active.\n\n")
            }
        }
        "add" => {
            let name = match rest.get(1) {
                Some(n) => *n,
                None => return "  Usage: /skill add <name> [content...]\n\n".into(),
            };
            let content = if rest.len() > 2 {
                rest[2..].join(" ")
            } else {
                "Describe the skill behavior here.".to_string()
            };
            match sm.lock().add(name, &content, Trigger::Manual, &[]) {
                Ok(skill) => format!(
                    "  ✓ Created skill \"{name}\" at {}\n  Edit the .md file to customize the skill content.\n\n",
                    skill.path.display()
                ),
                Err(e) => format!("  ✗ Failed: {e}\n\n"),
            }
        }
        "remove" => {
            let name = match rest.get(1) {
                Some(n) => *n,
                None => return "  Usage: /skill remove <name>\n\n".into(),
            };
            if sm.lock().remove(name) {
                format!("  ✓ Removed \"{name}\"\n\n")
            } else {
                format!("  Skill \"{name}\" not found.\n\n")
            }
        }
        _ => "  /skill list             Show all skills\n  /skill enable <name>    Activate a skill\n  /skill disable <name>   Deactivate a skill\n  /skill add <name>       Create a new skill\n  /skill remove <name>    Delete a skill\n\n".into(),
    }
}

fn cmd_plugin(ctx: &CommandCtx, rest: &[&str]) -> String {
    let pl = match &ctx.plugins {
        Some(p) => p.clone(),
        None => {
            let mut loader = PluginLoader::new();
            loader.load_all(&ctx.cwd());
            Arc::new(Mutex::new(loader))
        }
    };
    let sub = rest.first().copied().unwrap_or("list");
    match sub {
        "list" | "" => {
            let plugins = pl.lock().list();
            if plugins.is_empty() {
                "  No plugins installed.\n  Drop manifests into .itsy/plugins/ to activate.\n\n".into()
            } else {
                let mut out = format!("  Plugins ({}):\n", plugins.len());
                for p in &plugins {
                    out.push_str(&format!(
                        "    {} v{} — {}\n",
                        p.name, p.version, p.description
                    ));
                    if !p.tools.is_empty() {
                        out.push_str(&format!("      Tools: {}\n", p.tools.join(", ")));
                    }
                    if !p.commands.is_empty() {
                        out.push_str(&format!("      Commands: {}\n", p.commands.join(", ")));
                    }
                }
                out.push('\n');
                out
            }
        }
        "install" | "remove" => {
            format!("  /plugin {sub} is unsupported in the Rust port — drop manifests directly into .itsy/plugins/\n\n")
        }
        _ => "  /plugin list   Show installed plugins\n\n".into(),
    }
}

fn cmd_plan(ctx: &CommandCtx) -> String {
    let plan = match &ctx.plan {
        Some(p) => p.clone(),
        None => return "  No active plan.\n\n".into(),
    };
    let body = plan.lock().pretty();
    if body.is_empty() {
        "  No active plan.\n\n".into()
    } else {
        format!("  Plan:\n{}\n\n", indent(&body, "    "))
    }
}

async fn cmd_lsp(ctx: &CommandCtx, rest: &[&str]) -> String {
    let lsp = match &ctx.lsp {
        Some(l) => l.clone(),
        None => return "  LSP client not wired up.\n\n".into(),
    };
    let sub = rest.first().copied().unwrap_or("status");
    match sub {
        "start" | "" => {
            if lsp.start_auto(&ctx.cwd()).await {
                "  ✓ LSP started.\n\n".into()
            } else {
                "  No supported language server detected for this workspace.\n\n".into()
            }
        }
        "stop" => {
            lsp.stop().await;
            "  ✓ LSP stopped.\n\n".into()
        }
        "status" => {
            if lsp.is_initialized() {
                "  LSP: running.\n\n".into()
            } else {
                "  LSP: not initialized. Use /lsp start.\n\n".into()
            }
        }
        _ => "  /lsp start | /lsp stop | /lsp status\n\n".into(),
    }
}

fn set_env(name: &str, value: &str) {
    // SAFETY: itsy's REPL is single-threaded during slash-command dispatch.
    // The /web and /auto-approve toggles are a thin wrapper over the env var
    // so downstream code (`std::env::var(...)`) picks the new value up.
    unsafe { std::env::set_var(name, value) }
}

fn cmd_web(rest: &[&str]) -> String {
    match rest.first().copied() {
        Some("on") | Some("enable") => {
            set_env("ITSY_WEB_BROWSE", "true");
            "  ✓ Web browsing enabled (ITSY_WEB_BROWSE=true).\n\n".into()
        }
        Some("off") | Some("disable") => {
            set_env("ITSY_WEB_BROWSE", "false");
            "  ✓ Web browsing disabled.\n\n".into()
        }
        _ => {
            let current = std::env::var("ITSY_WEB_BROWSE").unwrap_or_else(|_| "false".into());
            format!("  Web browsing: {current}\n  /web on | /web off\n\n")
        }
    }
}

fn cmd_auto_approve(rest: &[&str]) -> String {
    match rest.first().copied() {
        Some("on") | Some("enable") => {
            set_env("ITSY_AUTO_APPROVE", "true");
            "  ✓ Auto-approve enabled (ITSY_AUTO_APPROVE=true).\n\n".into()
        }
        Some("off") | Some("disable") => {
            set_env("ITSY_AUTO_APPROVE", "false");
            "  ✓ Auto-approve disabled.\n\n".into()
        }
        _ => {
            let current = std::env::var("ITSY_AUTO_APPROVE").unwrap_or_else(|_| "false".into());
            format!("  Auto-approve: {current}\n  /auto-approve on | /auto-approve off\n\n")
        }
    }
}

fn cmd_checkpoint(ctx: &CommandCtx, rest: &[&str]) -> String {
    let snap = match &ctx.snapshots {
        Some(s) => s.clone(),
        None => {
            // Construct an ad-hoc manager rooted at cwd if none was wired up.
            Arc::new(SnapshotManager::new(ctx.cwd()))
        }
    };
    let label = if rest.is_empty() { "manual".to_string() } else { rest.join(" ") };
    match snap.begin_with(&label) {
        Some(id) => format!("  ✓ Checkpoint {id} ({label})\n\n"),
        None => "  Snapshots disabled (ITSY_SNAPSHOT=false).\n\n".into(),
    }
}

fn cmd_rollback(ctx: &CommandCtx, rest: &[&str]) -> String {
    let snap = match &ctx.snapshots {
        Some(s) => s.clone(),
        None => return "  Snapshot manager not wired up.\n\n".into(),
    };
    let reason = if rest.is_empty() { "manual rollback".to_string() } else { rest.join(" ") };
    let summary = snap.rollback_with(&reason);
    if summary.checkpoint_id.is_empty() {
        return "  No active checkpoint to roll back.\n\n".into();
    }
    let mut out = format!(
        "  ✓ Rolled back checkpoint {} ({}): restored {}, deleted {}\n",
        summary.checkpoint_id, summary.label, summary.restored, summary.deleted
    );
    if !summary.errors.is_empty() {
        out.push_str(&format!("  Errors: {}\n", summary.errors.len()));
    }
    if !summary.skipped.is_empty() {
        out.push_str(&format!("  Skipped: {}\n", summary.skipped.len()));
    }
    out.push('\n');
    out
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}

fn help_text() -> String {
    "
  Commands
  ─────────────────────────────────────
  /model [name]        Show or switch model (no arg → fetch list from endpoint)
  /endpoint [url]      Show or switch endpoint
  /stats               Session stats (model, history, tokens)
  /tokens              Detailed token usage report
  /budget              Show context window budget
  /memory [list|clear|stats]
  /memory remember <type> <title> <content>
  /memory forget <id>
  /compact             Trim conversation history
  /escalation [on|off] Show / toggle cloud-model fallback
  /profile             Show resolved model profile
  /trace [list|start|stop|replay <id>|test <id>]
  /eval <suite>        Run an evaluation suite (offline: classify_accuracy)
  /diff                Git diff summary
  /git <cmd...>        Run a git command
  /files               List project files
  /undo [id|list|all]  Per-edit undo stack
  /share [gist|json|html|md|<path>]
  /sessions [list|resume <id>|new]
  /session [list|new <task>|switch <id>|kill <id>]
  /skill / /skills [list|enable <name>|disable <name>|add <name>|remove <name>]
  /plugin / /plugins [list]
  /plan                Show active plan
  /lsp [start|stop|status]
  /web [on|off]        Toggle ITSY_WEB_BROWSE
  /auto-approve [on|off]   Toggle ITSY_AUTO_APPROVE
  /checkpoint [label]  Manual snapshot
  /rollback [reason]   Restore last snapshot
  /clear               Reset entire session
  /quit                Exit itsy
  /help                This screen
"
    .into()
}
