//! Entry point: parses args, boots the agent loop and the TUI (or runs in
//! non-interactive mode).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use parking_lot::Mutex;
use serde_json::Value;

use itsy::commands::{handle_command, CommandCtx, CommandResult};
use itsy::config::{check_endpoint, load_config, load_dotenv, Flags};
use itsy::escalation::{EscalationEngine, EscalationOptions};
use itsy::executor::{execute_tool, ExecCtx};
use itsy::governor::{classify_task, ToolScorer};
use itsy::governor::early_stop::EarlyStopDetector;
use itsy::mcp_bridge::McpBridge;
use itsy::memory::MemoryStore;
use itsy::model_client::{build_system_prompt, chat_completion, ChatContext};
use itsy::session::persistence::SessionStore;
use itsy::session::tokens::TokenTracker;
use itsy::tools::{get_all_tools, ToolDeps};
use itsy::tui;

#[derive(Debug, Parser)]
#[command(
    name = "itsy",
    version,
    about = "AI coding agent optimized for small LLMs (8B-35B parameters)"
)]
struct Cli {
    /// Model name (overrides ITSY_MODEL)
    #[arg(long)]
    model: Option<String>,
    /// Provider (openai-compatible by default)
    #[arg(long)]
    provider: Option<String>,
    /// Endpoint base URL
    #[arg(long)]
    endpoint: Option<String>,
    /// Single prompt for non-interactive mode
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Use the classic line-based TUI
    #[arg(long)]
    classic: bool,
    /// Verbose tool output
    #[arg(short = 'v', long)]
    verbose: bool,
    /// Print system prompt and exit
    #[arg(long)]
    print_system_prompt: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    let cli = Cli::parse();
    let flags = Flags {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        endpoint: cli.endpoint.clone(),
        base_url: cli.endpoint.clone(),
        classic: cli.classic,
        verbose: cli.verbose,
    };
    let mut config = load_config(&flags);

    if cli.print_system_prompt {
        println!("{}", build_system_prompt(&config, "", "", "", None));
        return Ok(());
    }

    println!("{}", tui::render_welcome(&config, false));
    let _reachable = check_endpoint(&mut config).await;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let memory = Arc::new(Mutex::new(MemoryStore::new(&cwd)));
    let history: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tokens = Arc::new(Mutex::new(TokenTracker::new()));
    let escalation = Arc::new(Mutex::new(EscalationEngine::new(EscalationOptions::default())));
    let mut sessions = SessionStore::new(cwd.clone());
    sessions.create();
    let _scorer = ToolScorer::new();
    let mut early_stop = EarlyStopDetector::new();

    let mcp_bridge = Arc::new(McpBridge::new());
    let bridge_ok = mcp_bridge.start().await.unwrap_or(false);
    if bridge_ok {
        let _ = mcp_bridge.init_code_graph(env!("CARGO_PKG_VERSION")).await;
    }

    let cfg_arc = Arc::new(Mutex::new(config));
    let cmd_ctx = CommandCtx {
        config: cfg_arc.clone(),
        history: history.clone(),
        memory: memory.clone(),
        tokens: tokens.clone(),
        escalation: escalation.clone(),
    };

    if let Some(prompt) = cli.prompt {
        handle_turn(&prompt, &cfg_arc, &history, &memory, &tokens, &mcp_bridge, &flags, &mut early_stop).await;
        return Ok(());
    }

    // Classic line-based REPL — the fullscreen renderer is opt-in via a future
    // flag now that we've separated the two code paths in the JS port.
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = String::new();
    loop {
        {
            let cfg = cfg_arc.lock();
            let hist = history.lock();
            print!("{}\n> ", tui::render_status(&cfg, hist.len()));
            stdout.lock().flush().ok();
        }
        input.clear();
        if stdin.lock().read_line(&mut input)? == 0 {
            break;
        }
        let line = input.trim_end_matches('\n').to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            match handle_command(&line, &cmd_ctx).await? {
                CommandResult::Quit => break,
                CommandResult::Print(s) => {
                    print!("{s}");
                    io::stdout().lock().flush().ok();
                }
                CommandResult::Continue => {}
            }
            continue;
        }
        handle_turn(&line, &cfg_arc, &history, &memory, &tokens, &mcp_bridge, &flags, &mut early_stop).await;
    }
    mcp_bridge.kill();
    Ok(())
}

async fn handle_turn(
    prompt: &str,
    cfg_arc: &Arc<Mutex<itsy::Config>>,
    history: &Arc<Mutex<Vec<Value>>>,
    memory: &Arc<Mutex<MemoryStore>>,
    tokens: &Arc<Mutex<TokenTracker>>,
    mcp_bridge: &Arc<McpBridge>,
    flags: &Flags,
    early_stop: &mut EarlyStopDetector,
) {
    early_stop.new_turn();
    let task_type = classify_task(prompt);
    history.lock().push(serde_json::json!({"role": "user", "content": prompt}));

    let ctx = ExecCtx {
        config: &cfg_arc.lock().clone(),
        flags,
        memory: memory.clone(),
        mcp_bridge: Some(mcp_bridge.clone()),
        mcp_client: None,
        fullscreen: None,
    };

    for _iter in 0..16 {
        let (config_snapshot, history_snapshot, tools) = {
            let cfg = cfg_arc.lock().clone();
            let hist = history.lock().clone();
            let tools = get_all_tools(&cfg, None, &ToolDeps::default());
            (cfg, hist, tools)
        };

        let chat_ctx = ChatContext {
            config: &config_snapshot,
            conversation: &history_snapshot,
            tools,
            current_task_type: Some(task_type),
            system_prompt: build_system_prompt(&config_snapshot, "", "", "", Some(task_type)),
        };
        let Some(data) = chat_completion(&chat_ctx).await else { break };
        if let Some(usage_obj) = data.get("usage") {
            tokens.lock().record(&serde_json::json!({"usage": usage_obj}), &config_snapshot.model.name);
        }
        let Some(msg) = data.pointer("/choices/0/message").cloned() else { break };
        history.lock().push(msg.clone());
        let calls = msg.get("tool_calls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if calls.is_empty() {
            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                if !content.trim().is_empty() {
                    println!("{}", tui::render_markdown(content));
                }
            }
            break;
        }
        for tc in calls {
            let name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let args_str = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            let args: Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            let started = std::time::Instant::now();
            println!("{}", tui::tool_start(&name));
            let result = execute_tool(&name, args, &ctx).await;
            let elapsed = started.elapsed().as_millis() as u64;
            if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
                println!("  {}", tui::tool_error(err));
            } else if let Some(action) = result.get("action").and_then(|v| v.as_str()) {
                let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let lines = result.get("lines").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                match action {
                    "Created" => println!("  {}", tui::tool_created(path, lines, elapsed)),
                    "Updated" => println!("  {}", tui::tool_updated(path, lines, elapsed)),
                    "Edited" => {
                        let line_num = result.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        println!("  {}", tui::tool_edited(path, line_num, elapsed));
                    }
                    _ => println!("  {}", tui::tool_success(&name, elapsed)),
                }
            } else if let Some(out) = result.get("result").and_then(|v| v.as_str()) {
                if flags.verbose {
                    println!("{out}");
                } else {
                    println!("  {}", tui::tool_success(&truncate(out, 80), elapsed));
                }
            }
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            history.lock().push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result.to_string(),
            }));
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}
