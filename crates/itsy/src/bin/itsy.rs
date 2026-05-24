//! Entry point for the `itsy` binary.
//!
//! This is the 1:1 Rust port of `bin/smallcode.js` — the full agent loop with
//! clarifier, plan tracker, two-stage routing, verification governor,
//! decompose strategies, escalation, dedup, token monitoring and trace
//! recording. Environment variables use the `ITSY_*` prefix.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

use itsy::runtime::agent_loop::{AgentSession, AgentSessionReadOnly, AgentSessionShared, AgentSessionMutable, GuardAction};
use itsy::commands::{handle_command, CommandCtx, CommandResult};
use itsy::config::{check_endpoint, load_config, load_dotenv, Config, Flags};
use itsy::cognition_adapter::classify_task_compiled;
use itsy::eval_runner::{format_results, known_suite, EvalRunner};
use itsy::executor::{execute_tool, ExecCtx};
use itsy::features_adapter;
use itsy::governor::{
    classify_task, pick_decompose_strategy, ToolScorer, VerificationHistory,
    HardFailAction,
};
use itsy::governor::early_stop::EarlyStopDetector;
use itsy::mcp_bridge::McpBridge;
use itsy::memory::MemoryStore;
use itsy::model_client::{
    build_system_prompt, chat_completion, run_validation, stream_final_response,
    ChatContext,
};
use itsy::plugins::loader::PluginLoader;
use itsy::plugins::skills::SkillManager;
use itsy::session::git_context::{get_git_diff_context, should_inject_git_context};
use itsy::session::persistence::SessionStore;
use itsy::session::references::{format_references_for_prompt, resolve_references};
use itsy::session::tokens::TokenTracker;
use itsy::token_monitor::{CallMetadata, TokenMonitor};
use itsy::tools::{get_all_tools, ToolDeps};
use itsy::trace_recorder::TraceRecorder;
use itsy::tui;

// ── Constants ────────────────────────────────────────────────────────────────

fn max_tool_calls_per_turn() -> u32 {
    itsy::settings::get().max_tool_calls_per_turn
}

/// Actionable hint injected when the same tool fails twice in a row.
/// Generic across tasks — no task-specific knowledge, just tool usage tips.
fn recency_tool_hint(tool_name: &str) -> &'static str {
    match tool_name {
        "patch" => "Try `read_and_patch` instead — it reads the current file first then patches atomically, avoiding stale-content mismatches.",
        "bash" => "Double-check that commands and paths exist. Use `which <cmd>` or `ls <path>` to probe before running complex commands.",
        "write_file" => "Call `read_file` on the target path first — the write guard requires a prior read before writing.",
        "read_file" => "Verify the path exists with `bash` before reading.",
        _ => "Try a different approach, a different tool, or verify your assumptions with a simpler command first.",
    }
}
/// Maximum auto-improvement iterations per file before we DECOMPOSE.
const MAX_IMPROVE_ITERATIONS: u32 = 4;

// ── CLI parsing ──────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "itsy",
    version,
    about = "AI coding agent optimized for small LLMs (8B-35B parameters)"
)]
struct Cli {
    /// Model name
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
    /// Run interactive setup wizard
    #[arg(long)]
    init: bool,
    /// Run an eval suite (classify_accuracy, tool_selection, response_quality)
    #[arg(long, value_name = "SUITE")]
    eval: Option<String>,
    /// Start in MCP server mode (JSON-RPC over stdio)
    #[arg(long)]
    mcp: bool,
    /// Treat as non-interactive: read prompt from stdin if not given via -p
    #[arg(long = "non-interactive", short = 'n')]
    non_interactive: bool,
    /// Resume the most recent session, if any
    #[arg(long)]
    resume: bool,

    // ── runtime knobs (override the persisted config) ────────────────
    /// Auto-approve every tool call (skip the y/n prompt).
    #[arg(long)]
    auto_approve: bool,
    /// Hard cap on tool calls in a single turn. Default 250.
    #[arg(long, value_name = "N")]
    max_tool_calls_per_turn: Option<u32>,
    /// Max output tokens passed to the chat-completion request.
    /// 0 = auto (thinking_budget + 4k). Default 0.
    #[arg(long, value_name = "N")]
    max_output_tokens: Option<u32>,
    /// Reasoning-token budget per turn. 0 = per-task heuristic.
    #[arg(long, value_name = "N")]
    thinking_budget: Option<u32>,
    /// Per-request chat-completion timeout in ms. Default 120000.
    #[arg(long, value_name = "MS")]
    request_timeout_ms: Option<u64>,
    /// Bash command timeout in seconds. Default 30.
    #[arg(long, value_name = "SECS")]
    bash_timeout: Option<u32>,
    /// Tool routing mode (direct, two_stage, auto). Default direct.
    #[arg(long, value_name = "MODE")]
    tool_routing: Option<String>,
    /// Allow read/write tools to touch absolute paths outside the
    /// project root. Sensitive paths are still blocked.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    allow_outside_paths: Option<bool>,
    /// Enable web_search / web_fetch tools.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    web_browse: Option<bool>,
    /// Use a persistent shell so `cd` etc. stick across calls.
    #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    shell_persist: Option<bool>,
    /// Pick a built-in model profile by name.
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,
    /// Override an arbitrary setting in `key=value` form. Repeatable.
    /// Keys are dotted paths into the [`crate::settings`] struct
    /// (e.g. `--set dedup.enabled=false`, `--set features.reviewer=true`).
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set_overrides: Vec<String>,

    /// Model name for second-opinion calls (evaluator, future analysis). Defaults to main model.
    #[arg(long, value_name = "MODEL")]
    second_opinion_model: Option<String>,
    /// Endpoint URL for second-opinion calls. Defaults to main endpoint.
    #[arg(long, value_name = "URL")]
    second_opinion_endpoint: Option<String>,

    /// Positional prompt (anything not consumed by --prompt/-p)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    positional: Vec<String>,
}

// ── Agent session: shared per-process state ──────────────────────────────────

// ── Helpers: estimation & history compaction ─────────────────────────────────

