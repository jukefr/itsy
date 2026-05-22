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
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

use itsy::commands::{handle_command, CommandCtx, CommandResult};
use itsy::config::{check_endpoint, load_config, load_dotenv, Config, Flags};
use itsy::cognition_adapter::classify_task_compiled;
use itsy::escalation::{EscalationEngine, EscalationOptions};
use itsy::eval_runner::{format_results, known_suite, EvalRunner};
use itsy::executor::{execute_tool, ExecCtx};
use itsy::features_adapter;
use itsy::governor::{
    classify_task, pick_decompose_strategy, ToolScorer, VerificationHistory,
    HardFailAction,
};
use itsy::governor::early_stop::EarlyStopDetector;
use itsy::knowledge::{get_knowledge_loader, SelectOptions};
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
use itsy::session::plan_tracker::{should_plan, PlanTracker};
use itsy::session::references::{format_references_for_prompt, resolve_references};
use itsy::session::tokens::TokenTracker;
use itsy::token_monitor::{CallMetadata, TokenMonitor};
use itsy::tools::{get_all_tools, ToolDeps};
use itsy::tools_impl::dedup::ToolDedup;
use itsy::tools_impl::test_runner;
use itsy::trace_recorder::TraceRecorder;
use itsy::tui;

// ── Constants ────────────────────────────────────────────────────────────────

fn max_tool_calls_per_turn() -> u32 {
    itsy::settings::get().max_tool_calls_per_turn
}
/// Maximum auto-improvement iterations per file before we DECOMPOSE.
const MAX_IMPROVE_ITERATIONS: u32 = 4;
/// Maximum size of any single tool result before we cap it (chars).
const MAX_TOOL_RESULT_CHARS: usize = 4000;

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
    /// Hard cap on tool calls per `run()`. Default 50.
    #[arg(long, value_name = "N")]
    max_tool_calls: Option<u32>,
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

    /// Positional prompt (anything not consumed by --prompt/-p)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    positional: Vec<String>,
}

// ── Agent session: shared per-process state ──────────────────────────────────

/// Bundle of state that lives across user turns within a single agent run.
struct AgentSession {
    config: Arc<Mutex<Config>>,
    history: Arc<Mutex<Vec<Value>>>,
    flags: Flags,
    memory: Arc<Mutex<MemoryStore>>,
    tokens: Arc<Mutex<TokenTracker>>,
    token_monitor: Arc<Mutex<TokenMonitor>>,
    escalation: Arc<Mutex<EscalationEngine>>,
    sessions: Arc<Mutex<SessionStore>>,
    plan_tracker: Arc<Mutex<PlanTracker>>,
    scorer: Arc<Mutex<ToolScorer>>,
    verification: Arc<Mutex<VerificationHistory>>,
    early_stop: Arc<Mutex<EarlyStopDetector>>,
    dedup: Arc<Mutex<ToolDedup>>,
    trace: Arc<Mutex<TraceRecorder>>,
    skills: Arc<Mutex<SkillManager>>,
    plugins: Arc<Mutex<PluginLoader>>,
    mcp_bridge: Arc<McpBridge>,
    cwd: PathBuf,
    /// Persists across turns so affirmation guards can keep the prior category.
    current_tool_category: Arc<Mutex<Option<String>>>,
    /// Active fullscreen renderer (Some when running ratatui REPL).
    fullscreen: Arc<Mutex<Option<Arc<itsy::fullscreen::Fullscreen>>>>,
}

// ── Helpers: estimation & history compaction ─────────────────────────────────

/// Cheap heuristic token estimator (~4 chars per token).
fn estimate_message_tokens(m: &Value) -> u64 {
    // Mirrors JS estimateMessageTokens — chars/4 ceil.
    let content_chars = match m.get("content") {
        Some(Value::String(s)) => s.len(),
        Some(other) if !other.is_null() => serde_json::to_string(other)
            .map(|s| s.len())
            .unwrap_or(0),
        _ => 0,
    };
    // tool_calls messages carry function.name + function.arguments;
    // upstream adds +20 per call as wire-overhead. Match exactly.
    let tc_chars = m
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| {
                    let name_len = tc
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    let args_len = tc
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    name_len + args_len + 20
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    ((content_chars + tc_chars) as f64 / 4.0).ceil() as u64
}

fn estimate_history_tokens(history: &[Value]) -> u64 {
    history.iter().map(estimate_message_tokens).sum()
}

/// Truncate a string to `max` chars while preserving a small tail. Used for
/// large tool results so the model still sees what failed at the end.
fn cap_tool_result(content: &str) -> String {
    if content.len() <= MAX_TOOL_RESULT_CHARS {
        return content.to_string();
    }
    // Snap both cuts to char boundaries — content may contain
    // multi-byte UTF-8 (e.g. the `│` line-number gutter used by
    // read_file output) and naive byte slicing panics.
    let mut head_end = MAX_TOOL_RESULT_CHARS - 200;
    while head_end > 0 && !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len().saturating_sub(200);
    while tail_start < content.len() && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    format!(
        "{head}\n\n...(truncated, {} chars total)...\n{tail}",
        content.len()
    )
}

fn truncate_short(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Detect a short affirmation like "yes" / "ok" / "go ahead". Keeps the prior
/// tool category instead of reclassifying — see JS lines 627-644.
fn is_affirmation(s: &str) -> bool {
    let trimmed = s.trim().trim_end_matches('.').to_lowercase();
    matches!(
        trimmed.as_str(),
        "yes"
            | "y"
            | "yep"
            | "yeah"
            | "sure"
            | "ok"
            | "okay"
            | "go"
            | "proceed"
            | "do it"
            | "continue"
            | "please"
            | "please do"
            | "alright"
    )
}

/// Detect quoted absolute paths or paths with a slash/extension. The
/// clarifier shouldn't fire on `'C:\\path\\foo.md'` even though it's short.
fn looks_like_path(s: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r#"[\\/]|\.\w{1,5}\s*$|^["'].*["']$"#).unwrap()
    });
    RE.is_match(s.trim())
}

/// Detect option-references like "option 2", "do 3", "first", "second".
/// The clarifier shouldn't fire on these — they're context-references to
/// a prior assistant message that proposed choices.
fn looks_like_option_ref(s: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r"(?i)^(option\s+\d|work\s+on\s+\d|do\s+\d|start\s+with\s+\d|\d+\.?\s*$|first|second|third|fourth)\b",
        )
        .unwrap()
    });
    RE.is_match(s.trim())
}

/// Auto-compact: trim oldest non-system messages once the budget is exceeded.
/// Mirrors JS lines 700-760 but without the LLM-based summary path.
fn maybe_compact(history: &mut Vec<Value>, config: &Config) -> bool {
    let estimated = estimate_history_tokens(history);
    let max_ctx_tokens =
        (config.context.detected_window as f64) * (config.context.max_budget_pct as f64 / 100.0);
    if (estimated as f64) <= max_ctx_tokens * 0.8 && history.len() <= 30 {
        return false;
    }
    let target = max_ctx_tokens * 0.7;
    let mut dropped = false;
    while history.len() > 6 {
        let est = estimate_history_tokens(history) as f64;
        if est < target {
            break;
        }
        let remove_idx = history
            .iter()
            .position(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));
        let Some(idx) = remove_idx else { break };
        history.remove(idx);
        dropped = true;
    }
    if dropped {
        let summary = format!(
            "[Context compacted to fit {} token budget]",
            max_ctx_tokens as u32
        );
        history.insert(0, json!({"role": "system", "content": summary}));
    }
    dropped
}

