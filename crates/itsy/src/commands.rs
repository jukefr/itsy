//! Slash-command dispatcher used by both the
//! classic TUI and the fullscreen renderer.

use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde_json::Value;

use crate::escalation::EscalationEngine;
use crate::memory::MemoryStore;
use crate::session::tokens::TokenTracker;
use crate::Config;

pub struct CommandCtx {
    pub config: Arc<Mutex<Config>>,
    pub history: Arc<Mutex<Vec<Value>>>,
    pub memory: Arc<Mutex<MemoryStore>>,
    pub tokens: Arc<Mutex<TokenTracker>>,
    pub escalation: Arc<Mutex<EscalationEngine>>,
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
    match head {
        "/quit" | "/q" | "/exit" => Ok(CommandResult::Quit),
        "/clear" => {
            ctx.history.lock().clear();
            Ok(CommandResult::Print("  ✓ Session cleared.\n".into()))
        }
        "/model" => {
            if parts.len() < 2 {
                let cfg = ctx.config.lock();
                Ok(CommandResult::Print(format!("  Current: {}\n  Endpoint: {}\n", cfg.model.name, cfg.model.base_url)))
            } else {
                let new_model = parts[1..].join(" ");
                let mut cfg = ctx.config.lock();
                cfg.model.name = new_model.clone();
                Ok(CommandResult::Print(format!("  ✓ Switched to {new_model}\n")))
            }
        }
        "/endpoint" => {
            if parts.len() < 2 {
                let cfg = ctx.config.lock();
                Ok(CommandResult::Print(format!("  Current: {}\n", cfg.model.base_url)))
            } else {
                let mut cfg = ctx.config.lock();
                cfg.model.base_url = parts[1].into();
                Ok(CommandResult::Print(format!("  ✓ Endpoint: {}\n", parts[1])))
            }
        }
        "/stats" => {
            let cfg = ctx.config.lock();
            let hist = ctx.history.lock();
            let tokens = ctx.tokens.lock();
            let stats = tokens.stats();
            Ok(CommandResult::Print(format!(
                "  Model:    {}\n  Endpoint: {}\n  History:  {} messages\n  Tokens:   {} total\n",
                cfg.model.name,
                cfg.model.base_url,
                hist.len(),
                stats.total
            )))
        }
        "/tokens" => {
            let tokens = ctx.tokens.lock();
            let s = tokens.stats();
            Ok(CommandResult::Print(format!(
                "  prompt: {} | completion: {} | total: {}\n",
                s.prompt, s.completion, s.total
            )))
        }
        "/memory" => {
            let mem = ctx.memory.lock();
            let stats = mem.stats();
            let mut out = format!("  Memory: {} objects\n", stats.total);
            for (k, v) in &stats.by_type {
                out.push_str(&format!("    {k}: {v}\n"));
            }
            Ok(CommandResult::Print(out))
        }
        "/escalation" => {
            let esc = ctx.escalation.lock();
            Ok(CommandResult::Print(format!("  {}\n", esc.status())))
        }
        "/help" | "/?" => Ok(CommandResult::Print(help_text())),
        _ => Ok(CommandResult::Print(format!("  Unknown command: {head}. Try /help\n"))),
    }
}

fn help_text() -> String {
    "
  Commands:
    /help, /?        Show this help
    /quit, /q        Exit
    /clear           Clear conversation history
    /model [name]    Show or switch model
    /endpoint [url]  Show or switch endpoint
    /stats           Session statistics
    /tokens          Token usage breakdown
    /memory          Memory store statistics
    /escalation      Escalation engine status
"
    .into()
}