/// Cheap heuristic token estimator (~4 chars per token).
async fn handle_turn(prompt_in: &str, session: &AgentSession) {
    // Reset early-stop bookkeeping for a fresh turn.
    session.mutable.lock().early_stop.new_turn();


    // Trace recording: start a fresh trace for this turn.
    {
        let model_name = itsy::settings::get().model_name.clone();
        session.mutable.lock().trace.start(prompt_in, &model_name);
    }
    session.shared.write().token_monitor.mark_next_call_new_turn();

    let user_msg = prompt_in.to_string();

    // 1) Clarifier check — only fires on short messages (< 80 chars) that
    // are *not* obviously actionable (paths, option-refs, affirmations).
    // Ports the a85c90c fix: instruction is spliced out after the model
    // responds so it doesn't linger across turns.
    // Ports the assistantAskedQuestion bypass: if the model's last turn ended
    // with a question mark, the user's reply is an answer, not a new vague task.
    let clarifier_enabled = itsy::settings::get().clarifier;
    let assistant_asked_question = {
        let locked = session.mutable.lock();
        let hist = &locked.history;
        hist.iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .and_then(|m| m.get("content").and_then(|c| c.as_str()))
            .map(|c| c.trim_end().ends_with('?'))
            .unwrap_or(false)
    };
    if clarifier_enabled
        && user_msg.len() < 80
        && !assistant_asked_question
        && !itsy::runtime::tool_router::looks_like_path(&user_msg)
        && !itsy::runtime::tool_router::looks_like_option_ref(&user_msg)
        && !itsy::runtime::tool_router::is_affirmation(&user_msg)
    {
        let needs = features_adapter::check_needs_clarification(&user_msg).await;
        if needs {
            let clarifier_idx = {
                let mut locked = session.mutable.lock();
                locked.history.push(json!({"role": "user", "content": user_msg.clone()}));
                let idx = locked.history.len();
                locked.history.push(json!({
                    "role": "system",
                    "content": itsy::session::clarify::get_clarification_instruction(),
                }));
                idx
            };
            // Snapshot for async use — must clone since lock can't be held across .await.
            let snapshot = (session.mutable.lock().history.clone(), session.shared.read().config.clone());
            let chat_ctx = ChatContext {
                model_name: &snapshot.1.model.name,
                base_url: &snapshot.1.model.base_url,
                api_key: snapshot.1.model.api_key.clone(),
                timeout: Duration::from_secs(snapshot.1.model.timeout),
                temp_adapt: snapshot.1.features.temp_adapt,
                conversation: &snapshot.0,
                tools: Vec::new(),
                current_task_type: None,
                system_prompt: itsy::model::prompts::build_full_system_prompt(
                    &snapshot.1, "explanation", &snapshot.0,
                    &session.shared.read().memory, &session.shared.read().skills, &session.shared.read().plugins, &session.ro.cwd,
                ),
                force_disable_thinking: false,
            };
            let response = chat_completion(&chat_ctx).await;
            // Splice the one-shot clarifier instruction out of history,
            // whether or not the model responded — otherwise it sticks
            // around and re-fires on every subsequent turn.
            {
                let mut locked = session.mutable.lock();
                if clarifier_idx < locked.history.len() {
                    let is_clarifier = locked.history[clarifier_idx]
                        .get("role")
                        .and_then(|r| r.as_str())
                        == Some("system")
                        && locked.history[clarifier_idx]
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains("vague"))
                            .unwrap_or(false);
                    if is_clarifier {
                        locked.history.remove(clarifier_idx);
                    }
                }
            }
            if let Some(data) = response {
                record_usage(&data, session, false);
                if let Some(msg) = data.pointer("/choices/0/message") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        session.mutable.lock().history
                            .push(json!({"role": "assistant", "content": content}));
                        println!("{}", tui::render_markdown(content));
                    }
                }
            }
            // Wait for the user to clarify on the next REPL iteration.
            return;
        }
    }

    // 2) Resolve `@file` references + auto-inject git diff when applicable.
    let refs = resolve_references(&user_msg, &session.ro.cwd);
    let mut augmented = if refs.is_empty() {
        user_msg.clone()
    } else {
        format!("{}{}", user_msg, format_references_for_prompt(&refs))
    };
    if should_inject_git_context(&user_msg) {
        if let Some(ctx) = get_git_diff_context(&session.ro.cwd) {
            augmented.push_str(&ctx);
        }
    }

    // Contract-proposal reframe. When the contract feature is on and
    // there's no active contract yet, wrap the user's prompt with a
    // "define done for the following" preamble. Probe data (see
    // benchmark-driven-development/baselines/) showed this is the only
    // shape that gets the model to emit propose_contract reliably while
    // keeping thinking ON — system-prompt directives alone don't work
    // because the model's reasoning layer locks onto the user task
    // before it consults the system message. The wrap moves the
    // priority into the user role where the model actually weighs it.
    //
    // Skip for conversational messages (`respond` category): "hello",
    // "thanks", etc. should never be wrapped — the model ends up printing
    // a fake propose_contract() call in its text even with no tools
    // available, because the preamble is in the user role and outweighs
    // the system-prompt gate. The classifier is cheap (deterministic
    // regex) so running it early costs nothing.
    if itsy::settings::get().contract && itsy::session::contract::current().is_none() {
        let early_cls = itsy::runtime::tool_router::classify_tool_category(&user_msg);
        if early_cls.category != "respond" {
            augmented = format!(
                "Define what 'done' means for the following task by calling \
                 propose_contract. Do not start the task yet — just enumerate \
                 the checks that would prove it's done.\n\n\
                 TASK:\n{augmented}"
            );
        }
    }

    session.mutable.lock().history
        .push(json!({"role": "user", "content": augmented.clone()}));

    // 3) Task classification (regex now; compiled LLM-based when available).
    //    `classify_task` defaults to "coding" for anything it doesn't match,
    //    which is wrong for conversational messages. We override below if
    //    the tool router also routes the message to `respond`.
    let task_type_initial = classify_task_compiled(&user_msg, classify_task).await;

    // 5) Tool routing: classify intent → filter (or strip) tools. The
    //    `respond` category strips tools entirely so the model can't be
    //    tempted to write a file on a conversational message like
    //    "say hello". This applies to BOTH direct and two-stage routing.
    let route = itsy::runtime::tool_router::classify_and_filter(
        &user_msg,
        session.mutable.lock().current_tool_category.as_deref(),
    );
    session.mutable.lock().current_tool_category = if route.category == "respond" {
        Some("respond".into())
    } else {
        None
    };
    let stage2_category: Option<String> = if route.category == "respond" {
        Some("respond".into())
    } else {
        None
    };


    // Align `task_type` with the routing decision. If the router says
    // `respond`, treat the task as an explanation so downstream guards
    // (system prompt, badger loops, plan tracker) don't try to coerce
    // the model into action mode for a chat message.
    let task_type: &'static str = if stage2_category.as_deref() == Some("respond") {
        "explanation"
    } else {
        task_type_initial
    };

    session.mutable.lock().trace.record_classification(
        task_type,
        stage2_category.as_deref(),
        0.0,
    );

    // 6) Maybe pre-compact the history before the first call.
    {
        let mut locked = session.mutable.lock();
        if itsy::session::compaction::maybe_compact(&mut locked.history) {
            println!("{}", tui::compacted(locked.history.len() as u32));
        }
    }

    let mut state = itsy::runtime::agent_loop::TurnState::new(
        user_msg.clone(), task_type, stage2_category, max_tool_calls_per_turn(),
    );
    const MAX_CONSECUTIVE_BLOCKS: u32 = 2;

    // Reset any pending Ctrl+C presses from earlier turns; the user starts
    // each turn with a clean slate.
    itsy::interrupt::reset();

    // 7) Main while-loop.
    loop {
        if state.tool_calls_this_turn >= state.max_tool_calls {
            println!("\n  \x1b[33m⚠ Reached tool call limit\x1b[0m");
            break;
        }

        // Cooperative SIGINT check. Once the user has pressed Ctrl+C, we
        // bail out as soon as we're between tool calls instead of letting
        // the model fire off another one.
        if itsy::interrupt::pending() > 0 {
            itsy::interrupt::take();
            println!("\n  \x1b[33m⚠ Interrupted\x1b[0m");
            break;
        }

        // Mid-turn eviction every 3 tool calls.
        if state.tool_calls_this_turn > 0 && state.tool_calls_this_turn % 3 == 0 {
            let mut locked = session.mutable.lock();
            let evicted = itsy::session::compaction::mid_turn_evict(&mut locked.history);
            if evicted > 0 {
                session.shared.write().token_monitor.record_eviction();
            }
        }

        // Build the request snapshot.
        let (cfg, hist, tools) = {
            let cfg = session.shared.read().config.clone();
            let hist = session.mutable.lock().history.clone();
            let deps = {
                let plugins = &session.shared.read().plugins;
                ToolDeps {
                    plugin_tools: plugins.get_tools(),
                    mcp_tools: Vec::new(),
                }
            };
            let mut tools = get_all_tools(&cfg, state.current_category.as_deref(), &deps);
            // Contract-first: when the feature is on, the task is
            // action-y, and there's no active contract yet, strip the
            // mutating tools from what the model sees. Reasoning-layer
            // dissociation (model thinks "propose contract" but emits
            // `write_file` anyway) is impossible if `write_file` isn't
            // in the toolset. Once a contract is active, the full
            // toolkit returns.
            if itsy::settings::get().contract
                && itsy::session::contract::current().is_none()
                && !matches!(task_type, "explanation" | "respond")
            {
                let mutating: &[&str] = &[
                    "write_file",
                    "append_file",
                    "patch",
                    "read_and_patch",
                    "create_and_run",
                    "run",
                    "memory_remember",
                    "memory_forget",
                ];
                tools.retain(|t| {
                    let name = t
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    !mutating.contains(&name)
                });
            }
            (cfg, hist, tools)
        };

        let system_prompt = itsy::model::prompts::build_full_system_prompt(
            &cfg, task_type, &hist,
            &session.shared.read().memory, &session.shared.read().skills, &session.shared.read().plugins, &session.ro.cwd,
        );
        let chat_ctx = ChatContext {
            model_name: &cfg.model.name,
            base_url: &cfg.model.base_url,
            api_key: cfg.model.api_key.clone(),
            timeout: Duration::from_secs(cfg.model.timeout),
            temp_adapt: cfg.features.temp_adapt,
            conversation: &hist,
            tools,
            current_task_type: Some(task_type),
            system_prompt,
            force_disable_thinking: state.force_disable_thinking,
        };

        // Forensic snapshot of the request body about to be sent. Lets a
        // smarter model later replay the call from the trace JSON.
        session.mutable.lock().trace.record_chat_request(
            &cfg.model.name,
            &chat_ctx.system_prompt,
            &json!(chat_ctx.conversation),
            &json!(chat_ctx.tools),
        );

        let Some(data) = chat_completion(&chat_ctx).await else {
            session.mutable.lock().trace
                .record_error("chat_completion", "no response from model");
            println!("  \x1b[31m✗ No response from model\x1b[0m");
            break;
        };
        record_usage(&data, session, false);
        state.last_prompt_tokens = data
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(state.last_prompt_tokens);

        let Some(msg) = data.pointer("/choices/0/message").cloned() else {
            break;
        };
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let response_content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        let response_has_content = !response_content.trim().is_empty();
        if state.empty_retry_injected && (!tool_calls.is_empty() || response_has_content) {
            let mut locked = session.mutable.lock();
            if locked.history.len() >= 2 {
                locked.history.pop();
                locked.history.pop();
            }
            state.empty_retry_injected = false;
        }

        // Record trace step.
        {
            session.mutable.lock().trace
                .record_model_response(Some(response_content), Some(&tool_calls));
        }

        // 7a) Model emitted tool calls → execute them.
        if !tool_calls.is_empty() {
            state.force_disable_thinking = false;
            let mut batch_had_mutating = false;
            // Contract-lifecycle calls (mark_assertion, close_contract, etc.)
            // count as meaningful forward progress — don't accumulate toward
            // the readonly-turn nudge threshold.
            let mut batch_had_contract_progress = false;
            // Non-read-only bash (compile, test, run) counts as active work —
            // the model is executing, not just passively reading files.
            let mut batch_had_active_bash = false;
            // A read_file that returns new/changed content is a fresh read —
            // the model is legitimately reacting to a state change, not looping.
            // Keyed off the FileStateTracker's "unchanged" signal in the result text.
            let mut batch_had_fresh_read = false;
            // Widen the tool set on subsequent calls unless the model picked a
            // category via select_category (in which case it'll set it below).
            let first_tool_name = tool_calls
                .first()
                .and_then(|tc| tc.pointer("/function/name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if first_tool_name != "select_category" {
                state.current_category = Some("plan".into());
                session.mutable.lock().current_tool_category = Some("plan".into());
            }

            // Push the assistant message verbatim.
            session.mutable.lock().history.push(msg.clone());

            // Quality monitor: validate tool names before executing.
            let known: Vec<&str> = chat_ctx.tools
                .iter()
                .filter_map(|t| t.pointer("/function/name").and_then(|v| v.as_str()))
                .collect();
            if let Some(GuardAction::Inject { ref message }) = state.check_quality_monitor(
                &tool_calls, &known, &mut session.mutable.lock().history,
            ) {
                eprintln!("  \x1b[33m\u{26a0} {message}\x1b[0m");
                continue;
            }

            for tc in &tool_calls {
                state.tool_calls_this_turn += 1;
                let name = tc
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args_str = tc
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let args: Value = match serde_json::from_str(args_str) {
                    Ok(v) => v,
                    Err(_) => {
                        // Repair attempt via features adapter.
                        let repair = features_adapter::repair_tool_call(
                            args_str,
                            "Invalid JSON",
                            "",
                        )
                        .await;
                        if repair.ok {
                            if let Some(rep) = repair.repaired_call {
                                serde_json::from_str(&rep).unwrap_or_else(|_| json!({}))
                            } else {
                                json!({})
                            }
                        } else {
                            eprintln!("  \x1b[31m✗ Failed to parse args for {name}\x1b[0m");
                            json!({})
                        }
                    }
                };

                // Track contract tools + mutating state for batch progress tracking.
                let is_contract_tool = matches!(
                    name.as_str(),
                    "propose_contract" | "mark_assertion" | "mark_feature"
                        | "contract_status" | "close_contract"
                );
                if is_contract_tool { batch_had_contract_progress = true; }

                let is_mutating = match name.as_str() {
                    "write_file" | "append_file" | "patch" | "read_and_patch"
                    | "create_and_run" | "run" | "memory_remember" | "memory_forget" => true,
                    "bash" => !itsy::tools_impl::dedup::bash_is_read_only(&args),
                    _ => false,
                };
                if name == "bash" && is_mutating { batch_had_active_bash = true; }

                // Contract gate: block mutating calls before contract exists.
                if let Some(GuardAction::Inject { ref message }) = state.check_contract_gate(
                    &name, is_mutating, &id, &mut session.mutable.lock().history,
                ) {
                    println!("  \x1b[33m\u{26a0} {message}\x1b[0m");
                    continue;
                }

                // Loop detection: block identical tool calls after 3 repeats.
                if let Some(action) = state.check_loop_detection(
                    &name, &args, &id, &mut session.mutable.lock().history,
                    &mut session.mutable.lock().tool_repeat_counts,
                    &mut session.mutable.lock().mutated_paths,
                    &mut session.mutable.lock().bash_loop_keys,
                ) {
                    state.consecutive_blocks += 1;
                    if state.consecutive_blocks >= MAX_CONSECUTIVE_BLOCKS {
                        break;
                    }
                    continue;
                }

                // Patch blocked-file check: if this file was stuck-blocked
                // during a patch spiral, hard-reject any further patch calls
                // on it before execution.
                if name == "patch" || name == "read_and_patch" {
                    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                        if let Some(signal) = session.mutable.lock().early_stop.check_patch_blocked(path) {
                            println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                            session.mutable.lock().history.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": serde_json::to_string(&json!({
                                    "result": signal.injection
                                })).unwrap_or_default()
                            }));
                            state.consecutive_blocks += 1;
                            if state.consecutive_blocks >= MAX_CONSECUTIVE_BLOCKS {
                                break;
                            }
                            continue;
                        }
                    }
                }

                // Idempotent-write dedup: skip duplicate memory_remember / mark_assertion.
                if state.check_idempotent_write(
                    &name, &args, &id, &mut session.mutable.lock().history,
                ).is_some() {
                    continue;
                }

                let started = Instant::now();
                let fs_handle = session.mutable.lock().fullscreen.clone();
                if let Some(fs) = &fs_handle {
                    fs.add_tool(&name, "running", "");
                } else {
                    println!("{}", tui::tool_start(&name));
                }

                // Extract needed data under brief locks, then release before .await.
                let config = session.shared.read().config.clone();
                let read_tracker_arc = session.shared.read().read_tracker.clone();
                let file_state_arc = session.shared.read().file_state.clone();
                let snapshot_manager_arc = session.mutable.lock().snapshot_manager.clone();
                let result = {
                    let ctx = ExecCtx {
                        config: &config,
                        flags: &session.ro.flags,
                        memory: Arc::new(parking_lot::Mutex::new(MemoryStore::new(&session.ro.cwd))),
                        mcp_bridge: Some(session.ro.mcp_bridge.clone()),
                        mcp_client: None,
                        fullscreen: fs_handle.clone(),
                        read_tracker: &read_tracker_arc,
                        file_state: &file_state_arc,
                        snapshot_manager: &snapshot_manager_arc,
                    };
                    execute_tool(&name, args.clone(), &ctx).await
                };

                let elapsed_ms = started.elapsed().as_millis() as u64;

                // select_category: update the active stage-2 category.
                if name == "select_category" {
                    if let Some(cat) = result.get("category").and_then(|v| v.as_str()) {
                        state.current_category = Some(cat.to_string());
                        session.mutable.lock().current_tool_category = Some(cat.to_string());
                    }
                }

                // Trace step.
                session.mutable.lock().trace.record_tool_call(
                    &name,
                    &args,
                    &result,
                    elapsed_ms,
                );

                // Track edited files and reset readonly-turn counter on any mutating call.
                const MUTATING_TOOLS: &[&str] = &[
                    "write_file", "patch", "read_and_patch", "create_file",
                    "move_file", "delete_file", "append_file",
                ];
                if MUTATING_TOOLS.contains(&name.as_str()) && result.get("error").is_none() {
                    state.had_mutating_call = true;
                    batch_had_mutating = true;
                    session.mutable.lock().total_mutating_calls += 1;
                    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                        state.edited_files.push(p.to_string());
                        // Signal that a subsequent read_file on this path is
                        // legitimate — reset its loop counter so the agent can
                        // re-read after patching.
                        session.mutable.lock().mutated_paths.insert(p.to_string());
                    }
                    // Reset all bash loop counters after any successful mutation.
                    // The model legitimately runs the same verify command after each
                    // edit cycle (patch → coqc → patch → coqc …); only consecutive
                    // bash calls with no mutation in between indicate a stuck loop.
                    {
                        let keys: Vec<String> = session.mutable.lock().bash_loop_keys.drain().collect();
                        let mut counts = session.mutable.lock().tool_repeat_counts.clone();
                        for k in keys {
                            counts.remove(&k);
                        }
                    }
                }

                // Pretty-print outcome.
                print_tool_result(&name, &result, elapsed_ms, session.ro.flags.verbose);

                // Record success/failure for tool scoring.
                {
                    let s = &mut session.mutable.lock().scorer;
                    if result.get("error").is_none() {
                        s.record_success(&name, task_type, elapsed_ms);
                    } else {
                        let err = result
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        s.record_failure(&name, task_type, err);
                    }
                }

                // A successful (non-blocked) tool execution resets the
                // consecutive-block counter — the model has done something real.
                state.consecutive_blocks = 0;

                // Successful read_file unblocks the file for patching.
                // Also track whether the content was fresh or stale: the
                // FileStateTracker stamps "unchanged since last read" into the
                // result when the file hasn't changed. Only stale reads count
                // toward the no-progress nudge; a fresh read means the model
                // is reacting to real state changes.
                if name == "read_file" && result.get("error").is_none() {
                    let is_stale = result
                        .get("result")
                        .and_then(|v| v.as_str())
                        .map(|s| s.contains("unchanged since last read"))
                        .unwrap_or(false);
                    if !is_stale {
                        batch_had_fresh_read = true;
                    }
                    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                        session.mutable.lock().early_stop.record_read(path);
                    }
                }

                // Early-stop on patch spirals.
                if name == "patch" || name == "read_and_patch" {
                    let patch_file = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
                    let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
                    // Read-tracker rejections ("call read_file first") are not
                    // patch-content failures — don't count them toward the spiral limit.
                    let err_str = result
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let is_read_guard = err_str.contains("call read_file");
                    let is_not_found = err_str.contains("not found") && name == "patch";
                    let patch_success = result.get("error").is_none() || is_read_guard;
                    // When old_str isn't found, immediately tell the model to use
                    // read_and_patch which atomically reads then patches.
                    if is_not_found {
                        session.mutable.lock().history.push(json!({
                            "role": "user",
                            "content": format!(
                                "[SYSTEM] patch failed: old_str not found in {patch_file}. \
                                 The file content has changed since you last read it. \
                                 Use read_and_patch instead — it reads the current content \
                                 first, then applies the patch atomically."
                            )
                        }));
                    }
                    if let Some(signal) = session.mutable.lock().early_stop.record_patch_result(
                        &patch_file,
                        patch_success,
                        old_str,
                        new_str,
                    ) {
                        println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                        session.mutable.lock().history
                            .push(json!({"role": "user", "content": signal.injection}));
                        // Force model to rewrite — break out of the inner tool loop.
                        break;
                    }
                }

                // Add the tool result to history (capped).
                let tool_content = result
                    .get("result")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        result
                            .get("error")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default();
                let context_ratio = {
                    let window = itsy::settings::get().detected_window as f32;
                    if window > 0.0 { state.last_prompt_tokens as f32 / window } else { 0.0 }
                };
                let capped = itsy::executor::cap_tool_result(&tool_content, context_ratio);
                session.mutable.lock().history.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": capped,
                }));

                // Recency-weighted tool guidance: when the same tool fails
                // ≥2 times consecutively, inject a brief actionable hint.
                // Reset the counter on success so hints don't accumulate.
                {
                    // For bash/run, "Exit code 1" is informational for many tools
                    // (pdflatex with warnings, diff, test, grep). Only count it as a
                    // failure if the exit code is > 1 or it's a non-exit-code error.
                    let had_error = result.get("error").map(|e| {
                        let msg = e.as_str().unwrap_or("");
                        !((name == "bash" || name == "run") && msg == "Exit code 1")
                    }).unwrap_or(false);
                    if had_error {
                        let n = state.recent_tool_failures
                            .entry(name.clone())
                            .and_modify(|c| *c += 1)
                            .or_insert(1);
                        if *n == 2 {
                            session.mutable.lock().history.push(json!({
                                "role": "user",
                                "content": format!(
                                    "[SYSTEM] `{name}` has failed {n} times in a row. {}",
                                    recency_tool_hint(&name)
                                )
                            }));
                        }
                    } else {
                        state.recent_tool_failures.remove(&name);
                    }
                }

                // ── Improvement loop for file writes/patches ──────────────
                if (name == "write_file" || name == "patch")
                    && result.get("error").is_none()
                {
                    let Some(file_path) = args.get("path").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let file_path = file_path.to_string();

                    // Verification governor: routes to Accept / Retry / Decompose.
                    let action = session.mutable.lock().verification
                        .check_and_enforce(&file_path);
                    match action {
                        HardFailAction::Accept { .. } => {
                            if state.improvement_attempts
                                .get(&file_path)
                                .copied()
                                .unwrap_or(0)
                                > 0
                            {
                                println!(
                                    "{}",
                                    tui::improvement_fixed(
                                        &file_path,
                                        state.improvement_attempts[&file_path]
                                    )
                                );
                                state.improvement_attempts.insert(file_path.clone(), 0);
                            }
                            // LLM self-critique: ask "does this still do
                            // what the user wanted?" Cheap to wire, costs
                            // one LLM call per accepted edit when enabled.
                            if itsy::settings::get().validate_edits {
                                if let Ok(content) = std::fs::read_to_string(&file_path) {
                                    let cfg_snapshot = session.shared.read().config.clone();
                                    let result = features_adapter::validate_edit_with_config(
                                        &file_path,
                                        &content,
                                        &user_msg,
                                        &cfg_snapshot,
                                    )
                                    .await;
                                    if !result.ok && !result.issues.is_empty() {
                                        let issues = result.issues.join("\n  - ");
                                        let msg = format!(
                                            "[SEMANTIC-REVIEW] The edit to {file_path} may need follow-up:\n  - {issues}"
                                        );
                                        session.mutable.lock().history.push(json!({
                                            "role": "user",
                                            "content": msg,
                                        }));
                                    }
                                }
                            }
                        }
                        HardFailAction::Retry {
                            errors,
                            attempt,
                            escalate,
                        } => {
                            state.improvement_attempts
                                .entry(file_path.clone())
                                .and_modify(|n| *n += 1)
                                .or_insert(1);
                            let attempt_n = state.improvement_attempts[&file_path];
                            println!(
                                "{}",
                                tui::improvement_loop(
                                    &errors,
                                    attempt_n,
                                    MAX_IMPROVE_ITERATIONS
                                )
                            );
                            session.shared.write().token_monitor.record_compaction();
                            let test_hint = if !itsy::model::prompts::get_test_runner_context(&session.ro.cwd).is_empty()
                            {
                                "\n\nAfter fixing, run the project test command to verify."
                                    .to_string()
                            } else {
                                String::new()
                            };
                            let fix_prompt = format!(
                                "[AUTO-VALIDATE] Errors in {file_path} (attempt {attempt_n}/{MAX_IMPROVE_ITERATIONS}):\n{}{test_hint}\n\nFix these errors. Do NOT repeat the same approach that failed before.",
                                errors.join("\n"),
                            );
                            session.mutable.lock().history.push(json!({
                                "role": "user",
                                "content": fix_prompt,
                            }));
                            // Optional escalation hint surfaces in the governor
                            // status; we let the loop continue and try fixing.
                            let _ = (attempt, escalate);
                        }
                        HardFailAction::Decompose {
                            errors,
                            file_content,
                            lines,
                            strategy: governor_strategy,
                        } => {
                            // Try LLM-based decompose first; fall back to the
                            // governor's regex strategy.
                            let cfg_snap = session.shared.read().config.clone();
                            let strat = features_adapter::decompose_task(
                                &user_msg,
                                &errors.join("\n"),
                                &file_content.chars().take(1000).collect::<String>(),
                                &cfg_snap,
                            )
                            .await;
                            let (kind, reason, instruction) = if let Some(s) = strat {
                                (s.strategy, s.reason, s.instruction)
                            } else {
                                (
                                    governor_strategy.kind.clone(),
                                    governor_strategy.reason.clone(),
                                    governor_strategy.instruction.clone(),
                                )
                            };
                            println!("  \x1b[33m◇ DECOMPOSE: {reason}\x1b[0m");
                            println!("  \x1b[90m  Strategy: {kind}\x1b[0m");
                            state.improvement_attempts.insert(file_path.clone(), 0);
                            session.mutable.lock().history.push(json!({
                                "role": "user",
                                "content": format!("[DECOMPOSE] After {MAX_IMPROVE_ITERATIONS} failed fix attempts, changing strategy.\n\n{instruction}\n\nFile length: {lines} lines."),
                            }));
                            // Avoid using the unused field by recording the
                            // governor's chosen strategy in the trace.
                            let _ = pick_decompose_strategy(
                                &file_content,
                                &errors,
                                &file_path,
                            );
                        }
                    }

                    // Also run the inline runValidation (lint/compile/etc.)
                    // as a quick sanity check — surfaces in trace recorder.
                    if let Some(v) = run_validation(&file_path) {
                        session.mutable.lock().trace.record_validation(
                            &file_path,
                            v.passed,
                            &v.errors,
                        );
                    }
                }

                // ── Improvement loop for failing bash/run commands ────────
                if (name == "bash" || name == "run" || name == "create_and_run")
                    && result.get("error").is_some()
                {
                    let counter = state.improvement_attempts
                        .entry("__bash".into())
                        .and_modify(|n| *n += 1)
                        .or_insert(1);
                    let attempt_n = *counter;
                    if attempt_n <= 2 {
                        let err = result
                            .get("result")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(800)
                            .collect::<String>();
                        session.mutable.lock().history.push(json!({
                            "role": "user",
                            "content": format!("[AUTO-FIX] The command FAILED (attempt {attempt_n}/2). Do NOT claim success. The error was:\n{err}\n\nRead the error, identify the bug, and fix it."),
                        }));
                    } else {
                        let cfg_snap = session.shared.read().config.clone();
                        let strategy = features_adapter::decompose_task(
                            &user_msg,
                            result
                                .get("result")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            args.get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            &cfg_snap,
                        )
                        .await
                        .map(|s| (s.strategy, s.reason, s.instruction))
                        .unwrap_or_else(|| {
                            let g = pick_decompose_strategy(
                                "",
                                &[result
                                    .get("result")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(300)
                                    .collect::<String>()],
                                args.get("command").and_then(|v| v.as_str()).unwrap_or(""),
                            );
                            (g.kind, g.reason, g.instruction)
                        });
                        println!(
                            "  \x1b[33m◇ DECOMPOSE bash: {}\x1b[0m",
                            strategy.1
                        );
                        session.mutable.lock().history.push(json!({
                            "role": "user",
                            "content": format!("[DECOMPOSE] The command has failed 3 times. STOP retrying the same approach.\n\n{}", strategy.2),
                        }));
                        state.improvement_attempts.insert("__bash".into(), 0);
                    }
                } else if (name == "bash" || name == "run")
                    && result.get("error").is_none()
                {
                    state.improvement_attempts.insert("__bash".into(), 0);
                }

                // Early-stop on bash failure loops (8 consecutive non-zero exits).
                // Exit code 1 is informational for many programs (pdflatex, diff,
                // test, grep) — only count exit codes > 1 as hard failures.
                if name == "bash" || name == "run" {
                    let exit_code = result.get("error")
                        .and_then(|e| e.as_str())
                        .and_then(|msg| msg.strip_prefix("Exit code "))
                        .and_then(|n| n.parse::<i32>().ok())
                        .filter(|&c| c != 1)   // exit 1 = informational, not a hard failure
                        .unwrap_or_else(|| if result.get("error").is_none() { 0 } else { 2 });
                    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(signal) = session.mutable.lock().early_stop.record_bash_result(exit_code, command) {
                        println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                        session.mutable.lock().history
                            .push(json!({"role": "user", "content": signal.injection}));
                        break;
                    }
                }
            }
            // Reset the text-only streak counter whenever a tool-call batch fires.
            state.per_turn_repeats.insert("__text_only_streak".into(), 0);

            // No-progress nudge within a single handle_turn: fires when the
            // No-progress nudge: if N consecutive read-only batches, push model to act.
            if let Some(GuardAction::Inject { ref message }) = state.check_no_progress(
                batch_had_mutating, batch_had_contract_progress,
                batch_had_active_bash, batch_had_fresh_read,
                session,
            ) {
                println!("  \x1b[33m\u{26a0} {message}\x1b[0m");
            }

            // Loop back to chat_completion — model may want to call more tools.
            continue;
        }

        // 7b) No tool calls → model is responding with text.
        let content_opt = msg.get("content").and_then(|c| c.as_str()).map(String::from);
        let content_trimmed_len = content_opt.as_deref().map(|c| c.trim().len()).unwrap_or(0);

        // Empty-response retry: model returned no content AND no tool
        // calls. This is the IQ2_XXS "I give up" failure mode. Push the
        // empty assistant turn (so the model sees its own no-op) and ask
        // it to try again. Bounded by `MAX_EMPTY_RETRIES` to prevent
        // spinning on a model that keeps refusing.
        if state.tool_calls_this_turn == 0
            && state.current_category.as_deref() != Some("respond")
            && content_trimmed_len == 0
        {
            const MAX_EMPTY_RETRIES: u32 = 2;
            let n = *state.per_turn_repeats.entry("__empty_response".into()).and_modify(|n| *n += 1).or_insert(1);
            if n <= MAX_EMPTY_RETRIES {
                state.force_disable_thinking = true;
                state.empty_retry_injected = true;
                println!("  \x1b[33m⚠ Model returned empty response — retrying ({n}/{MAX_EMPTY_RETRIES}) with thinking disabled\x1b[0m");
                session.mutable.lock().history.push(json!({
                    "role": "assistant",
                    "content": content_opt.clone().unwrap_or_default(),
                }));
                session.mutable.lock().history.push(json!({
                    "role": "user",
                    "content": "[SYSTEM] Your previous turn was empty. Thinking is disabled for the retry. Respond with exactly one concrete next step: either call a tool, or give a direct text answer. Do not return an empty turn.",
                }));
                continue;
            }
            // Exhausted retries: surface to user, stop spinning.
            println!("  \x1b[31m✗ Model returned empty responses {n} times — giving up on this turn.\x1b[0m");
            session.mutable.lock().trace.record_error("empty_response", &format!("{n} consecutive empty responses"));
            session.mutable.lock().history.push(json!({
                "role": "assistant",
                "content": "(no response from model after multiple retries)",
            }));
            break;
        }
        state.force_disable_thinking = false;

        // Badger: action-task gave no-tool short response. Skip when the
        // router pinned us to `respond` (no tools available — badgering
        // would spin the loop), and only fire when there's actual content.
        if state.tool_calls_this_turn == 0
            && state.current_category.as_deref() != Some("respond")
            && matches!(task_type, "coding" | "editing" | "backend")
            && content_opt
                .as_deref()
                .map(|c| !c.contains('?') && c.len() < 200 && !c.is_empty())
                .unwrap_or(false)
        {
            if let Some(content) = &content_opt {
                let mut locked = session.mutable.lock();
                locked.history.push(json!({"role": "assistant", "content": content}));
                locked.history.push(json!({
                    "role": "user",
                    "content": "[SYSTEM] You responded without using any tools. This task requires file operations. Use the appropriate tools (read_file, write_file, patch, etc.) — actually do it.",
                }));
                continue;
            }
        }

        // Text-only streak guard: force tool use after N consecutive text responses.
        if let Some(GuardAction::Inject { ref message }) = state.check_text_only_streak(
            content_opt.as_deref(), &mut session.mutable.lock().history,
        ) {
            println!("  \x1b[33mM-bM-^ZM-  {message}\x1b[0m");
        }

        // Contract close-the-loop guard: refuse to end turn with unclosed assertions.
        if let Some(GuardAction::Inject { ref message }) = state.check_contract_close_loop(
            content_opt.as_deref(), &mut session.mutable.lock().history,
        ) {
            println!("  \x1b[33m\u{26a0} {message}\x1b[0m");
            continue;
        }
        // Stream/print the final response.
        if let Some(content) = &content_opt {
            session.mutable.lock().history
                .push(json!({"role": "assistant", "content": content}));

            let fs_assist = session.mutable.lock().fullscreen.clone();
            if let Some(fs) = &fs_assist {
                fs.add_chat(itsy::fullscreen::ChatRole::Assistant, content.clone());
            } else {
                println!("{}", tui::render_markdown(content));
            }
        } else if state.tool_calls_this_turn == 0 {
            // No content + no tool calls + nothing tried — try streaming.
            let cfg = session.shared.read().config.clone();
            let hist = session.mutable.lock().history.clone();
            let fs_handle = session.mutable.lock().fullscreen.clone();
            // Swap early_stop out of the lock so the guard is dropped before .await.
            let mut early = std::mem::take(&mut session.mutable.lock().early_stop);
            if let Some(ref fs) = fs_handle {
                fs.set_streaming(true);
            }
            if let Some(out) = stream_final_response(
                &cfg.model.name,
                &cfg.model.base_url,
                cfg.model.api_key.as_deref(),
                cfg.model.timeout,
                &hist,
                Some(&mut early),
                |tok| {
                    if let Some(ref fs) = fs_handle {
                        fs.stream_token(tok);
                    } else {
                        print!("{tok}");
                        io::stdout().lock().flush().ok();
                    }
                },
            )
            .await
            {
                let _ = early;
                session.mutable.lock().history
                    .push(json!({"role": "assistant", "content": out}));
            }
            if let Some(ref fs) = fs_handle {
                fs.end_stream();
            } else {
                println!();
            }
        }
        break;
    }

    // Show the contract's current state once at end of turn so the user
    // sees the model's own scoreboard alongside its text reply.
    if itsy::settings::get().contract {
        if let Some(c) = itsy::session::contract::current() {
            println!("{}", tui::render_contract(&c));
        }
    }

    // Update readonly-turn counter. Resets when any mutating call succeeded;
    // increments otherwise so the next turn's nudge threshold check is current.
    {
        let count = &mut session.mutable.lock().readonly_turn_count;
        if state.had_mutating_call {
            *count = 0;
        } else {
            *count += 1;
        }
    }

    if state.tool_calls_this_turn > 0 {
        println!("{}", tui::turn_summary(state.tool_calls_this_turn));

        // Auto-commit (Feature: git.auto_commit).
        let auto_commit = itsy::settings::get().auto_commit;
        if auto_commit {
            try_auto_commit(&session.ro.cwd, &user_msg, &state.edited_files).await;
        }
    }

    // Stop the trace recorder for this turn.
    let _ = session.mutable.lock().trace.stop();
}