/// Mid-turn eviction: when in the middle of a tool chain and history blows up,
/// truncate large arguments in old assistant messages and replace tool results
/// with stubs. JS lines 786-863.
fn mid_turn_evict(history: &mut Vec<Value>, config: &Config) -> u32 {
    let max_budget = (config.context.detected_window as f64) * 0.6;
    if (estimate_history_tokens(history) as f64) <= max_budget {
        return 0;
    }
    // Find last assistant index with tool_calls — we won't touch that one.
    let last_assistant_idx = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("tool_calls").is_some())
        .map(|(i, _)| i)
        .last()
        .unwrap_or(0);
    // First pass: truncate huge args in older assistant tool_calls.
    for m in history.iter_mut().take(last_assistant_idx) {
        let Some(calls) = m.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for tc in calls.iter_mut() {
            let Some(args) = tc.pointer_mut("/function/arguments") else { continue };
            let Some(s) = args.as_str() else { continue };
            if s.len() <= 200 {
                continue;
            }
            // Minimize all string fields > 100 chars.
            let minimal = serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| {
                    let obj = v.as_object()?;
                    let mut out = serde_json::Map::new();
                    for (k, v) in obj.iter() {
                        match v {
                            Value::String(s) if s.len() > 100 => {
                                out.insert(
                                    k.clone(),
                                    Value::String(format!("{}…", &s[..80.min(s.len())])),
                                );
                            }
                            other => {
                                out.insert(k.clone(), other.clone());
                            }
                        }
                    }
                    Some(Value::Object(out).to_string())
                })
                .unwrap_or_else(|| "{}".into());
            *args = Value::String(minimal);
        }
    }
    // Second pass: evict tool results in the first half.
    let half = history.len() / 2;
    let mut evicted = 0u32;
    let mut i = 0;
    while i < half && i < history.len() {
        let role = history[i].get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" {
            let content = history[i]
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let approx = (content.len() / 4) as u64;
            history[i]["content"] = json!(format!("[evicted: {approx} tokens]"));
            evicted += 1;
        }
        i += 1;
        if (estimate_history_tokens(history) as f64) <= max_budget * 0.7 {
            break;
        }
    }
    evicted
}

// ── System prompt builders ───────────────────────────────────────────────────

/// JS `getMemoryContext`. Loads scored memory for the last user message and
/// formats it inline (≤ ~800 tokens / 3200 chars).
fn get_memory_context(messages: &[Value], memory: &MemoryStore) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(task) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let items = memory.load_for_task(task);
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\nRelevant project memory:\n");
    let max_chars = 3200usize;
    for o in items {
        let entry = format!("[{}] {}: {}\n", o.kind, o.title, o.content);
        if out.len() + entry.len() > max_chars {
            break;
        }
        out.push_str(&entry);
    }
    out
}

/// JS `getSkillContext`. Auto-loads matching skills based on the last user
/// message and formats them (capped at ~4000 chars).
fn get_skill_context(messages: &[Value], skills: &SkillManager) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(msg) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let auto = skills.get_auto_skills(msg);
    if auto.is_empty() {
        return String::new();
    }
    let formatted = skills.format_for_prompt(&auto);
    if formatted.len() > 4000 {
        format!(
            "{}\n... (skills truncated to fit context)",
            &formatted[..4000]
        )
    } else {
        formatted
    }
}

/// JS `getPluginPrompts`. Plugin-supplied prompt injections gated by task type.
fn get_plugin_prompts(plugins: &PluginLoader, task_type: Option<&str>) -> String {
    let injection = plugins.get_prompt_injections(task_type);
    if injection.is_empty() {
        return String::new();
    }
    let capped = if injection.len() > 2000 {
        format!("{}\n... (plugin prompts truncated)", &injection[..2000])
    } else {
        injection
    };
    format!("\n\n{capped}")
}

/// JS `getKnowledgeContext`. Walks the project's `knowledge/` directory and
/// pulls in docs that overlap with the last user message.
fn get_knowledge_context(messages: &[Value], config: &Config) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(query) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let max_tokens = (config.context.detected_window as f64 * 0.04)
        .clamp(200.0, 1500.0) as usize;
    let loader = get_knowledge_loader();
    loader.format_for_prompt(query, &SelectOptions { max_tokens: Some(max_tokens) })
}

/// JS `getActivePlanContext`.
fn get_active_plan_context(plan: &PlanTracker) -> String {
    plan.format_for_prompt()
}

/// JS `getTestRunnerContext`.
fn get_test_runner_context(cwd: &Path) -> String {
    test_runner::format_for_prompt(cwd)
}