fn record_usage(data: &Value, session: &AgentSession, is_tool_call: bool) {
    if let Some(usage) = data.get("usage") {
        let pt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let ct = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let model = itsy::settings::get().model_name.clone();
        session.shared.write().tokens
            .record(&json!({"usage": usage}), &model);
        session.shared.write().token_monitor.record_call(
            pt,
            ct,
            CallMetadata {
                new_turn: false,
                is_tool_call,
            },
        );
        session.mutable.lock().trace.record_tokens(pt, ct);
    }
}

fn print_tool_result(name: &str, result: &Value, elapsed_ms: u64, verbose: bool) {
    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        println!("  {}", tui::tool_error(err));
    } else if let Some(action) = result.get("action").and_then(|v| v.as_str()) {
        let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let lines = result.get("lines").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        match action {
            "Created" => println!("  {}", tui::tool_created(path, lines, elapsed_ms)),
            "Updated" => println!("  {}", tui::tool_updated(path, lines, elapsed_ms)),
            "Edited" => {
                let line_num = result.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                println!("  {}", tui::tool_edited(path, line_num, elapsed_ms));
            }
            _ => println!("  {}", tui::tool_success(name, elapsed_ms)),
        }
    } else if let Some(cmd) = result.get("command").and_then(|v| v.as_str()) {
        println!("  {}", tui::tool_bash(cmd, elapsed_ms));
    } else if let Some(out) = result.get("result").and_then(|v| v.as_str()) {
        if verbose {
            println!("{out}");
        } else {
            let summary = out.lines().next().unwrap_or(out).trim_end_matches(':');
            println!("  {}", tui::tool_success(&itsy::executor::truncate_short(summary, 80), elapsed_ms));
        }
    } else {
        println!("  {}", tui::tool_success("", elapsed_ms));
    }
}

async fn try_auto_commit(cwd: &Path, task: &str, edited: &[String]) {
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output();
    let Ok(out) = status else { return };
    if out.stdout.is_empty() {
        return;
    }
    let edited_vec: Vec<String> = edited.to_vec();
    let msg = features_adapter::generate_commit_message(task, &edited_vec).await;
    let _ = Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .status();
    if Command::new("git")
        .args(["commit", "-m", &msg])
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        println!("  \x1b[32m✓ git commit: {}\x1b[0m", itsy::executor::truncate_short(&msg, 60));
    }
}

// ── Non-interactive entrypoint ───────────────────────────────────────────────

async fn run_non_interactive(session: &AgentSession, prompt: Option<String>) {
    let prompt = match prompt {
        Some(p) if !p.is_empty() => p,
        _ => {
            let mut buf = String::new();
            let _ = io::stdin().lock().read_to_string(&mut buf);
            buf.trim().to_string()
        }
    };
    if prompt.is_empty() {
        eprintln!("No prompt provided.");
        std::process::exit(1);
    }
    handle_turn(&prompt, session).await;

    // Adversarial evaluator — runs after the generator finishes if the
    // contract feature is on and the generator closed the contract as
    // completed. The evaluator gets a fresh context window (no generator
    // history) and tries to break the solution by running checks itself.
    if itsy::settings::get().contract {
        if let Some(completed) = itsy::session::contract::take_completed() {
            run_evaluator_phase(&prompt, session, &completed).await;
        }
    }
}