/// JS `buildCompactSystemPrompt` — assemble the per-call system prompt. Static
/// sections come from `build_system_prompt`; we layer the dynamic bits on top.
fn build_full_system_prompt(
    config: &Config,
    task_type: &str,
    messages: &[Value],
    session: &AgentSession,
) -> String {
    // Contract-mode short-circuit: when the contract feature is on AND
    // the current turn is action-y, the contract is the entire job for
    // this turn. Return a focused contract-shaped system prompt
    // instead of the generic kitchen-sink one. We learned the hard way
    // that a 1.5k-char generic prompt with a paragraph about
    // `propose_contract` buried in it loses every time — the model's
    // reasoning layer notices the requirement but the tool-call
    // decoder picks an easier neighbour. The fix is to make the
    // contract the WHOLE prompt.
    if itsy::settings::get().contract
        && !matches!(task_type, "explanation" | "respond")
    {
        let active = itsy::session::contract::current();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(c) = active {
            return build_contract_active_prompt(&c, &cwd, config);
        }
        return build_contract_proposal_prompt(&cwd, config);
    }

    let memory = session.memory.lock();
    let mem_ctx = get_memory_context(messages, &memory);
    drop(memory);

    let skills = session.skills.lock();
    let skill_ctx = get_skill_context(messages, &skills);
    drop(skills);

    let plugins = session.plugins.lock();
    let plugin_ctx = get_plugin_prompts(&plugins, Some(task_type));
    drop(plugins);

    let mut prompt = build_system_prompt(
        config,
        &mem_ctx,
        &skill_ctx,
        &plugin_ctx,
        Some(task_type),
    );

    // Knowledge auto-injection.
    let know = get_knowledge_context(messages, config);
    if !know.is_empty() {
        prompt.push_str(&know);
    }

    // Code-graph hits for long user messages — gated on
    // `features.context_retrieval`. Uses the local code graph (no LLM).
    if config.features.context_retrieval {
        if let Some(last_user) = messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        {
            if last_user.len() > 200 {
                if let Some(graph) = itsy::code_graph::try_get_code_graph() {
                    if let Ok(hits) = graph.search_graph(last_user, 1500) {
                        if !hits.is_empty() {
                            prompt.push_str("\n\nRelevant code from the project:\n");
                            for h in hits.iter().take(5) {
                                prompt.push_str(&format!(
                                    "- {} ({} at {}:{})\n",
                                    h.name, h.kind, h.file, h.line
                                ));
                                if let Some(sig) = &h.signature {
                                    prompt.push_str(&format!("    {}\n", sig));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Active plan re-anchor.
    let plan = session.plan_tracker.lock();
    let plan_ctx = get_active_plan_context(&plan);
    drop(plan);
    if !plan_ctx.is_empty() {
        prompt.push_str(&plan_ctx);
    }

    // Test runner hint.
    let tr = get_test_runner_context(&session.cwd);
    if !tr.is_empty() {
        prompt.push_str(&tr);
    }

    // (Contract-mode prompts are handled at the top of this function
    // as a complete short-circuit — when there's an active contract or
    // we need to propose one, the rest of the layering is skipped.)

    prompt
}

/// Contract-mode prompt for turn 1 (no contract yet). Short, focused,
/// laser-targeted: the model's only job right now is to call
/// `propose_contract`. The kitchen-sink prompt is intentionally
/// absent here — small models latch onto whatever's loudest, and a
/// 1.5k-char "you're a coding assistant who can also propose
/// contracts" loses to a 300-char "you must propose a contract."
fn build_contract_proposal_prompt(cwd: &str, config: &Config) -> String {
    let model_line = if config.model.name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {}", config.model.name)
    };
    format!(
        "You are itsy, working in CONTRACT mode. Working directory: {cwd}.{model_line}\n\
        \n\
        YOUR FIRST ACTION MUST BE `propose_contract`. No exceptions, no exploration first.\n\
        \n\
        A contract is the definition of done for the user's task. It is 2–6 short, testable assertions \
        — each one a single thing you can later prove with a shell command:\n\
        \n\
          GOOD:  \"the file /app/regex.txt exists\"\n\
          GOOD:  \"running `python3 /tmp/check.py` exits 0\"\n\
          GOOD:  \"`pytest /tests/test_outputs.py -q` reports 3 passed\"\n\
          BAD:   \"the code is correct\"          (not testable)\n\
          BAD:   \"the implementation is complete\" (not testable)\n\
          BAD:   \"all tests pass\"              (vague — which tests?)\n\
        \n\
        Until propose_contract returns, NO other tools are available. \
        `write_file`, `patch`, mutating `bash`, etc. will refuse. \
        Read-only tools (read_file, search) are available but you should not need them — \
        you're not exploring, you're stating what 'done' means.\n\
        \n\
        Skip the planning preamble. Skip the analysis. Emit `propose_contract` now with:\n\
        - title:       short human title for the task\n\
        - brief:       1–2 sentences describing the work\n\
        - assertions:  array of {{id, text}} — pick 2–6\n\
        \n\
        After it returns the toolkit opens up and you can do the work.\n",
        cwd = cwd,
        model_line = model_line,
    )
}

/// Contract-mode prompt for turn 2+ (contract is active). The
/// assertions and their current states ARE the prompt — the model
/// works through them one by one. We don't include the generic
/// instructions/code-graph hints — the contract is enough.
fn build_contract_active_prompt(
    c: &itsy::session::contract::Contract,
    cwd: &str,
    config: &Config,
) -> String {
    let model_line = if config.model.name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {}", config.model.name)
    };
    let body = itsy::session::contract::render_for_prompt(c);
    format!(
        "You are itsy, working under an active contract. Working directory: {cwd}.{model_line}\n\
        \n\
        {body}\n\
        \n\
        How to work:\n\
        - Do the work for each pending assertion (write_file / patch / bash — all available now).\n\
        - When you've verified an assertion, call `mark_assertion` with:\n\
            id          the assertion id (A.001, A.002, …)\n\
            state       \"passed\" / \"failed\" / \"skipped\"\n\
            evidence    one-sentence summary of how you verified\n\
            command     (recommended for passed) the shell command you ran\n\
            exit_code   the exit code\n\
            observation the actual output you saw — NOT \"OK\" or \"passed\"\n\
        - When every assertion is non-pending, call `close_contract completed` to finish.\n\
        - `close_contract completed` is refused while any assertion is still pending — \
          there's no way to claim 'done' without resolving each one.\n\
        - If you genuinely cannot verify an assertion, mark it `failed` or `skipped` with an honest reason.\n",
        cwd = cwd,
        model_line = model_line,
        body = body,
    )
}

// ── Agent turn ───────────────────────────────────────────────────────────────

/// Run one agent loop turn for the given user message. Mirrors the JS
/// `runAgentLoop` function (~1100 lines).
async fn handle_turn(prompt_in: &str, session: &AgentSession) {
    // Reset early-stop bookkeeping for a fresh turn.
    session.early_stop.lock().new_turn();

    // Trace recording: start a fresh trace for this turn.
    {
        let model_name = session.config.lock().model.name.clone();
        session.trace.lock().start(prompt_in, &model_name);
    }
    session.token_monitor.lock().mark_next_call_new_turn();

    let user_msg = prompt_in.to_string();

    // 1) Clarifier check — only fires on short messages (< 80 chars) that
    // are *not* obviously actionable (paths, option-refs, affirmations).
    // Ports the a85c90c fix: instruction is spliced out after the model
    // responds so it doesn't linger across turns.
    // Ports the assistantAskedQuestion bypass: if the model's last turn ended
    // with a question mark, the user's reply is an answer, not a new vague task.
    let clarifier_enabled = session.config.lock().features.clarifier;
    let assistant_asked_question = {
        let hist = session.history.lock();
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
        && !looks_like_path(&user_msg)
        && !looks_like_option_ref(&user_msg)
        && !is_affirmation(&user_msg)
    {
        let needs = features_adapter::check_needs_clarification(&user_msg).await;
        if needs {
            let clarifier_idx = {
                let mut hist = session.history.lock();
                hist.push(json!({"role": "user", "content": user_msg.clone()}));
                let idx = hist.len();
                hist.push(json!({
                    "role": "system",
                    "content": itsy::session::clarify::get_clarification_instruction(),
                }));
                idx
            };
            let snapshot = (session.history.lock().clone(), session.config.lock().clone());
            let chat_ctx = ChatContext {
                config: &snapshot.1,
                conversation: &snapshot.0,
                tools: Vec::new(),
                current_task_type: None,
                system_prompt: build_full_system_prompt(&snapshot.1, "explanation", &snapshot.0, session),
            };
            let response = chat_completion(&chat_ctx).await;
            // Splice the one-shot clarifier instruction out of history,
            // whether or not the model responded — otherwise it sticks
            // around and re-fires on every subsequent turn.
            {
                let mut hist = session.history.lock();
                if clarifier_idx < hist.len() {
                    let is_clarifier = hist[clarifier_idx]
                        .get("role")
                        .and_then(|r| r.as_str())
                        == Some("system")
                        && hist[clarifier_idx]
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains("vague"))
                            .unwrap_or(false);
                    if is_clarifier {
                        hist.remove(clarifier_idx);
                    }
                }
            }
            if let Some(data) = response {
                record_usage(&data, session, false);
                if let Some(msg) = data.pointer("/choices/0/message") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        session
                            .history
                            .lock()
                            .push(json!({"role": "assistant", "content": content}));
                        println!("{}", tui::render_markdown(content));
                    }
                }
            }
            // Wait for the user to clarify on the next REPL iteration.
            return;
        }
    }

    // 1.5) Optional planner→executor chain. Runs a separate LLM call
    // up front to break the task into steps, then injects the plan as a
    // system message before the main agent loop. Off by default
    // (`features.chain`); when on, only fires once per turn at the top.
    let planner_injection: Option<String> = if session.config.lock().features.chain {
        let cfg = session.config.lock().clone();
        let plan = itsy::model::chain::call_planner(&user_msg, &cfg).await;
        let injection = itsy::model::chain::format_planner_injection(plan.as_deref());
        if injection.is_empty() {
            None
        } else {
            session.history.lock().push(json!({
                "role": "system",
                "content": injection.clone(),
            }));
            Some(injection)
        }
    } else {
        None
    };
    let _ = planner_injection;

    // 2) Resolve `@file` references + auto-inject git diff when applicable.
    let refs = resolve_references(&user_msg, &session.cwd);
    let mut augmented = if refs.is_empty() {
        user_msg.clone()
    } else {
        format!("{}{}", user_msg, format_references_for_prompt(&refs))
    };
    if should_inject_git_context(&user_msg) {
        if let Some(ctx) = get_git_diff_context(&session.cwd) {
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
    // We wrap unconditionally (don't bother with task-type classification
    // here — the contract-mode prompt + downstream gate already skip
    // the explanation case). If the prompt is genuinely chat-y the
    // model will just emit a quick 2-3 assertion contract and we move on.
    if itsy::settings::get().contract && itsy::session::contract::current().is_none() {
        augmented = format!(
            "Define what 'done' means for the following task by calling \
             propose_contract. Do not start the task yet — just enumerate \
             the checks that would prove it's done.\n\n\
             TASK:\n{augmented}"
        );
    }

    session
        .history
        .lock()
        .push(json!({"role": "user", "content": augmented.clone()}));

    // 3) Plan-then-execute: ask the model for a numbered plan up front and
    // remove the one-shot instruction once the plan is captured.
    let mut plan_instruction_idx: i32 = -1;
    {
        let mut plan = session.plan_tracker.lock();
        plan.reset();
        if should_plan(&user_msg) {
            plan.activate();
            drop(plan);
            let mut hist = session.history.lock();
            plan_instruction_idx = hist.len() as i32;
            hist.push(json!({
                "role": "system",
                "content": PlanTracker::plan_request_instruction(),
            }));
        }
    }

    // 4) Task classification (regex now; compiled LLM-based when available).
    //    `classify_task` defaults to "coding" for anything it doesn't match,
    //    which is wrong for conversational messages. We override below if
    //    the tool router also routes the message to `respond`.
    let task_type_initial = classify_task_compiled(&user_msg, classify_task).await;

    // 5) Tool routing: classify intent → filter (or strip) tools. The
    //    `respond` category strips tools entirely so the model can't be
    //    tempted to write a file on a conversational message like
    //    "say hello". This applies to BOTH direct and two-stage routing.
    let stage2_category = {
        let mut current = session.current_tool_category.lock();
        // Affirmation guard — keep the prior turn's tool set so "yes"/"ok"
        // after a proposed action lets the model proceed.
        if is_affirmation(&user_msg)
            && current.as_deref().is_some()
            && current.as_deref() != Some("respond")
        {
            current.clone()
        } else if is_affirmation(&user_msg) {
            *current = Some("plan".into());
            Some("plan".into())
        } else {
            // Run the deterministic classifier. If it picks `respond`
            // with positive confidence, pin to that — the model gets
            // zero tools and must reply with plain text.
            let classification = itsy::runtime::tool_router::classify_tool_category(&user_msg);
            if classification.category == "respond" && classification.confidence > 0.0 {
                *current = Some("respond".into());
                Some("respond".into())
            } else {
                *current = None;
                None
            }
        }
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

    // Record the routing decision so failures can be replayed with
    // full context. We rerun the deterministic router here purely to
    // capture the confidence score for the trace.
    let cls_for_log = itsy::runtime::tool_router::classify_tool_category(&user_msg);
    session.trace.lock().record_classification(
        task_type,
        stage2_category.as_deref(),
        cls_for_log.confidence,
    );

    // 6) Maybe pre-compact the history before the first call.
    {
        let mut hist = session.history.lock();
        let cfg = session.config.lock().clone();
        if maybe_compact(&mut hist, &cfg) {
            println!("{}", tui::compacted(hist.len() as u32));
        }
    }

    let mut tool_calls_this_turn: u32 = 0;
    let max_tool_calls_this_turn = max_tool_calls_per_turn();
    let mut edited_files: Vec<String> = Vec::new();
    let mut improvement_attempts: std::collections::HashMap<String, u32> = Default::default();
    // Per-turn "you've already called this" counter, keyed by a hash of
    // (tool_name, args). Catches identical mutating calls (like `rm -rf X`
    // 4 times in a row) that the regular dedup doesn't because the tool
    // is impure.
    // `per_turn_repeats` is also used by other counters (empty-response
    // retry, contract-gate refusals, contract-loop nudges, …). The
    // identical-call repeat counter that used to live here was removed —
    // see the "Spiral defense is upstream's" comment in the loop body.
    let mut per_turn_repeats: std::collections::HashMap<String, u32> = Default::default();
    let mut current_category: Option<String> = stage2_category;

    // Reset any pending Ctrl+C presses from earlier turns; the user starts
    // each turn with a clean slate.
    itsy::interrupt::reset();

    // 7) Main while-loop.
    loop {
        if tool_calls_this_turn >= max_tool_calls_this_turn {
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
        if tool_calls_this_turn > 0 && tool_calls_this_turn % 3 == 0 {
            let cfg = session.config.lock().clone();
            let mut hist = session.history.lock();
            let evicted = mid_turn_evict(&mut hist, &cfg);
            if evicted > 0 {
                session.token_monitor.lock().record_eviction();
            }
        }

        // Build the request snapshot.
        let (cfg, hist, tools) = {
            let cfg = session.config.lock().clone();
            let hist = session.history.lock().clone();
            let deps = {
                let plugins = session.plugins.lock();
                ToolDeps {
                    plugin_tools: plugins.get_tools(),
                    mcp_tools: Vec::new(),
                }
            };
            let mut tools = get_all_tools(&cfg, current_category.as_deref(), &deps);
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

        let system_prompt = build_full_system_prompt(&cfg, task_type, &hist, session);
        let chat_ctx = ChatContext {
            config: &cfg,
            conversation: &hist,
            tools,
            current_task_type: Some(task_type),
            system_prompt,
        };

        // Forensic snapshot of the request body about to be sent. Lets a
        // smarter model later replay the call from the trace JSON.
        session.trace.lock().record_chat_request(
            &cfg.model.name,
            &chat_ctx.system_prompt,
            &json!(chat_ctx.conversation),
            &json!(chat_ctx.tools),
        );

        let Some(data) = chat_completion(&chat_ctx).await else {
            session
                .trace
                .lock()
                .record_error("chat_completion", "no response from model");
            println!("  \x1b[31m✗ No response from model\x1b[0m");
            break;
        };
        record_usage(&data, session, false);

        let Some(msg) = data.pointer("/choices/0/message").cloned() else {
            break;
        };
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Record trace step.
        {
            let content = msg.get("content").and_then(|c| c.as_str());
            session
                .trace
                .lock()
                .record_model_response(content, Some(&tool_calls));
        }

        // 7a) Model emitted tool calls → execute them.
        if !tool_calls.is_empty() {
            // Widen the tool set on subsequent calls unless the model picked a
            // category via select_category (in which case it'll set it below).
            let first_tool_name = tool_calls
                .first()
                .and_then(|tc| tc.pointer("/function/name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if first_tool_name != "select_category" {
                current_category = Some("plan".into());
                *session.current_tool_category.lock() = Some("plan".into());
            }

            // Push the assistant message verbatim.
            session.history.lock().push(msg.clone());

            // Plan extraction from textual content. Try the LLM extractor
            // first when the plan toggle is on; fall back to the regex
            // parser inside ingest_response.
            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                let needs = { session.plan_tracker.lock().needs_plan() };
                let llm_steps = if needs && session.config.lock().features.plan {
                    features_adapter::extract_plan_steps(content).await
                } else {
                    None
                };
                let mut plan = session.plan_tracker.lock();
                let ingested = if let Some(steps) = llm_steps {
                    plan.set_plan(steps);
                    true
                } else {
                    plan.needs_plan() && plan.ingest_response(content)
                };
                if ingested {
                    drop(plan);
                    // Strip the one-shot plan request instruction we injected.
                    if plan_instruction_idx >= 0 {
                        let mut hist = session.history.lock();
                        let idx = plan_instruction_idx as usize;
                        if idx < hist.len() {
                            if hist[idx]
                                .get("content")
                                .and_then(|c| c.as_str())
                                .map(|s| s.contains("numbered plan"))
                                .unwrap_or(false)
                            {
                                hist.remove(idx);
                            }
                        }
                        plan_instruction_idx = -1;
                    }
                }
            }

            for tc in &tool_calls {
                tool_calls_this_turn += 1;
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

                // Contract-first gate: when the contract feature is on
                // and the model is on an action-y task without an
                // active contract yet, refuse the first mutating tool
                // calls until it commits to a contract. Read-only
                // exploration and the contract tools themselves stay
                // free. Bounded by `MAX_CONTRACT_GATE_REFUSALS` so a
                // model that genuinely can't produce a contract
                // doesn't get stuck forever.
                const MAX_CONTRACT_GATE_REFUSALS: u32 = 3;
                let is_contract_tool = matches!(
                    name.as_str(),
                    "propose_contract"
                        | "mark_assertion"
                        | "mark_feature"
                        | "contract_status"
                        | "close_contract"
                );
                // Gate fires for any task type EXCEPT pure-explanation —
                // those don't produce mutating side effects anyway.
                // (Earlier we matched only a whitelist of action tasks
                // and lost coverage on `search` / `shell` task types
                // where the model still git-merge'd / git-commit'd.)
                let task_is_action = !matches!(task_type, "explanation" | "respond");
                let is_mutating = match name.as_str() {
                    "write_file" | "append_file" | "patch" | "read_and_patch"
                    | "create_and_run" | "run" | "memory_remember" | "memory_forget" => true,
                    "bash" => !itsy::tools_impl::dedup::bash_is_read_only(&args),
                    _ => false,
                };
                if itsy::settings::get().contract
                    && task_is_action
                    && itsy::session::contract::current().is_none()
                    && !is_contract_tool
                    && is_mutating
                {
                    let refused = per_turn_repeats
                        .entry("__contract_gate".into())
                        .and_modify(|n| *n += 1)
                        .or_insert(1);
                    if *refused <= MAX_CONTRACT_GATE_REFUSALS {
                        println!(
                            "  \x1b[33m⚠ blocked `{name}` — define what 'done' means first ({}/{})\x1b[0m",
                            *refused, MAX_CONTRACT_GATE_REFUSALS
                        );
                        session.history.lock().push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": "[BLOCKED] No contract yet. Before any mutating action, call `propose_contract` \
                                with: (1) a short title, (2) a 2-3 sentence brief, (3) 2-6 assertions you'll verify \
                                (each one-line, testable, ≤120 chars; e.g. \"Exit code is 0 when running pytest\", \
                                \"about.md byte-for-byte matches the lost commit's version\"). Read-only tools \
                                (read_file, search, find_files, git status/log/show, …) are still allowed."
                        }));
                        continue;
                    }
                    // Exhausted the gate budget — let the call through
                    // so the model isn't permanently blocked.
                    session.trace.lock().record_error(
                        "contract_gate_exhausted",
                        "model never proposed a contract; letting tools through",
                    );
                }

                // Spiral defense is upstream's: dedup catches pure-tool
                // repeats, improvement_attempts catches failed mutating
                // calls (bash/patch), the per-turn tool-call cap bounds
                // the worst case. itsy had a Rust-only BREAK_ON_REPEAT
                // counter here that aborted after 5 identical calls and
                // then RESET to 0 — turning into a 5-emit / 1-abort
                // cycle that never escapes. Removed: no bench evidence
                // it helped, the reset bug made the spiral worse than
                // upstream's bounded "spam until tool-call cap" path.

                // Dedup lookup. Mirrors upstream:
                //   let cached = dedup.lookup(name, args);
                //   if (cached) result = cached;
                //   else { result = await execute(...); dedup.record(...); }
                let cached = session.dedup.lock().lookup(&name, &args);
                let started = Instant::now();
                let fs_handle = session.fullscreen.lock().clone();
                if let Some(fs) = &fs_handle {
                    fs.add_tool(&name, "running", "");
                } else {
                    println!("{}", tui::tool_start(&name));
                }

                let result = match cached {
                    Some(c) => itsy::tools_impl::dedup::mark_cached(c),
                    None => {
                        let ctx = ExecCtx {
                            config: &session.config.lock().clone(),
                            flags: &session.flags,
                            memory: session.memory.clone(),
                            mcp_bridge: Some(session.mcp_bridge.clone()),
                            mcp_client: None,
                            fullscreen: fs_handle.clone(),
                        };
                        let r = execute_tool(&name, args.clone(), &ctx).await;
                        session.dedup.lock().record(&name, &args, &r);
                        r
                    }
                };

                let elapsed_ms = started.elapsed().as_millis() as u64;

                // select_category: update the active stage-2 category.
                if name == "select_category" {
                    if let Some(cat) = result.get("category").and_then(|v| v.as_str()) {
                        current_category = Some(cat.to_string());
                        *session.current_tool_category.lock() = Some(cat.to_string());
                    }
                }

                // Trace step.
                session.trace.lock().record_tool_call(
                    &name,
                    &args,
                    &result,
                    elapsed_ms,
                );

                // Track edited files.
                if (name == "write_file" || name == "patch")
                    && result.get("error").is_none()
                {
                    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                        edited_files.push(p.to_string());
                    }
                }

                // Pretty-print outcome.
                print_tool_result(&name, &result, elapsed_ms, session.flags.verbose);

                // Record success/failure for tool scoring.
                {
                    let mut s = session.scorer.lock();
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

                // Early-stop on patch spirals.
                if name == "patch" || name == "read_and_patch" {
                    let patch_file = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
                    let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
                    let patch_success = result.get("error").is_none();
                    if let Some(signal) = session.early_stop.lock().record_patch_result(
                        &patch_file,
                        patch_success,
                        old_str,
                        new_str,
                    ) {
                        println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                        session
                            .history
                            .lock()
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
                let capped = cap_tool_result(&tool_content);
                session.history.lock().push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": capped,
                }));

                // ── Improvement loop for file writes/patches ──────────────
                if (name == "write_file" || name == "patch")
                    && result.get("error").is_none()
                {
                    let Some(file_path) = args.get("path").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let file_path = file_path.to_string();

                    // Verification governor: routes to Accept / Retry / Decompose.
                    let action = session
                        .verification
                        .lock()
                        .check_and_enforce(&file_path);
                    match action {
                        HardFailAction::Accept { .. } => {
                            if improvement_attempts
                                .get(&file_path)
                                .copied()
                                .unwrap_or(0)
                                > 0
                            {
                                println!(
                                    "{}",
                                    tui::improvement_fixed(
                                        &file_path,
                                        improvement_attempts[&file_path]
                                    )
                                );
                                improvement_attempts.insert(file_path.clone(), 0);
                            }
                            // LLM self-critique: ask "does this still do
                            // what the user wanted?" Cheap to wire, costs
                            // one LLM call per accepted edit when enabled.
                            if session.config.lock().features.validate_edits {
                                if let Ok(content) = std::fs::read_to_string(&file_path) {
                                    let result = features_adapter::validate_edit_compiled(
                                        &file_path,
                                        &content,
                                        &user_msg,
                                    )
                                    .await;
                                    if !result.ok && !result.issues.is_empty() {
                                        let issues = result.issues.join("\n  - ");
                                        let msg = format!(
                                            "[SEMANTIC-REVIEW] The edit to {file_path} may need follow-up:\n  - {issues}"
                                        );
                                        session.history.lock().push(json!({
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
                            improvement_attempts
                                .entry(file_path.clone())
                                .and_modify(|n| *n += 1)
                                .or_insert(1);
                            let attempt_n = improvement_attempts[&file_path];
                            println!(
                                "{}",
                                tui::improvement_loop(
                                    &errors,
                                    attempt_n,
                                    MAX_IMPROVE_ITERATIONS
                                )
                            );
                            session.token_monitor.lock().record_compaction();
                            let test_hint = if !get_test_runner_context(&session.cwd).is_empty()
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
                            session.history.lock().push(json!({
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
                            let strat = features_adapter::decompose_task(
                                &user_msg,
                                &errors.join("\n"),
                                &file_content.chars().take(1000).collect::<String>(),
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
                            // Mirrors upstream __decompose:${filePath} counter:
                            // only escalate to the stronger model on the 2nd decompose
                            // attempt for this file. The first attempt uses the
                            // local model with a new strategy.
                            let decompose_key = format!("__decompose:{file_path}");
                            let decompose_n = improvement_attempts
                                .entry(decompose_key.clone())
                                .and_modify(|n| *n += 1)
                                .or_insert(1);
                            let should_escalate = *decompose_n >= 2
                                && session.escalation.lock().can_escalate();
                            if should_escalate {
                                println!(
                                    "  \x1b[35m⬆ Escalating to {} (decompose attempt {})\x1b[0m",
                                    session.escalation.lock().status(),
                                    decompose_n,
                                );
                                improvement_attempts.insert(file_path.clone(), 0);
                                improvement_attempts.insert(decompose_key, 0);
                                let recent = {
                                    let h = session.history.lock();
                                    h.iter()
                                        .rev()
                                        .take(6)
                                        .cloned()
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .rev()
                                        .collect::<Vec<_>>()
                                };
                                let prompt = format!(
                                    "Fix these errors in {file_path}. The code:\n```\n{}\n```\n\nErrors:\n{}\n\nPrevious attempts failed. Fix it correctly.",
                                    file_content.chars().take(12000).collect::<String>(),
                                    errors.join("\n"),
                                );
                                let mut messages: Vec<Value> = recent;
                                messages.push(json!({"role": "user", "content": prompt}));
                                let tools_snapshot = {
                                    let cfg = session.config.lock();
                                    let plugins = session.plugins.lock();
                                    get_all_tools(
                                        &cfg,
                                        None,
                                        &ToolDeps {
                                            plugin_tools: plugins.get_tools(),
                                            mcp_tools: Vec::new(),
                                        },
                                    )
                                };
                                let mut esc = session.escalation.lock();
                                match esc.escalate(messages, tools_snapshot, "").await {
                                    Ok(Some(resp)) if resp.get("error").is_none() => {
                                        session.history.lock().push(resp);
                                    }
                                    _ => {
                                        eprintln!("  \x1b[31m✗ Escalation failed\x1b[0m");
                                        session.history.lock().push(json!({
                                            "role": "user",
                                            "content": "[ESCALATION FAILED] Even the stronger model couldn't fix this. Deliver the best version you have and explain what's still broken.",
                                        }));
                                    }
                                }
                            } else {
                                // First decompose for this file — try local model with
                                // a new strategy. Matches JS: only reset the file counter,
                                // not the decompose counter.
                                improvement_attempts.insert(file_path.clone(), 0);
                            }
                            session.history.lock().push(json!({
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
                        session.trace.lock().record_validation(
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
                    let counter = improvement_attempts
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
                        session.history.lock().push(json!({
                            "role": "user",
                            "content": format!("[AUTO-FIX] The command FAILED (attempt {attempt_n}/2). Do NOT claim success. The error was:\n{err}\n\nRead the error, identify the bug, and fix it."),
                        }));
                    } else {
                        // Mirrors upstream __decompose:bash counter:
                        // only escalate on the 2nd decompose attempt.
                        let decompose_bash_n = improvement_attempts
                            .entry("__decompose:bash".into())
                            .and_modify(|n| *n += 1)
                            .or_insert(1);
                        let should_escalate_bash = *decompose_bash_n >= 2
                            && session.escalation.lock().can_escalate();
                        if should_escalate_bash {
                            println!(
                                "  \x1b[35m⬆ Escalating to {} (bash decompose attempt {})\x1b[0m",
                                session.escalation.lock().status(),
                                decompose_bash_n,
                            );
                            improvement_attempts.insert("__bash".into(), 0);
                            improvement_attempts.insert("__decompose:bash".into(), 0);
                            let recent = {
                                let h = session.history.lock();
                                h.iter()
                                    .rev()
                                    .take(8)
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect::<Vec<_>>()
                            };
                            let bash_err = result
                                .get("result")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .chars()
                                .take(1500)
                                .collect::<String>();
                            let mut messages: Vec<Value> = recent;
                            messages.push(json!({
                                "role": "user",
                                "content": format!("The command keeps failing. Fix the underlying issue. Error: {bash_err}"),
                            }));
                            let tools_snapshot = {
                                let cfg = session.config.lock();
                                let plugins = session.plugins.lock();
                                get_all_tools(
                                    &cfg,
                                    None,
                                    &ToolDeps {
                                        plugin_tools: plugins.get_tools(),
                                        mcp_tools: Vec::new(),
                                    },
                                )
                            };
                            let mut esc = session.escalation.lock();
                            match esc.escalate(messages, tools_snapshot, "").await {
                                Ok(Some(resp)) if resp.get("error").is_none() => {
                                    session.history.lock().push(resp);
                                }
                                _ => {
                                    eprintln!("  \x1b[31m✗ Escalation failed\x1b[0m");
                                    session.history.lock().push(json!({
                                        "role": "user",
                                        "content": "[ESCALATION FAILED] Move on. Explain what you tried and what's still broken.",
                                    }));
                                }
                            }
                        } else {
                            // First bash decompose — try local model with new strategy.
                            let strategy = features_adapter::decompose_task(
                                &user_msg,
                                result
                                    .get("result")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(""),
                                args.get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(""),
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
                            session.history.lock().push(json!({
                                "role": "user",
                                "content": format!("[DECOMPOSE] The command has failed 3 times. STOP retrying the same approach.\n\n{}", strategy.2),
                            }));
                            improvement_attempts.insert("__bash".into(), 0);
                        }
                    }
                } else if (name == "bash" || name == "run")
                    && result.get("error").is_none()
                {
                    improvement_attempts.insert("__bash".into(), 0);
                }
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
        if tool_calls_this_turn == 0
            && current_category.as_deref() != Some("respond")
            && content_trimmed_len == 0
        {
            const MAX_EMPTY_RETRIES: u32 = 2;
            let n = *per_turn_repeats.entry("__empty_response".into()).and_modify(|n| *n += 1).or_insert(1);
            if n <= MAX_EMPTY_RETRIES {
                println!("  \x1b[33m⚠ Model returned empty response — retrying ({n}/{MAX_EMPTY_RETRIES})\x1b[0m");
                session.history.lock().push(json!({
                    "role": "assistant",
                    "content": content_opt.clone().unwrap_or_default(),
                }));
                session.history.lock().push(json!({
                    "role": "user",
                    "content": "[SYSTEM] Your previous turn was empty. Please respond — either call a tool to gather information, or give a direct text answer. Do not return an empty turn.",
                }));
                continue;
            }
            // Exhausted retries: surface to user, stop spinning.
            println!("  \x1b[31m✗ Model returned empty responses {n} times — giving up on this turn.\x1b[0m");
            session.trace.lock().record_error("empty_response", &format!("{n} consecutive empty responses"));
            session.history.lock().push(json!({
                "role": "assistant",
                "content": "(no response from model after multiple retries)",
            }));
            break;
        }

        // Badger: action-task gave no-tool short response. Skip when the
        // router pinned us to `respond` (no tools available — badgering
        // would spin the loop), and only fire when there's actual content.
        if tool_calls_this_turn == 0
            && current_category.as_deref() != Some("respond")
            && matches!(task_type, "coding" | "editing" | "backend")
            && content_opt
                .as_deref()
                .map(|c| !c.contains('?') && c.len() < 200 && !c.is_empty())
                .unwrap_or(false)
        {
            if let Some(content) = &content_opt {
                let mut hist = session.history.lock();
                hist.push(json!({"role": "assistant", "content": content}));
                hist.push(json!({
                    "role": "user",
                    "content": "[SYSTEM] You responded without using any tools. This task requires file operations. Use the appropriate tools (read_file, write_file, patch, etc.) — actually do it.",
                }));
                continue;
            }
        }

        // Contract close-the-loop guard. If a contract is active AND
        // unclosed when the model tries to end the turn (final text
        // response, no tool calls this iteration), refuse: the model
        // must call mark_assertion for each pending assertion and then
        // close_contract. This is what makes the contract actually do
        // its job — without it, the model proposes the contract and
        // walks away, defeating the whole point of the feature.
        //
        // Bounded to MAX_CONTRACT_LOOP retries so we don't spin
        // forever on a model that refuses to engage with the contract
        // tools.
        if itsy::settings::get().contract {
            if let Some(c) = itsy::session::contract::current() {
                let counts = c.counts();
                let needs_more_work = counts.pending > 0 || counts.failed > 0;
                if needs_more_work {
                    const MAX_CONTRACT_LOOP: u32 = 4;
                    let n = *per_turn_repeats
                        .entry("__contract_loop".into())
                        .and_modify(|n| *n += 1)
                        .or_insert(1);
                    if n <= MAX_CONTRACT_LOOP {
                        println!(
                            "  \x1b[33m⚠ contract not closed: {} pending, {} failed — re-asking model to finish ({n}/{MAX_CONTRACT_LOOP})\x1b[0m",
                            counts.pending, counts.failed
                        );
                        // Build a precise list of what's outstanding.
                        let pending_ids: Vec<&str> = c
                            .assertions
                            .iter()
                            .filter(|a| a.state
                                == itsy::session::contract::AssertionState::Pending)
                            .map(|a| a.id.as_str())
                            .collect();
                        let failed_ids: Vec<&str> = c
                            .assertions
                            .iter()
                            .filter(|a| a.state
                                == itsy::session::contract::AssertionState::Failed)
                            .map(|a| a.id.as_str())
                            .collect();
                        let mut hist = session.history.lock();
                        if let Some(content) = &content_opt {
                            hist.push(json!({"role": "assistant", "content": content}));
                        }
                        let mut msg = String::from(
                            "[SYSTEM] The turn cannot end yet — the contract is not closed.\n\n"
                        );
                        if !pending_ids.is_empty() {
                            msg.push_str(&format!(
                                "Pending assertions: {}. For each one, run a verification command \
                                 (test, script, etc.) and call `mark_assertion` with the id, state \
                                 (passed/failed/skipped), the command you ran, exit_code, and the \
                                 actual observation (not \"OK\").\n\n",
                                pending_ids.join(", ")
                            ));
                        }
                        if !failed_ids.is_empty() {
                            msg.push_str(&format!(
                                "Failed assertions: {}. Fix the underlying issue (re-edit files, \
                                 re-run tests) and re-mark passed. If genuinely impossible, mark \
                                 skipped with a justification.\n\n",
                                failed_ids.join(", ")
                            ));
                        }
                        msg.push_str(
                            "Once every assertion is non-pending and non-failed, call \
                             `close_contract` `completed`. If you've given up, call \
                             `close_contract` `aborted` with the current state — but try to fix \
                             real failures first."
                        );
                        hist.push(json!({"role": "user", "content": msg}));
                        continue;
                    }
                    // Exhausted the retry budget — record + let turn end.
                    session.trace.lock().record_error(
                        "contract_loop_exhausted",
                        &format!(
                            "model gave up; {} pending + {} failed at end of turn",
                            counts.pending, counts.failed
                        ),
                    );
                }
            }
        }

        // Greeting guard: detect lost-context greeting after failures.
        if tool_calls_this_turn > 0 {
            if let Some(content) = &content_opt {
                if let Some(signal) = session
                    .early_stop
                    .lock()
                    .check_greeting(content, tool_calls_this_turn > 0)
                {
                    let mut hist = session.history.lock();
                    hist.push(json!({"role": "assistant", "content": content}));
                    hist.push(json!({"role": "user", "content": signal.injection}));
                    continue;
                }
            }
        }

        // Stream/print the final response.
        if let Some(content) = &content_opt {
            session
                .history
                .lock()
                .push(json!({"role": "assistant", "content": content}));

            // Detect "step N done" markers to advance the plan tracker.
            let mut plan = session.plan_tracker.lock();
            for cap in regex::Regex::new(r"(?i)\bstep\s*(\d{1,2})[\s:.\-]+(?:done|complete|completed|finished|✓)\b")
                .unwrap()
                .captures_iter(content)
            {
                if let Some(num) = cap.get(1).and_then(|m| m.as_str().parse::<usize>().ok())
                {
                    plan.complete_step(num.saturating_sub(1));
                }
            }
            drop(plan);

            let fs_assist = session.fullscreen.lock().clone();
            if let Some(fs) = &fs_assist {
                fs.add_chat(itsy::fullscreen::ChatRole::Assistant, content.clone());
            } else {
                println!("{}", tui::render_markdown(content));
            }
            // Post-hoc reviewer — costs one extra LLM call per turn so
            // it's off by default. When enabled, surfaces an "LGTM" or
            // a short list of issues. Only fires when files were edited
            // this turn and the response is non-trivial (> 50 chars) —
            // mirrors JS: `_editedFilesThisTurn.length > 0 && message.content.length > 50`.
            if session.config.lock().features.reviewer
                && !edited_files.is_empty()
                && content.len() > 50
            {
                let cfg = session.config.lock().clone();
                let review = itsy::model::reviewer::review_response(
                    &user_msg,
                    content,
                    &edited_files,
                    &cfg,
                )
                .await;
                let notice = itsy::model::reviewer::format_reviewer_injection(review.as_ref());
                if !notice.is_empty() {
                    if let Some(fs) = &fs_assist {
                        fs.add_chat(itsy::fullscreen::ChatRole::System, notice.clone());
                    } else {
                        println!("{}", tui::paint("\x1b[33m", &notice));
                    }
                    session.history.lock().push(json!({
                        "role": "user",
                        "content": notice,
                    }));
                }
            }
        } else if tool_calls_this_turn == 0 {
            // No content + no tool calls + nothing tried — try streaming.
            let cfg = session.config.lock().clone();
            let hist = session.history.lock().clone();
            let mut early = session.early_stop.lock();
            let fs_handle = session.fullscreen.lock().clone();
            if let Some(ref fs) = fs_handle {
                fs.set_streaming(true);
            }
            if let Some(out) = stream_final_response(
                &cfg,
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
                drop(early);
                session
                    .history
                    .lock()
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

    if tool_calls_this_turn > 0 {
        println!("{}", tui::turn_summary(tool_calls_this_turn));

        // Auto-commit (Feature: git.auto_commit).
        let auto_commit = session.config.lock().git.auto_commit
            || itsy::settings::get().auto_commit;
        if auto_commit {
            try_auto_commit(&session.cwd, &user_msg, &edited_files).await;
        }
    }

    // Stop the trace recorder for this turn.
    let _ = session.trace.lock().stop();
}

fn record_usage(data: &Value, session: &AgentSession, is_tool_call: bool) {
    if let Some(usage) = data.get("usage") {
        let pt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let ct = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let model = session.config.lock().model.name.clone();
        session
            .tokens
            .lock()
            .record(&json!({"usage": usage}), &model);
        session.token_monitor.lock().record_call(
            pt,
            ct,
            CallMetadata {
                new_turn: false,
                is_tool_call,
            },
        );
        session.trace.lock().record_tokens(pt, ct);
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
            println!("  {}", tui::tool_success(&truncate_short(out, 80), elapsed_ms));
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
        println!("  \x1b[32m✓ git commit: {}\x1b[0m", truncate_short(&msg, 60));
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
    let cwd = session.cwd.clone();

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
            if regex::Regex::new(r"rm\s+-rf\s+/[^.]")
                .unwrap()
                .is_match(command)
                || regex::Regex::new(r"(?i)format\s+c:")
                    .unwrap()
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
            let items = session.memory.lock().load_for_task(task);
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
            let obj = session
                .memory
                .lock()
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
                            config: &cfg,
                            conversation: &conv,
                            tools: get_all_tools(&cfg, None, &ToolDeps::default()),
                            current_task_type: None,
                            system_prompt: build_system_prompt(&cfg, "", "", "", None),
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
                            config: &cfg,
                            conversation: &conv,
                            tools: Vec::new(),
                            current_task_type: None,
                            system_prompt: build_system_prompt(&cfg, "", "", "", None),
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

fn build_session(config: Config, flags: Flags, cwd: PathBuf, mcp_bridge: Arc<McpBridge>) -> AgentSession {
    let memory = Arc::new(Mutex::new(MemoryStore::new(&cwd)));
    let history = Arc::new(Mutex::new(Vec::new()));
    let tokens = Arc::new(Mutex::new(TokenTracker::new()));
    let token_monitor = Arc::new(Mutex::new(TokenMonitor::new()));
    let escalation_opts = EscalationOptions {
        provider: config.escalation.provider.clone(),
        api_key: config.escalation.api_key.clone(),
        model: config.escalation.model.clone(),
        max_per_session: config.escalation.max_per_session,
        confirm: config.escalation.confirm,
        base_url: None,
    };
    let escalation = Arc::new(Mutex::new(EscalationEngine::new(escalation_opts)));
    let mut session_store = SessionStore::new(cwd.clone());
    session_store.create();
    let sessions = Arc::new(Mutex::new(session_store));

    let plan_tracker = Arc::new(Mutex::new(PlanTracker::new()));
    let scorer = Arc::new(Mutex::new(ToolScorer::new()));
    let verification = Arc::new(Mutex::new(VerificationHistory::default()));
    let early_stop = Arc::new(Mutex::new(EarlyStopDetector::new()));
    let dedup = Arc::new(Mutex::new(ToolDedup::new()));
    let trace = Arc::new(Mutex::new(TraceRecorder::new(cwd.clone())));

    let mut skills = SkillManager::with_project_dir(&cwd);
    skills.load_from(&cwd);
    let skills = Arc::new(Mutex::new(skills));

    let mut plugins = PluginLoader::new();
    plugins.load_all(&cwd);
    let plugins = Arc::new(Mutex::new(plugins));

    let config_arc = Arc::new(Mutex::new(config));

    AgentSession {
        config: config_arc,
        history,
        flags,
        memory,
        tokens,
        token_monitor,
        escalation,
        sessions,
        plan_tracker,
        scorer,
        verification,
        early_stop,
        dedup,
        trace,
        skills,
        plugins,
        mcp_bridge,
        cwd,
        current_tool_category: Arc::new(Mutex::new(None)),
        fullscreen: Arc::new(Mutex::new(None)),
    }
}

fn make_cmd_ctx(session: &AgentSession) -> CommandCtx {
    CommandCtx {
        config: session.config.clone(),
        history: session.history.clone(),
        memory: session.memory.clone(),
        tokens: session.tokens.clone(),
        escalation: session.escalation.clone(),
        cwd: Some(session.cwd.clone()),
        token_monitor: Some(session.token_monitor.clone()),
        sessions: Some(session.sessions.clone()),
        multi: None,
        undo: None,
        snapshots: None,
        trace: Some(session.trace.clone()),
        skills: Some(session.skills.clone()),
        plugins: Some(session.plugins.clone()),
        plan: Some(session.plan_tracker.clone()),
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
            let cfg = session.config.lock();
            let hist = session.history.lock();
            print!("{}\n> ", tui::render_status(&cfg, hist.len()));
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
        let cfg = session.config.lock();
        fs.set_model(cfg.model.name.clone());
        fs.set_status(format!("cwd: {}", session.cwd.display()));
    }

    *session.fullscreen.lock() = Some(fs.clone());

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
            let cfg = session.config.lock();
            fs.set_model(cfg.model.name.clone());
            let totals = session.tokens.lock().stats();
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
    *session.fullscreen.lock() = None;
    Ok(())
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

    let flags = Flags {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        endpoint: cli.endpoint.clone(),
        base_url: cli.endpoint.clone(),
        classic: cli.classic,
        verbose: cli.verbose,
    };
    let mut config = load_config(&flags);

    if config.model.name.is_empty() {
        eprintln!("\n  ✗ No model configured.");
        eprintln!("  Edit {}", itsy::paths::config_file().display());
        eprintln!("  Run `itsy --init` to re-run the setup wizard.\n");
        std::process::exit(1);
    }

    // ── Build the merged Settings (config + CLI) and install globally.
    // Anything below this point that wants a runtime knob reads it from
    // `itsy::settings::get()` — there is no env-var fallback any more.
    {
        let mut s = itsy::settings::from_full_config(&config);
        // Named CLI flags.
        if cli.auto_approve {
            s.auto_approve = true;
        }
        if let Some(v) = cli.max_tool_calls {
            s.max_tool_calls = v;
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
        // Mirror CLI overrides back into `config` so downstream code that
        // still threads `&Config` (rather than calling settings::get())
        // sees the same values during the deprecation window.
        config.limits.max_tool_calls = s.max_tool_calls;
        config.limits.max_tool_calls_per_turn = s.max_tool_calls_per_turn;
        config.limits.max_output_tokens = s.max_output_tokens;
        config.limits.request_timeout_ms = s.request_timeout_ms;
        config.tools.bash_timeout = s.bash_timeout;
        config.tools.tool_routing = s.tool_routing.clone();
        config.tools.shell_persist = s.shell_persist;
        config.tools.web_browse = s.web_browse;
        config.tui.auto_approve = s.auto_approve;
        config.security.allow_outside_paths = s.allow_outside_paths;
        config.features.thinking_budget = s.thinking_budget;

        itsy::settings::init(s);
    }

    if cli.print_system_prompt {
        println!("{}", build_system_prompt(&config, "", "", "", None));
        return Ok(());
    }

    if let Some(suite) = cli.eval.as_deref() {
        let code = run_eval(&config, suite).await;
        std::process::exit(code);
    }

    let _reachable = check_endpoint(&mut config).await;

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
    println!("{}", tui::render_welcome(&config, graph_ok));

    let session = Arc::new(build_session(config, flags, cwd, mcp_bridge.clone()));

    if cli.resume {
        if let Some(record) = session.sessions.lock().resume() {
            session.history.lock().extend(record.messages.iter().cloned());
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