// ── Adversarial evaluator ────────────────────────────────────────────────

fn evaluator_tools() -> Vec<Value> {
    fn tool(name: &str, desc: &str, params: Value) -> Value {
        json!({"type":"function","function":{"name":name,"description":desc,"parameters":params}})
    }
    vec![
        tool("bash", "Run a shell command and return stdout+stderr. Use for verification only.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})),
        tool("read_file", "Read a file. Returns content.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})),
        tool("evaluator_verdict",
            "Report your evaluation verdict. Call this ONCE when you have checked all assertions.",
            json!({
                "type": "object",
                "properties": {
                    "passed": {"type":"boolean","description":"true if every assertion verified; false if any failed"},
                    "findings": {"type":"string","description":"Describe each failure on its own line. Empty if passed=true."}
                },
                "required": ["passed","findings"]
            })),
    ]
}

/// Run a bash command for the evaluator (read-only intent, 15s timeout).
fn evaluator_run_bash(command: &str, cwd: &str) -> String {
    let result = run_bash_with_timeout(command, cwd, std::time::Duration::from_secs(15));
    let mut s = format!("exit_code={}", result.exit_code);
    if !result.stdout.is_empty() {
        s.push_str(&format!("\nstdout:\n{}", result.stdout.trim_end()));
    }
    if !result.stderr.is_empty() {
        s.push_str(&format!("\nstderr:\n{}", result.stderr.trim_end()));
    }
    if s.len() > 2000 {
        s.truncate(2000);
        s.push_str("\n[output truncated]");
    }
    s
}

struct BashOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn run_bash_with_timeout(command: &str, cwd: &str, timeout: std::time::Duration) -> BashOutput {
    use std::process::Command;
    use std::io::Read;
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

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut child_stdout = child.stdout.take().expect("piped stdout");
    let mut child_stderr = child.stderr.take().expect("piped stderr");
    let start = std::time::Instant::now();

    // Wait for the child to finish, with a timeout.
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            return BashOutput { exit_code: -1, stdout: String::new(), stderr: "timed out".into() };
        }

        // Read available data from stdout/stderr.
        let _ = child_stdout.read_to_end(&mut stdout);
        let _ = child_stderr.read_to_end(&mut stderr);

        match child.try_wait() {
            Ok(Some(status)) => {
                // Child exited — read any remaining output.
                let _ = child_stdout.read_to_end(&mut stdout);
                let _ = child_stderr.read_to_end(&mut stderr);
                return BashOutput {
                    exit_code: status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                };
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => return BashOutput { exit_code: -1, stdout: String::new(), stderr: format!("error: {e}") },
        }
    }
}

fn evaluator_read_file(path: &str, cwd: &str) -> String {
    let p = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        std::path::Path::new(cwd).join(path)
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            if s.len() > 3000 {
                format!("{}\n[file truncated at 3000 chars]", &s[..3000])
            } else {
                s
            }
        }
        Err(e) => format!("error reading {}: {e}", p.display()),
    }
}

/// Adversarial evaluation phase. Runs after the generator closes the
/// contract. Gets a clean history, tool-restricted to read+bash only,
/// and a system prompt that instructs it to treat self-reported passes
/// as unconfirmed.
async fn run_evaluator_phase(
    task: &str,
    session: &AgentSession,
    contract: &itsy::session::contract::Contract,
) {
    const MAX_TURNS: u32 = 50;

    let cwd = session.ro.cwd.to_string_lossy().to_string();
    let mut config = session.shared.read().config.clone();
    // Use second-opinion model/endpoint if configured.
    if let Some(m) = config.second_opinion.model.clone() {
        config.model.name = m;
    }
    if let Some(e) = config.second_opinion.endpoint.clone() {
        config.model.base_url = e;
    }

    let system_prompt = format!(
        "You are an adversarial evaluator. A code generator claimed to have completed the \
         following task — your job is to independently verify whether it actually did.\n\n\
         TASK:\n{task}\n\n\
         Working directory: {cwd}\n\n\
         You have no knowledge of what the generator did or claimed. Devise your own \
         verification strategy from the task description alone. Use `bash` to run commands \
         and observe real output. Use `read_file` to inspect files.\n\n\
         When you have enough evidence, call `evaluator_verdict` with:\n\
         - passed=true if the task is genuinely complete\n\
         - passed=false + findings describing what is wrong or missing\n\n\
         Be terse and direct. If the first check reveals a clear failure, call \
         evaluator_verdict immediately."
    );

    let tools = evaluator_tools();
    let mut history: Vec<Value> = vec![json!({
        "role": "user",
        "content": "Begin evaluation."
    })];

    println!("\n  \x1b[36m◆ evaluator phase starting ({} assertions)\x1b[0m", contract.assertions.len());

    for turn in 0..MAX_TURNS {
        let ctx = itsy::model_client::ChatContext {
            model_name: &config.model.name,
            base_url: &config.model.base_url,
            api_key: config.model.api_key.clone(),
            timeout: Duration::from_secs(config.model.timeout),
            temp_adapt: config.features.temp_adapt,
            conversation: &history,
            tools: tools.clone(),
            current_task_type: Some("evaluation"),
            system_prompt: system_prompt.clone(),
            force_disable_thinking: false,
        };

        let Some(response) = itsy::model_client::chat_completion(&ctx).await else {
            println!("  \x1b[31m✗ evaluator API call failed\x1b[0m");
            return;
        };

        let msg = response
            .pointer("/choices/0/message")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Push assistant message with reasoning_content stripped — evaluator
        // history should stay compact.
        let mut clean_msg = msg.clone();
        if let Some(obj) = clean_msg.as_object_mut() {
            obj.remove("reasoning_content");
        }
        history.push(clean_msg);

        if tool_calls.is_empty() {
            // Text-only response — nudge toward a verdict if not at limit yet.
            if turn < MAX_TURNS - 1 {
                history.push(json!({
                    "role": "user",
                    "content": "[SYSTEM] Call evaluator_verdict to record your conclusion."
                }));
                continue;
            }
            println!("  \x1b[33m⚠ evaluator reached turn limit without verdict\x1b[0m");
            return;
        }

        let mut tool_results = Vec::new();
        let mut verdict: Option<(bool, Vec<String>)> = None;

        for tc in &tool_calls {
            let name = tc
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args_str = tc
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            let tc_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let result = match name.as_str() {
                "bash" => {
                    let cmd = args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("  \x1b[36m  eval$ {}\x1b[0m", cmd);
                    evaluator_run_bash(cmd, &cwd)
                }
                "read_file" => {
                    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    evaluator_read_file(path, &cwd)
                }
                "evaluator_verdict" => {
                    let passed = args
                        .get("passed")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let findings_raw = args
                        .get("findings")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let findings: Vec<String> = findings_raw
                        .lines()
                        .map(|l| l.trim_start_matches('-').trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    verdict = Some((passed, findings.clone()));
                    format!("verdict recorded: passed={passed}")
                }
                other => format!("tool `{other}` not available in evaluator"),
            };

            tool_results.push(json!({
                "role": "tool",
                "tool_call_id": tc_id,
                "content": result,
            }));
        }

        // Countdown nudge: with 2 turns left, append a warning to the last
        // tool result so the model sees it before its next response.
        let turns_left = MAX_TURNS - 1 - turn;
        if turns_left <= 2 && verdict.is_none() {
            let last = tool_results.last_mut().expect("at least one tool result");
            let existing = last["content"].as_str().unwrap_or("").to_string();
            last["content"] = json!(format!(
                "{existing}\n\n[SYSTEM] {turns_left} turn(s) remaining. \
                 Call evaluator_verdict NOW — do not run any more bash commands."
            ));
        }

        history.extend(tool_results);

        if let Some((passed, findings)) = verdict {
            evaluator_emit_result(passed, &findings, session);
            return;
        }
    }

    // Turn budget exhausted without a verdict — treat as inconclusive fail.
    println!("  \x1b[33m⚠ evaluator exhausted turns without reaching verdict\x1b[0m");
    evaluator_emit_result(false, &["evaluation inconclusive: turn limit reached".into()], session);
}

fn evaluator_emit_result(passed: bool, findings: &[String], session: &AgentSession) {
    if passed {
        println!("  \x1b[32m✓ evaluator passed — all assertions confirmed\x1b[0m");
    } else {
        println!("  \x1b[31m✗ evaluator failed — {} finding(s)\x1b[0m", findings.len());
        for f in findings {
            println!("    · {f}");
        }
        let findings_text = findings.join("\n  · ");
        session.mutable.lock().history.push(json!({
            "role": "user",
            "content": format!(
                "[EVALUATOR] Independent verification found issues:\n  · {findings_text}\n\n\
                 These assertions were self-reported as passed but failed when checked \
                 by the evaluator."
            )
        }));
    }
    itsy::session::contract::set_evaluator_result(
        itsy::session::contract::EvaluatorResult { passed, findings: findings.to_vec() },
    );
}

// std::io::Read import for read_to_string above.
use std::io::Read;

// ── MCP server mode ──────────────────────────────────────────────────────────

/// Minimal MCP server speaking JSON-RPC 2.0 over stdio. Mirrors the JS
/// `runMCP()` / `handleMCPRequest` / `handleMCPToolCall` triad.
async fn run_mcp(session: Arc<AgentSession>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = TokioBufReader::new(stdin).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle_mcp_request(req, &session).await,
            Err(_) => json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "Parse error" },
            }),
        };
        println!("{response}");
    }
    Ok(())
}

async fn handle_mcp_request(request: Value, session: &AgentSession) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    match method.as_str() {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "itsy", "version": env!("CARGO_PKG_VERSION") },
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {"name": "itsy_read_file", "description": "Read file contents", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
                    {"name": "itsy_search", "description": "Search code with regex", "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string"}}, "required": ["pattern"]}},
                    {"name": "itsy_patch", "description": "Edit file via search-and-replace", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"}}, "required": ["path", "old_str", "new_str"]}},
                    {"name": "itsy_bash", "description": "Run shell command", "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
                    {"name": "itsy_memory_load", "description": "Load relevant project memory", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}},
                    {"name": "itsy_memory_remember", "description": "Save knowledge to project memory", "inputSchema": {"type": "object", "properties": {"type": {"type": "string"}, "title": {"type": "string"}, "content": {"type": "string"}}, "required": ["type", "title", "content"]}},
                    {"name": "itsy_agent", "description": "Send a prompt to itsy", "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}},
                ]
            }
        }),
        "tools/call" => handle_mcp_tool_call(id, request.get("params").cloned(), session).await,
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Unknown method: {method}") }
        }),
    }
}

async fn handle_mcp_tool_call(id: Value, params: Option<Value>, session: &AgentSession) -> Value {
    let params = params.unwrap_or(Value::Null);
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let cwd = session.ro.cwd.clone();

    let result_text = match name.as_str() {
        "itsy_read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match std::fs::read_to_string(cwd.join(path)) {
                Ok(s) => s,
                Err(e) => format!("Error: {e}"),
            }
        }
        "itsy_bash" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if regex::Regex::new(r"rm\s+-rf\s+/[^.]").expect("valid regex literal")
                .is_match(command)
                || regex::Regex::new(r"(?i)format\s+c:").expect("valid regex literal")
                    .is_match(command)
            {
                "Error: destructive command blocked".into()
            } else {
                let out = Command::new("sh")
                    .args(["-c", command])
                    .current_dir(&cwd)
                    .output();
                match out {
                    Ok(o) => {
                        let combined = format!(
                            "{}{}",
                            String::from_utf8_lossy(&o.stdout),
                            String::from_utf8_lossy(&o.stderr)
                        );
                        combined.chars().take(4000).collect()
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
        }
        "itsy_search" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let out = Command::new("rg")
                .args(["--line-number", "--max-count", "10", pattern, path])
                .current_dir(&cwd)
                .output();
            match out {
                Ok(o) if !o.stdout.is_empty() => {
                    String::from_utf8_lossy(&o.stdout).chars().take(3000).collect()
                }
                _ => "No matches".into(),
            }
        }
        "itsy_patch" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
            let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
            let full = cwd.join(path);
            match std::fs::read_to_string(&full) {
                Ok(content) => {
                    if !content.contains(old_str) {
                        "Error: old_str not found".into()
                    } else if content.matches(old_str).count() > 1 {
                        "Error: old_str matches multiple locations".into()
                    } else {
                        let updated = content.replace(old_str, new_str);
                        std::fs::write(&full, updated)
                            .map(|_| format!("Patched {path}"))
                            .unwrap_or_else(|e| format!("Error: {e}"))
                    }
                }
                Err(e) => format!("Error: {e}"),
            }
        }
        "itsy_memory_load" => {
            let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let items = session.shared.read().memory.load_for_task(task);
            if items.is_empty() {
                "No relevant memory found.".into()
            } else {
                items
                    .iter()
                    .map(|o| format!("[{}] {}: {}", o.kind, o.title, o.content))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        }
        "itsy_memory_remember" => {
            let kind = args.get("type").and_then(|v| v.as_str()).unwrap_or("context");
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let obj = session.shared.write().memory
                .remember(kind, title, content, Vec::new());
            format!("Remembered: [{}] {} ({})", obj.kind, obj.title, obj.id)
        }
        "itsy_agent" => {
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            handle_turn(&message, session).await;
            "Agent finished.".into()
        }
        other => format!("Unknown tool: {other}"),
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": result_text }] }
    })
}

// ── Eval mode ────────────────────────────────────────────────────────────────

async fn run_eval(config: &Config, suite_name: &str) -> i32 {
    let name = match known_suite(suite_name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("  \x1b[31m✗ {e}\x1b[0m");
            return 2;
        }
    };
    println!("\n  Running evaluation: {name}\n");
    let mut runner = EvalRunner::new(config);
    let result = match name {
        "classify_accuracy" => runner.run_classify(),
        "tool_selection" => {
            runner
                .run_tool_selection(|cfg, input| {
                    let cfg = cfg.clone();
                    let input = input.to_string();
                    async move {
                        let conv = vec![json!({"role": "user", "content": input})];
                        let chat_ctx = ChatContext {
                            model_name: &cfg.model.name,
                            base_url: &cfg.model.base_url,
                            api_key: cfg.model.api_key.clone(),
                            timeout: Duration::from_secs(cfg.model.timeout),
                            temp_adapt: cfg.features.temp_adapt,
                            conversation: &conv,
                            tools: get_all_tools(&cfg, None, &ToolDeps::default()),
                            current_task_type: None,
                            system_prompt: build_system_prompt(&cfg, "", "", "", None),
                            force_disable_thinking: false,
                        };
                        let data = chat_completion(&chat_ctx).await?;
                        let calls = data.pointer("/choices/0/message/tool_calls")?.as_array()?;
                        let first = calls.first()?;
                        first
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    }
                })
                .await
        }
        "response_quality" => {
            runner
                .run_response_quality(|cfg, input| {
                    let cfg = cfg.clone();
                    let input = input.to_string();
                    async move {
                        let conv = vec![json!({"role": "user", "content": input})];
                        let chat_ctx = ChatContext {
                            model_name: &cfg.model.name,
                            base_url: &cfg.model.base_url,
                            api_key: cfg.model.api_key.clone(),
                            timeout: Duration::from_secs(cfg.model.timeout),
                            temp_adapt: cfg.features.temp_adapt,
                            conversation: &conv,
                            tools: Vec::new(),
                            current_task_type: None,
                            system_prompt: build_system_prompt(&cfg, "", "", "", None),
                            force_disable_thinking: false,
                        };
                        let data = chat_completion(&chat_ctx).await?;
                        data.pointer("/choices/0/message/content")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    }
                })
                .await
        }
        _ => unreachable!(),
    };
    println!("{}", format_results(&result));
    println!();
    if result.failed > 0 { 1 } else { 0 }
}

// ── Boot helpers ─────────────────────────────────────────────────────────────

/// Probe the llama-server /props endpoint for the configured context window.
/// Returns `None` when the server doesn't expose /props (non-llama backends).
async fn probe_context_window(base_url: &str) -> Option<u32> {
    // Strip /v1 suffix — /props lives at the root of the llama.cpp HTTP server.
    let root = base_url
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or(base_url.trim_end_matches('/'));
    let url = format!("{root}/props");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    data.pointer("/default_generation_settings/n_ctx")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
}

fn build_session(config: Config, flags: Flags, cwd: PathBuf, mcp_bridge: Arc<McpBridge>) -> AgentSession {
    let mut session_store = SessionStore::new(cwd.clone());
    session_store.create();

    let mut skills = SkillManager::with_project_dir(&cwd);
    skills.load_from(&cwd);

    let mut plugins = PluginLoader::new();
    plugins.load_all(&cwd);

    AgentSession {
        ro: AgentSessionReadOnly {
            flags,
            cwd: cwd.clone(),
            mcp_bridge,
        },
        shared: parking_lot::RwLock::new(AgentSessionShared {
            config,
            memory: MemoryStore::new(&cwd),
            skills,
            plugins,
            tokens: TokenTracker::new(),
            token_monitor: TokenMonitor::new(),
            sessions: session_store,
            read_tracker: Arc::new(itsy::tools_impl::read_tracker::ReadTracker::new()),
            file_state: Arc::new(itsy::session::file_state::FileStateTracker::new()),
        }),
        mutable: parking_lot::Mutex::new(AgentSessionMutable {
            history: Vec::new(),
            scorer: ToolScorer::new(),
            verification: VerificationHistory::default(),
            early_stop: EarlyStopDetector::new(),
            trace: TraceRecorder::new(cwd.clone()),
            current_tool_category: None,
            fullscreen: None,
            tool_repeat_counts: std::collections::HashMap::new(),
            mutated_paths: std::collections::HashSet::new(),
            bash_loop_keys: std::collections::HashSet::new(),
            readonly_turn_count: 0,
            total_mutating_calls: 0,
            snapshot_manager: Arc::new(itsy::session::snapshot::SnapshotManager::new(cwd.clone())),
        }),
    }
}

fn make_cmd_ctx(session: &AgentSession) -> CommandCtx {
    let shared = session.shared.read();
    CommandCtx {
        config: Arc::new(parking_lot::Mutex::new(shared.config.clone())), // snapshot; command writes go through settings::update()
        history: {
            let hist = session.mutable.lock().history.clone();
            Arc::new(parking_lot::Mutex::new(hist))
        },
        memory: Arc::new(parking_lot::Mutex::new(MemoryStore::new(&session.ro.cwd))),
        tokens: Arc::new(parking_lot::Mutex::new(shared.tokens.clone())),
        cwd: Some(session.ro.cwd.clone()),
        token_monitor: Some(Arc::new(parking_lot::Mutex::new(TokenMonitor::new()))),
        sessions: { let mut s = SessionStore::new(session.ro.cwd.clone()); s.create(); Some(Arc::new(parking_lot::Mutex::new(s))) },
        multi: None,
        undo: None,
        snapshots: None,
        trace: Some(Arc::new(parking_lot::Mutex::new(TraceRecorder::new(session.ro.cwd.clone())))),
        skills: { let mut s = SkillManager::with_project_dir(&session.ro.cwd); s.load_from(&session.ro.cwd); Some(Arc::new(parking_lot::Mutex::new(s))) },
        plugins: { let mut p = PluginLoader::new(); p.load_all(&session.ro.cwd); Some(Arc::new(parking_lot::Mutex::new(p))) },
        lsp: None,
    }
}

// ── REPL ─────────────────────────────────────────────────────────────────────

async fn run_repl(session: &AgentSession) -> Result<()> {
    let cmd_ctx = make_cmd_ctx(session);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = String::new();
    loop {
        {
            let locked = session.mutable.lock();
            print!("{}\n> ", tui::render_status(locked.history.len()));
            stdout.lock().flush().ok();
        }
        input.clear();
        if stdin.lock().read_line(&mut input)? == 0 {
            // EOF — Ctrl+D in classic mode → exit.
            break;
        }
        // A Ctrl+C while we were waiting for input. read_line() returned an
        // empty line (or partial) after the SIGINT was delivered; treat it as
        // a "quit" if the user already hit it once with no input pending.
        if itsy::interrupt::take() > 0 && input.trim().is_empty() {
            println!("\n  bye");
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
        handle_turn(&line, session).await;
    }
    Ok(())
}

/// Fullscreen ratatui REPL — the default interactive mode.
///
/// Spawns the blocking `fullscreen::run_loop` on a dedicated thread; the
/// closures forward submitted input through an mpsc channel that this async
/// task drains, dispatching to `handle_turn` / `handle_command` like the
/// classic REPL does.
async fn run_fullscreen_repl(session: Arc<AgentSession>) -> Result<()> {
    use itsy::fullscreen::{Fullscreen, Theme};
    use tokio::sync::mpsc;

    // Build the renderer and stash it on the session so executor + handle_turn
    // can route their output (diffs, tool indicators, streamed tokens).
    let fs = Arc::new(Fullscreen::with_theme(Theme::from_env()));
    {
        // Seed the status bar from the current config + cwd.
        fs.set_model(itsy::settings::get().model_name.clone());
        fs.set_status(format!("cwd: {}", session.ro.cwd.display()));
    }

    session.mutable.lock().fullscreen = Some(fs.clone());

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // UI thread: blocking crossterm event loop.
    let ui_state = fs.state.clone();
    let tx_submit = tx.clone();
    let tx_command = tx.clone();
    let ui_handle = std::thread::spawn(move || {
        let _ = itsy::fullscreen::run_loop(
            ui_state,
            move |text| {
                let _ = tx_submit.send(text);
            },
            move |cmd| {
                let _ = tx_command.send(cmd);
            },
        );
    });

    let cmd_ctx = make_cmd_ctx(&session);

    // Async dispatch loop — exits when the UI quits (channel closes) or the
    // user runs `/quit`.
    loop {
        // Refresh the status bar each tick. Use a short timeout so the loop
        // can poll the quit flag the UI sets.
        let recv = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;
        // Check whether the UI thread has set the quit flag (Ctrl+Q, Esc).
        if fs.state.lock().quit {
            break;
        }
        // Periodic status refresh.
        {
            fs.set_model(itsy::settings::get().model_name.clone());
            let totals = session.shared.read().tokens.stats();
            fs.set_token_count(totals.prompt as u32, totals.completion as u32);
        }
        let payload = match recv {
            Ok(Some(p)) => p,
            Ok(None) => break,            // channel closed
            Err(_) => continue,           // tick — loop back for a redraw cycle
        };
        if payload.starts_with('/') {
            match handle_command(&payload, &cmd_ctx).await {
                Ok(CommandResult::Quit) => {
                    fs.request_quit();
                    break;
                }
                Ok(CommandResult::Print(s)) => {
                    fs.add_chat(itsy::fullscreen::ChatRole::System, s);
                }
                Ok(CommandResult::Continue) => {}
                Err(e) => {
                    fs.add_chat(itsy::fullscreen::ChatRole::System, format!("error: {e}"));
                }
            }
        } else {
            handle_turn(&payload, &session).await;
        }
    }

    // Signal the UI to tear down, then wait for the thread to exit so the
    // alt-screen / raw-mode state is restored before main() returns.
    fs.request_quit();
    let _ = ui_handle.join();
    // Drop the renderer from the session so executor stops routing into a
    // dead handle if anything async fires later.
    session.mutable.lock().fullscreen = None;
    Ok(())
}

/// Build [`Config`] and [`Flags`] from CLI args, then merge everything into
/// global [`Settings`]. Exits the process on errors (missing model, bad --set).
fn build_config_and_settings(cli: &Cli) -> (Config, Flags) {
    let flags = Flags {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        endpoint: cli.endpoint.clone(),
        base_url: cli.endpoint.clone(),
        classic: cli.classic,
        verbose: cli.verbose,
    };
    let mut config = load_config(&flags);

    if let Some(m) = cli.second_opinion_model.clone() {
        config.second_opinion.model = Some(m);
    }
    if let Some(e) = cli.second_opinion_endpoint.clone() {
        config.second_opinion.endpoint = Some(e);
    }

    if config.model.name.is_empty() {
        eprintln!("\n  ✗ No model configured.");
        eprintln!("  Edit {}", itsy::paths::config_file().display());
        eprintln!("  Run `itsy --init` to re-run the setup wizard.\n");
        std::process::exit(1);
    }

    // Build the merged Settings (config + CLI) and install globally.
    let mut s = itsy::settings::from_full_config(&config);
    if cli.auto_approve {
        s.auto_approve = true;
    }
    if let Some(v) = cli.max_tool_calls_per_turn {
        s.max_tool_calls_per_turn = v;
    }
    if let Some(v) = cli.max_output_tokens {
        s.max_output_tokens = v;
    }
    if let Some(v) = cli.thinking_budget {
        s.thinking_budget = v;
    }
    if let Some(v) = cli.request_timeout_ms {
        s.request_timeout_ms = v;
    }
    if let Some(v) = cli.bash_timeout {
        s.bash_timeout = v;
    }
    if let Some(v) = cli.tool_routing.clone() {
        s.tool_routing = v;
    }
    if let Some(v) = cli.allow_outside_paths {
        s.allow_outside_paths = v;
    }
    if let Some(v) = cli.web_browse {
        s.web_browse = v;
    }
    if let Some(v) = cli.shell_persist {
        s.shell_persist = v;
    }
    if let Some(v) = cli.profile.clone() {
        s.profile = Some(v);
    }
    if cli.verbose {
        s.verbose = true;
    }
    // Generic `--set key=value` overrides.
    for entry in &cli.set_overrides {
        let Some((k, v)) = entry.split_once('=') else {
            eprintln!("  ✗ --set expects `key=value`, got `{entry}`");
            std::process::exit(1);
        };
        if let Err(e) = s.apply_set_override(k.trim(), v.trim()) {
            eprintln!("  ✗ --set {entry}: {e}");
            std::process::exit(1);
        }
    }
    itsy::settings::init(s);
    (config, flags)
}

// ── Main dispatcher ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    itsy::interrupt::install();
    let cli = Cli::parse();

    // Non-interactive modes never run the wizard: CI, `-p "..."`, `--eval`,
    // `--mcp`, and `--print-system-prompt` all need to be scriptable.
    let non_interactive = cli.prompt.is_some()
        || cli.eval.is_some()
        || cli.mcp
        || cli.print_system_prompt
        || cli.non_interactive
        || !std::io::IsTerminal::is_terminal(&std::io::stdin());

    // First-launch / explicit `--init`: run the interactive wizard before
    // anything else. The wizard writes ~/.config/itsy/config.toml so the
    // rest of `main` can boot normally on subsequent runs.
    if cli.init || (itsy::init_wizard::is_first_launch() && !non_interactive) {
        match itsy::init_wizard::run() {
            Ok(_) => {
                if cli.init {
                    return Ok(());
                }
                // First-launch path: fall through into normal startup so the
                // user lands in the REPL right away.
                println!();
            }
            Err(e) => {
                eprintln!("  ✗ Setup failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let (mut config, flags) = build_config_and_settings(&cli);

    if cli.print_system_prompt {
        println!("{}", build_system_prompt(&config, "", "", "", None));
        return Ok(());
    }

    if let Some(suite) = cli.eval.as_deref() {
        let code = run_eval(&config, suite).await;
        std::process::exit(code);
    }

    let _reachable = check_endpoint(&mut config).await;

    // Auto-detect context window from llama-server /props endpoint.
    // Falls back to the configured value when the server doesn't expose /props.
    if let Some(n_ctx) = probe_context_window(&config.model.base_url).await {
        config.context.detected_window = n_ctx;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Restore the active contract (if any) into the in-memory cache so
    // tool calls in this session see the same state as the last one.
    itsy::session::contract::rehydrate(&cwd);

    // Native code graph indexes regardless of whether the npm MCP bridge is
    // installed. The MCP bridge is attempted as a bonus; its failure is
    // expected and not treated as an error.
    let mcp_bridge = Arc::new(McpBridge::new());
    let _ = mcp_bridge.start().await;
    let graph_ok = mcp_bridge.init_code_graph(env!("CARGO_PKG_VERSION")).await;

    // Welcome banner reflects the actual state of the native code graph.
    println!("{}", tui::render_welcome(graph_ok));

    let session = Arc::new(build_session(config, flags, cwd, mcp_bridge.clone()));

    if cli.resume {
        if let Some(record) = session.shared.write().sessions.resume() {
            session.mutable.lock().history.extend(record.messages.iter().cloned());
        }
    }

    // MCP server mode short-circuits everything else.
    if cli.mcp {
        let result = run_mcp(session.clone()).await;
        mcp_bridge.kill();
        return result;
    }

    // Non-interactive: single prompt + exit.
    let positional = cli.positional.join(" ");
    let positional = if positional.trim().is_empty() { None } else { Some(positional) };
    let prompt = cli.prompt.or(positional);
    if cli.non_interactive || prompt.is_some() {
        run_non_interactive(&session, prompt).await;
        mcp_bridge.kill();
        return Ok(());
    }

    // Default to the fullscreen ratatui REPL. Fall back to the classic
    // line-based REPL when the user passes `--classic` or stdin isn't a TTY
    // (piped input would otherwise break the alt-screen renderer).
    let use_fullscreen = !cli.classic && io::stdin().is_terminal();
    let res = if use_fullscreen {
        run_fullscreen_repl(session.clone()).await
    } else {
        run_repl(&session).await
    };
    mcp_bridge.kill();
    res
}
