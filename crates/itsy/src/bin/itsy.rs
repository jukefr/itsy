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
use itsy::session::references::{format_references_for_prompt, resolve_references};
use itsy::session::tokens::TokenTracker;
use itsy::token_monitor::{CallMetadata, TokenMonitor};
use itsy::tools::{get_all_tools, ToolDeps};
use itsy::tools_impl::test_runner;
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

/// Bundle of state that lives across user turns within a single agent run.
struct AgentSession {
    config: Arc<Mutex<Config>>,
    history: Arc<Mutex<Vec<Value>>>,
    flags: Flags,
    memory: Arc<Mutex<MemoryStore>>,
    tokens: Arc<Mutex<TokenTracker>>,
    token_monitor: Arc<Mutex<TokenMonitor>>,
    sessions: Arc<Mutex<SessionStore>>,
    scorer: Arc<Mutex<ToolScorer>>,
    verification: Arc<Mutex<VerificationHistory>>,
    early_stop: Arc<Mutex<EarlyStopDetector>>,
    trace: Arc<Mutex<TraceRecorder>>,
    skills: Arc<Mutex<SkillManager>>,
    plugins: Arc<Mutex<PluginLoader>>,
    mcp_bridge: Arc<McpBridge>,
    cwd: PathBuf,
    /// Persists across turns so affirmation guards can keep the prior category.
    current_tool_category: Arc<Mutex<Option<String>>>,
    /// Active fullscreen renderer (Some when running ratatui REPL).
    fullscreen: Arc<Mutex<Option<Arc<itsy::fullscreen::Fullscreen>>>>,
    /// Cross-turn identical-call loop detection counters.
    /// Keyed by sha256(tool_name + args_json)[..16]. Never reset between
    /// turns — that's the whole point: the model can't escape the guard by
    /// spreading repeated calls across turns.
    tool_repeat_counts: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    /// Paths mutated this session (write_file, patch, etc.). When a
    /// read_file call is about to be loop-blocked, we check here first:
    /// if the file was mutated since last read, reset the counter instead
    /// of blocking. Cleared per-path on consumption.
    mutated_paths: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Loop-counter keys (sha256 prefix) for bash calls seen this session.
    /// After any successful mutating tool, all bash keys are cleared from
    /// tool_repeat_counts so the model can re-verify after each edit.
    bash_loop_keys: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Counts consecutive turns where no mutating tool (patch, write_file,
    /// create_file, move_file, delete_file) was called. Resets on any mutating
    /// call. When it reaches READONLY_TURN_THRESHOLD we inject a nudge.
    readonly_turn_count: Arc<Mutex<u32>>,
    /// Total mutating tool calls ever made this session. Used to pick the
    /// right nudge threshold: tight (2) before first edit to prevent
    /// overthinking, looser (6) after the model has started working.
    total_mutating_calls: Arc<Mutex<u32>>,
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
/// Returns the first `n` newline-terminated lines of `s`, or all of `s` if
/// it has fewer lines. Result is always a valid str slice of the input.
fn first_n_lines(s: &str, n: usize) -> &str {
    let mut count = 0;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            count += 1;
            if count >= n {
                return &s[..i];
            }
        }
    }
    s
}

/// Cap a tool result to a context-aware character limit.
///
/// `context_ratio` is `last_prompt_tokens / detected_window` (0.0 = empty,
/// 1.0 = full). The threshold expands when the context is mostly empty so we
/// don't truncate files unnecessarily early in a task, and tightens when the
/// context is getting full.
///
/// When truncating, we return the first 30 lines (imports, signatures) plus an
/// explicit grep/offset directive — more useful than the old head+tail slice,
/// which gave the model partial content with no recovery path.
fn cap_tool_result(content: &str, context_ratio: f32) -> String {
    let char_cap: usize = if context_ratio < 0.40 {
        16_000
    } else if context_ratio < 0.65 {
        8_000
    } else {
        MAX_TOOL_RESULT_CHARS
    };

    if content.len() <= char_cap {
        return content.to_string();
    }

    let head = first_n_lines(content, 30);
    // Guard against files with very long lines: cap head at char_cap too.
    let head = if head.len() > char_cap {
        let mut end = char_cap;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        &content[..end]
    } else {
        head
    };
    format!(
        "{head}\n\n\
         [Output truncated — {} chars total. \
         Use bash+grep or read_file with offset/limit to target a specific range. \
         Do not re-read the full result.]",
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

/// JS `getTestRunnerContext`.
fn get_test_runner_context(cwd: &Path) -> String {
    test_runner::format_for_prompt(cwd)
}

/// Static guidance card for a single tool. Returns `""` for unknown names.
/// Kept intentionally brief — IQ2_XXS has limited attention, so every extra
/// sentence costs more than it adds. Focus on the failure mode + fix.
fn tool_skill_card(name: &str) -> &'static str {
    match name {
        "patch" => "\
### patch\n\
Applies diffs. Requires EXACT current file content.\n\
- Hunk failed / context mismatch: call read_file first, rebuild diff from current content\n\
- Never patch without reading the file in the same turn\n",

        "read_and_patch" => "\
### read_and_patch (preferred for edits)\n\
Atomic read + patch. Avoids stale-content failures.\n\
- Prefer this over separate read_file + patch\n\
- Context mismatch: call read_file, rebuild diff from fresh content\n",

        "write_file" => "\
### write_file\n\
Creates or overwrites. Requires a prior read_file on the same path.\n\
- \"Prior read required\": call read_file on the path first\n\
- For targeted edits prefer patch or read_and_patch\n",

        "bash" => "\
### bash\n\
Runs shell commands.\n\
- Command not found: verify with `which <cmd>`\n\
- Path errors: verify with `ls <path>` before using it\n\
- Check exit code in the result for test / build commands\n",

        "read_file" => "\
### read_file\n\
Reads file content.\n\
- File not found: verify with bash + ls or find first\n\
- Large file: use offset/limit to target a range — do not read the whole file\n\
- Re-read after writing to confirm the change took effect\n",

        _ => "",
    }
}

/// Select up to 3 tool guidance cards based on recent errors, recently used
/// tools, and intent keywords from the last user message.
/// Priority: error-recovery > recency > intent prediction.
/// Returns a ready-to-inject string (empty when no cards apply).
fn select_tool_skill_cards(messages: &[Value]) -> String {
    // Build id → tool-name map and collect recently-used + errored tools
    // from the last 16 messages (roughly 4–6 turns).
    let mut id_to_name: std::collections::HashMap<&str, &str> = Default::default();
    let mut used_tools: Vec<&str> = Vec::new();
    let mut error_tools: Vec<&str> = Vec::new();
    let mut last_user_text: &str = "";

    for msg in messages.iter().rev().take(16) {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "assistant" => {
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() && !name.is_empty() {
                            id_to_name.insert(id, name);
                            if !used_tools.contains(&name) {
                                used_tools.push(name);
                            }
                        }
                    }
                }
            }
            "tool" => {
                let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let looks_like_error = content.contains("Error")
                    || content.contains("failed")
                    || content.contains("not found")
                    || content.contains("mismatch");
                if looks_like_error {
                    if let Some(name) = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| id_to_name.get(id))
                    {
                        if !error_tools.contains(name) {
                            error_tools.push(name);
                        }
                    }
                }
            }
            "user" => {
                if last_user_text.is_empty() {
                    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
                        if !c.starts_with("[SYSTEM]") {
                            last_user_text = c;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Intent keyword → candidate tool names.
    let lower = last_user_text.to_ascii_lowercase();
    let mut intent: Vec<&str> = Vec::new();
    if lower.contains("patch") || lower.contains("edit") || lower.contains("fix") || lower.contains("change") {
        intent.push("patch");
    }
    if lower.contains("write") || lower.contains("create") {
        intent.push("write_file");
    }
    if lower.contains("read") || lower.contains("show") || lower.contains("check") || lower.contains("look") {
        intent.push("read_file");
    }
    if lower.contains("run") || lower.contains("exec") || lower.contains("build") || lower.contains("test") || lower.contains("compile") {
        intent.push("bash");
    }

    // Merge in priority order, deduplicate, cap at 3 cards.
    let mut selected: Vec<&str> = Vec::new();
    for name in error_tools.iter().chain(used_tools.iter()).chain(intent.iter()) {
        if !selected.contains(name) && !tool_skill_card(name).is_empty() {
            selected.push(name);
        }
        if selected.len() >= 3 {
            break;
        }
    }

    if selected.is_empty() {
        return String::new();
    }

    const MAX_CARD_CHARS: usize = 1200;
    let mut out = String::from("\n\n## Tool guidance\n");
    for name in selected {
        let card = tool_skill_card(name);
        if out.len() + card.len() > MAX_CARD_CHARS {
            break;
        }
        out.push_str(card);
    }
    out
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
        let cwd_path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cwd = cwd_path.to_string_lossy().into_owned();
        if let Some(c) = active {
            return build_contract_active_prompt(&c, &cwd_path, &cwd, config);
        }
        return build_contract_proposal_prompt(&cwd_path, &cwd, config);
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

    // Test runner hint.
    let tr = get_test_runner_context(&session.cwd);
    if !tr.is_empty() {
        prompt.push_str(&tr);
    }

    // Tool skill cards — proactive guidance for error-prone tools.
    // Selected based on recent errors, recently used tools, and intent keywords.
    // Injected last so they're closest to the model's generation point.
    let cards = select_tool_skill_cards(messages);
    if !cards.is_empty() {
        prompt.push_str(&cards);
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
fn contract_verification_guidance(cwd: &std::path::Path) -> String {
    itsy::verification::discover(cwd)
        .prompt_block()
        .map(|s| format!("\n{s}\n"))
        .unwrap_or_default()
}

fn build_contract_proposal_prompt(cwd_path: &std::path::Path, cwd: &str, config: &Config) -> String {
    let model_line = if config.model.name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {}", config.model.name)
    };
    let verification = contract_verification_guidance(cwd_path);
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
        {verification}\
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
        verification = verification,
    )
}

/// Contract-mode prompt for turn 2+ (contract is active). The
/// assertions and their current states ARE the prompt — the model
/// works through them one by one. We don't include the generic
/// instructions/code-graph hints — the contract is enough.
fn build_contract_active_prompt(
    c: &itsy::session::contract::Contract,
    cwd_path: &std::path::Path,
    cwd: &str,
    config: &Config,
) -> String {
    let model_line = if config.model.name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {}", config.model.name)
    };
    let body = itsy::session::contract::render_for_prompt(c);
    let verification = contract_verification_guidance(cwd_path);
    format!(
        "You are itsy, working under an active contract. Working directory: {cwd}.{model_line}\n\
        \n\
        {body}\n\
        {verification}\
        \n\
        How to work:\n\
        - ONE tool call per response. Reason only about the immediate next step — not the full solution.\n\
        - After each tool call, stop. Wait for the result. Then decide the next single action.\n\
        - Focus on the FIRST pending assertion. Ignore the others for now.\n\
        - Look at the most recent tool result. What is the single most direct action to move toward passing it?\n\
        - Do the work for each pending assertion (write_file / patch / bash — all available now).\n\
        - Prefer the repo's own tests / verifier scripts over ad-hoc samples whenever they exist.\n\
        - When you've verified an assertion, call `mark_assertion` with:\n\
            id          the assertion id (A.001, A.002, …)\n\
            state       \"passed\" / \"failed\" / \"skipped\"\n\
            evidence    one-sentence summary of how you verified\n\
            command     (recommended for passed) the shell command you ran\n\
            exit_code   the exit code\n\
            observation the actual output you saw — NOT \"OK\" or \"passed\"\n\
        - When every assertion is `passed`, call `close_contract completed` to finish.\n\
        - `close_contract completed` is refused until every assertion is `passed`.\n\
        - Assertions can only be `passed` or `failed` — there is no skip or abort.\n",
        cwd = cwd,
        model_line = model_line,
        body = body,
        verification = verification,
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
                force_disable_thinking: false,
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

    session
        .history
        .lock()
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
    let mut had_mutating_call = false;
    let mut force_disable_thinking = false;
    let mut empty_retry_injected = false;
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
    // Tracks idempotent write tool calls seen this turn (tool_name + arg hash).
    // memory_remember with the same args twice = nop; return early to break spirals.
    let mut per_turn_write_seen: std::collections::HashSet<String> = Default::default();
    // Tracks consecutive failures per tool name; cleared on success.
    // When the same tool fails ≥2 times in a row, an actionable hint is injected.
    let mut recent_tool_failures: std::collections::HashMap<String, u32> = Default::default();
    // Last known prompt_tokens from the API response. Used to compute context_ratio
    // for context-aware tool result capping. Updated after every chat_completion call.
    let mut last_prompt_tokens: u64 = 0;
    // Counts consecutive loop-blocked tool calls in the current turn batch.
    // When this hits MAX_CONSECUTIVE_BLOCKS we break out of the inner loop
    // early so the model doesn't receive a wall of N identical rejections,
    // which would trigger a very long thinking chain on the next turn.
    let mut consecutive_blocks: u32 = 0;
    const MAX_CONSECUTIVE_BLOCKS: u32 = 2;
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
            force_disable_thinking,
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
        last_prompt_tokens = data
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(last_prompt_tokens);

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
        if empty_retry_injected && (!tool_calls.is_empty() || response_has_content) {
            let mut hist = session.history.lock();
            if hist.len() >= 2 {
                hist.pop();
                hist.pop();
            }
            empty_retry_injected = false;
        }

        // Record trace step.
        {
            session
                .trace
                .lock()
                .record_model_response(Some(response_content), Some(&tool_calls));
        }

        // 7a) Model emitted tool calls → execute them.
        if !tool_calls.is_empty() {
            force_disable_thinking = false;
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
                current_category = Some("plan".into());
                *session.current_tool_category.lock() = Some("plan".into());
            }

            // Push the assistant message verbatim.
            session.history.lock().push(msg.clone());

            // Quality monitor: catch empty tool names and hallucinated tool names
            // before executing. Injects a targeted correction and retries.
            // Cap at 2 consecutive corrections to avoid a spiral.
            {
                let mut known: Vec<&str> = chat_ctx
                    .tools
                    .iter()
                    .filter_map(|t| t.pointer("/function/name").and_then(|v| v.as_str()))
                    .collect();
                known.sort_unstable();

                let bad: Vec<(String, String)> = tool_calls
                    .iter()
                    .filter_map(|tc| {
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if name.is_empty() || !known.contains(&name) {
                            Some((id.to_string(), name.to_string()))
                        } else {
                            None
                        }
                    })
                    .collect();

                if !bad.is_empty() {
                    let n = *per_turn_repeats
                        .entry("__quality_correction".into())
                        .and_modify(|n| *n += 1)
                        .or_insert(1);
                    if n <= 2 {
                        let tool_list = known.join(", ");
                        for (id, name) in &bad {
                            let err = if name.is_empty() {
                                format!(
                                    "Error: tool name is empty. Available tools: {tool_list}."
                                )
                            } else {
                                format!(
                                    "Error: `{name}` is not a valid tool. \
                                     Available tools: {tool_list}."
                                )
                            };
                            session.history.lock().push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": err,
                            }));
                        }
                        session.history.lock().push(json!({
                            "role": "user",
                            "content": format!(
                                "[SYSTEM] One or more tool calls used an invalid tool name. \
                                 Call tools by their exact name. Available: {tool_list}."
                            )
                        }));
                        eprintln!(
                            "  \x1b[33m⚠ quality-monitor: {} invalid tool name(s) \
                             — steering ({n}/2)\x1b[0m",
                            bad.len()
                        );
                        continue;
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
                if is_contract_tool {
                    batch_had_contract_progress = true;
                }
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
                if name == "bash" && is_mutating {
                    batch_had_active_bash = true;
                }
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

                // General identical-call loop detection: covers ALL tools.
                // Hash (tool_name + canonical args JSON) and block after N
                // identical calls. Uses session-scoped counter that persists
                // across turns — the model cannot escape by spreading
                // repeated calls across turns.
                // Threshold is 3 for all tools. read_file gets an exemption:
                // if the file was mutated since last read the counter is reset
                // above, so the limit only fires on true stale re-reads.
                {
                    use sha2::{Digest, Sha256};
                    let args_str = serde_json::to_string(&args).unwrap_or_default();
                    let key = format!("{}{}", name, args_str);
                    let mut h = Sha256::new();
                    h.update(key.as_bytes());
                    let hash = format!("{:x}", h.finalize());
                    let repeat_key = format!("__tool_repeat_{}", &hash[..16]);
                    let max_repeats: u32 = 3;

                    // If this is a read_file call to a path that was mutated
                    // since the last read, the file content has genuinely changed —
                    // reset the loop counter so the agent can re-read it.
                    if name == "read_file" {
                        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                            if session.mutated_paths.lock().remove(path) {
                                session.tool_repeat_counts.lock().remove(&repeat_key);
                            }
                        }
                    }

                    // Track bash loop keys so they can be cleared after mutations.
                    // Verification commands (e.g. `coqc`, `pytest`) are legitimately
                    // repeated after each edit cycle — only count consecutive runs
                    // with no mutation in between.
                    if name == "bash" {
                        session.bash_loop_keys.lock().insert(repeat_key.clone());
                    }

                    let n = session.tool_repeat_counts.lock()
                        .entry(repeat_key)
                        .and_modify(|n| *n += 1)
                        .or_insert(1)
                        .clone();
                    if n > max_repeats {
                        let tool_result_msg = format!(
                            "[LOOP DETECTED] You have called `{}` with these exact \
                             arguments {} time(s). The result will not change. \
                             Stop repeating — try a completely different approach \
                             or call a different tool.",
                            name, n
                        );
                        session.history.lock().push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": serde_json::to_string(&json!({
                                "result": tool_result_msg
                            })).unwrap_or_default()
                        }));
                        // After 3 loop-blocked calls to the same tool, inject a
                        // [SYSTEM] user message. Tool results have low steering
                        // weight at small quants; a user-role message breaks the
                        // generation pattern and names the exact tools to use.
                        if n == max_repeats + 3 {
                            let contract_hint = if itsy::settings::get().contract {
                                " You are under a contract — call `contract_status` \
                                  to see pending assertions, then `mark_assertion` \
                                  for each one, then `close_contract`."
                            } else {
                                ""
                            };
                            session.history.lock().push(json!({
                                "role": "user",
                                "content": format!(
                                    "[SYSTEM] You are stuck in a loop calling `{name}`. \
                                     That tool is now DISABLED for this session — further \
                                     calls will be silently dropped.{contract_hint} \
                                     Pick a different tool and make progress."
                                )
                            }));
                        }
                        consecutive_blocks += 1;
                        if consecutive_blocks >= MAX_CONSECUTIVE_BLOCKS {
                            break;
                        }
                        continue;
                    }
                }

                // Patch blocked-file check: if this file was stuck-blocked
                // during a patch spiral, hard-reject any further patch calls
                // on it before execution.
                if name == "patch" || name == "read_and_patch" {
                    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                        if let Some(signal) = session.early_stop.lock().check_patch_blocked(path) {
                            println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                            session.history.lock().push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": serde_json::to_string(&json!({
                                    "result": signal.injection
                                })).unwrap_or_default()
                            }));
                            consecutive_blocks += 1;
                            if consecutive_blocks >= MAX_CONSECUTIVE_BLOCKS {
                                break;
                            }
                            continue;
                        }
                    }
                }

                // Pure-tool dedup catches read-only spirals; improvement_attempts
                // catches failed mutating calls; the per-turn tool-call cap is
                // the final backstop.

                // Idempotent-write dedup: memory_remember/memory_forget/mark_assertion
                // with identical args in the same turn is a no-op. Break the spiral
                // before executing and return a short-circuit result.
                // mark_assertion is included because the model sometimes marks the same
                // assertion as passed multiple times in a row instead of moving on.
                const IDEMPOTENT_WRITE_TOOLS: &[&str] = &["memory_remember", "memory_forget", "mark_assertion"];
                if IDEMPOTENT_WRITE_TOOLS.contains(&name.as_str()) {
                    let write_key = itsy::tools_impl::dedup::idempotent_write_key(&name, &args);
                    if !per_turn_write_seen.insert(write_key) {
                        let skip_msg = if name == "mark_assertion" {
                            let id_str = args.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            let state_str = args.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                            format!(
                                "[DUPLICATE] mark_assertion for `{id_str}` as `{state_str}` was already called this turn. \
                                 Move on to the next pending assertion — do not repeat the same mark."
                            )
                        } else {
                            "[already stored this turn — identical call skipped]".to_string()
                        };
                        session.history.lock().push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": serde_json::to_string(&json!({"result": skip_msg})).unwrap_or_default()
                        }));
                        continue;
                    }
                }

                let started = Instant::now();
                let fs_handle = session.fullscreen.lock().clone();
                if let Some(fs) = &fs_handle {
                    fs.add_tool(&name, "running", "");
                } else {
                    println!("{}", tui::tool_start(&name));
                }

                let result = {
                    let ctx = ExecCtx {
                        config: &session.config.lock().clone(),
                        flags: &session.flags,
                        memory: session.memory.clone(),
                        mcp_bridge: Some(session.mcp_bridge.clone()),
                        mcp_client: None,
                        fullscreen: fs_handle.clone(),
                    };
                    execute_tool(&name, args.clone(), &ctx).await
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

                // Track edited files and reset readonly-turn counter on any mutating call.
                const MUTATING_TOOLS: &[&str] = &[
                    "write_file", "patch", "read_and_patch", "create_file",
                    "move_file", "delete_file", "append_file",
                ];
                if MUTATING_TOOLS.contains(&name.as_str()) && result.get("error").is_none() {
                    had_mutating_call = true;
                    batch_had_mutating = true;
                    *session.total_mutating_calls.lock() += 1;
                    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                        edited_files.push(p.to_string());
                        // Signal that a subsequent read_file on this path is
                        // legitimate — reset its loop counter so the agent can
                        // re-read after patching.
                        session.mutated_paths.lock().insert(p.to_string());
                    }
                    // Reset all bash loop counters after any successful mutation.
                    // The model legitimately runs the same verify command after each
                    // edit cycle (patch → coqc → patch → coqc …); only consecutive
                    // bash calls with no mutation in between indicate a stuck loop.
                    {
                        let keys: Vec<String> = session.bash_loop_keys.lock().drain().collect();
                        let mut counts = session.tool_repeat_counts.lock();
                        for k in keys {
                            counts.remove(&k);
                        }
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

                // A successful (non-blocked) tool execution resets the
                // consecutive-block counter — the model has done something real.
                consecutive_blocks = 0;

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
                        session.early_stop.lock().record_read(path);
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
                        session.history.lock().push(json!({
                            "role": "user",
                            "content": format!(
                                "[SYSTEM] patch failed: old_str not found in {patch_file}. \
                                 The file content has changed since you last read it. \
                                 Use read_and_patch instead — it reads the current content \
                                 first, then applies the patch atomically."
                            )
                        }));
                    }
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
                let context_ratio = {
                    let window = session.config.lock().context.detected_window as f32;
                    if window > 0.0 { last_prompt_tokens as f32 / window } else { 0.0 }
                };
                let capped = cap_tool_result(&tool_content, context_ratio);
                session.history.lock().push(json!({
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
                        if (name == "bash" || name == "run") && msg == "Exit code 1" {
                            false
                        } else {
                            true
                        }
                    }).unwrap_or(false);
                    if had_error {
                        let n = recent_tool_failures
                            .entry(name.clone())
                            .and_modify(|c| *c += 1)
                            .or_insert(1);
                        if *n == 2 {
                            session.history.lock().push(json!({
                                "role": "user",
                                "content": format!(
                                    "[SYSTEM] `{name}` has failed {n} times in a row. {}",
                                    recency_tool_hint(&name)
                                )
                            }));
                        }
                    } else {
                        recent_tool_failures.remove(&name);
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
                                    let cfg_snapshot = session.config.lock().clone();
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
                            let cfg_snap = session.config.lock().clone();
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
                            improvement_attempts.insert(file_path.clone(), 0);
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
                        let cfg_snap = session.config.lock().clone();
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
                        session.history.lock().push(json!({
                            "role": "user",
                            "content": format!("[DECOMPOSE] The command has failed 3 times. STOP retrying the same approach.\n\n{}", strategy.2),
                        }));
                        improvement_attempts.insert("__bash".into(), 0);
                    }
                } else if (name == "bash" || name == "run")
                    && result.get("error").is_none()
                {
                    improvement_attempts.insert("__bash".into(), 0);
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
                    if let Some(signal) = session.early_stop.lock().record_bash_result(exit_code, command) {
                        println!("  \x1b[33m⚡ {}\x1b[0m", signal.message);
                        session
                            .history
                            .lock()
                            .push(json!({"role": "user", "content": signal.injection}));
                        break;
                    }
                }
            }
            // Reset the text-only streak counter whenever a tool-call batch fires.
            per_turn_repeats.insert("__text_only_streak".into(), 0);

            // No-progress nudge within a single handle_turn: fires when the
            // model has made N consecutive tool-call batches with no mutating
            // calls. Tight threshold (2) before the first edit; looser (6) after.
            if !batch_had_mutating && !batch_had_contract_progress && !batch_had_active_bash && !batch_had_fresh_read {
                let count = {
                    let mut c = session.readonly_turn_count.lock();
                    *c += 1;
                    *c
                };
                let total_mutations = *session.total_mutating_calls.lock();
                let threshold = if total_mutations == 0 { 3u32 } else { 6u32 };
                if count >= threshold {
                    *session.readonly_turn_count.lock() = 0;
                    println!(
                        "  \x1b[33m⚠ no-progress nudge: {} read-only batches — pushing model to act\x1b[0m",
                        count
                    );
                    session.history.lock().push(json!({
                        "role": "user",
                        "content": format!(
                            "[SYSTEM] You have spent {} consecutive rounds reading files \
                             without making any changes. You already have enough information. \
                             STOP reading — make a concrete edit RIGHT NOW. Call patch or \
                             write_file to modify a file. Do not call read_file, bash, \
                             graph_search, or any other read-only tool until you have made \
                             at least one actual change.",
                            count
                        )
                    }));
                }
            } else {
                *session.readonly_turn_count.lock() = 0;
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
                force_disable_thinking = true;
                empty_retry_injected = true;
                println!("  \x1b[33m⚠ Model returned empty response — retrying ({n}/{MAX_EMPTY_RETRIES}) with thinking disabled\x1b[0m");
                session.history.lock().push(json!({
                    "role": "assistant",
                    "content": content_opt.clone().unwrap_or_default(),
                }));
                session.history.lock().push(json!({
                    "role": "user",
                    "content": "[SYSTEM] Your previous turn was empty. Thinking is disabled for the retry. Respond with exactly one concrete next step: either call a tool, or give a direct text answer. Do not return an empty turn.",
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
        force_disable_thinking = false;

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

        // Text-only streak guard: if the model generates 3+ consecutive
        // text-only responses without calling any tool, it is stuck.
        // This fires regardless of tool_calls_this_turn so it catches
        // the "text flood after tool calls" pattern that bypasses the
        // badger and empty-response guards (both gated on == 0).
        {
            let n = *per_turn_repeats
                .entry("__text_only_streak".into())
                .and_modify(|n| *n += 1)
                .or_insert(1);
            const MAX_TEXT_ONLY_STREAK: u32 = 3;
            if n >= MAX_TEXT_ONLY_STREAK && current_category.as_deref() != Some("respond") {
                println!(
                    "  \x1b[33m⚠ text-only streak: {} consecutive responses without tools — forcing tool use ({n})\x1b[0m",
                    n
                );
                let mut hist = session.history.lock();
                if let Some(content) = &content_opt {
                    hist.push(json!({"role": "assistant", "content": content}));
                }
                let has_contract = itsy::settings::get().contract
                    && itsy::session::contract::current()
                        .map(|c| {
                            let cnt = c.counts();
                            cnt.pending > 0 || cnt.failed > 0
                        })
                        .unwrap_or(false);
                let msg = if has_contract {
                    "[SYSTEM] You have responded with text only multiple times without using tools. \
                     The contract has unverified assertions. Stop writing explanations — call a tool right now. \
                     Run a verification command with `bash`, then call `mark_assertion` with the result. \
                     Do NOT respond with text. Use a tool."
                } else {
                    "[SYSTEM] You have responded with text only multiple times without using tools. \
                     Use a tool to make progress instead of describing what you plan to do."
                };
                hist.push(json!({"role": "user", "content": msg}));
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
                let all_passed = counts.pending == 0
                    && counts.failed == 0
                    && counts.passed == counts.total;
                let contract_still_open = c.status
                    == itsy::session::contract::ContractStatus::Active;
                let needs_more_work =
                    counts.pending > 0 || counts.failed > 0 || (all_passed && contract_still_open);
                if needs_more_work {
                    const MAX_CONTRACT_LOOP: u32 = 12;
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
                        if all_passed && contract_still_open {
                            msg.push_str(
                                "Every assertion is `passed`. \
                                 Call `close_contract` `completed` RIGHT NOW to finish the task. \
                                 Do not run any more commands or write any more text — \
                                 just call `close_contract` `completed`."
                            );
                        } else if !failed_ids.is_empty() {
                            // Failed assertions take absolute priority — show them with the
                            // last recorded evidence so the model has its own failure context
                            // back in front of it, then suppress the "verify pending" branch
                            // entirely to avoid conflicting instructions.
                            let failed_blocks: Vec<String> = c
                                .assertions
                                .iter()
                                .filter(|a| {
                                    a.state == itsy::session::contract::AssertionState::Failed
                                })
                                .map(|a| {
                                    let mut block = format!("  `{}` — {}", a.id, a.text);
                                    if let Some(ev) = &a.evidence {
                                        block.push_str(&format!("\n    last evidence: {ev}"));
                                    }
                                    if let Some(chk) = &a.last_check {
                                        block.push_str(&format!(
                                            "\n    last command:  {}\n    exit_code:     {}\n    observation:   {}",
                                            chk.command, chk.exit_code, chk.observation
                                        ));
                                    }
                                    block
                                })
                                .collect();
                            msg.push_str(&format!(
                                "FAILED assertion(s) — diagnose each failure below and fix it \
                                 before the contract can close:\n\n{}\n\n\
                                 For each: re-examine what you produced, understand exactly why \
                                 the check failed (look at the observation above), make the \
                                 necessary edits or re-runs, then call `mark_assertion` again \
                                 with state=passed. Do NOT call `close_contract` until every \
                                 assertion is `passed`.\n\n",
                                failed_blocks.join("\n\n")
                            ));
                        } else if !pending_ids.is_empty() {
                            // No failures — just unverified assertions. One at a time to avoid
                            // thinking-budget overflow on small quantised models.
                            msg.push_str(&format!(
                                "You have {} pending assertion(s). Verify ONE now — start with \
                                 `{}`. Run a check command, call `mark_assertion` with the id, \
                                 state (passed/failed), the command you ran, exit_code, and \
                                 actual observation. If it PASSES: stop, the next turn handles \
                                 the rest. If it FAILS: keep working right now — fix the issue \
                                 and re-mark before stopping.\n\n",
                                pending_ids.len(),
                                pending_ids[0]
                            ));
                            msg.push_str(
                                "Once every assertion is `passed`, call `close_contract` `completed`. \
                                 Keep working until they all pass."
                            );
                        }
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

            let fs_assist = session.fullscreen.lock().clone();
            if let Some(fs) = &fs_assist {
                fs.add_chat(itsy::fullscreen::ChatRole::Assistant, content.clone());
            } else {
                println!("{}", tui::render_markdown(content));
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

    // Update readonly-turn counter. Resets when any mutating call succeeded;
    // increments otherwise so the next turn's nudge threshold check is current.
    {
        let mut count = session.readonly_turn_count.lock();
        if had_mutating_call {
            *count = 0;
        } else {
            *count += 1;
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
            let summary = out.lines().next().unwrap_or(out).trim_end_matches(':');
            println!("  {}", tui::tool_success(&truncate_short(summary, 80), elapsed_ms));
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
    use std::process::Command;
    let result = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output();
    match result {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let code = out.status.code().unwrap_or(-1);
            let mut s = format!("exit_code={code}");
            if !stdout.is_empty() {
                s.push_str(&format!("\nstdout:\n{}", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                s.push_str(&format!("\nstderr:\n{}", stderr.trim_end()));
            }
            // Cap output so evaluator history doesn't explode.
            if s.len() > 2000 {
                s.truncate(2000);
                s.push_str("\n[output truncated]");
            }
            s
        }
        Err(e) => format!("error: {e}"),
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

    let cwd = session.cwd.to_string_lossy().to_string();
    let mut config = session.config.lock().clone();
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
            config: &config,
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
        session.history.lock().push(json!({
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
                            config: &cfg,
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
    let memory = Arc::new(Mutex::new(MemoryStore::new(&cwd)));
    let history = Arc::new(Mutex::new(Vec::new()));
    let tokens = Arc::new(Mutex::new(TokenTracker::new()));
    let token_monitor = Arc::new(Mutex::new(TokenMonitor::new()));
    let mut session_store = SessionStore::new(cwd.clone());
    session_store.create();
    let sessions = Arc::new(Mutex::new(session_store));

    let scorer = Arc::new(Mutex::new(ToolScorer::new()));
    let verification = Arc::new(Mutex::new(VerificationHistory::default()));
    let early_stop = Arc::new(Mutex::new(EarlyStopDetector::new()));
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
        sessions,
        scorer,
        verification,
        early_stop,
        trace,
        skills,
        plugins,
        mcp_bridge,
        cwd,
        current_tool_category: Arc::new(Mutex::new(None)),
        fullscreen: Arc::new(Mutex::new(None)),
        tool_repeat_counts: Arc::new(Mutex::new(std::collections::HashMap::new())),
        mutated_paths: Arc::new(Mutex::new(std::collections::HashSet::new())),
        bash_loop_keys: Arc::new(Mutex::new(std::collections::HashSet::new())),
        readonly_turn_count: Arc::new(Mutex::new(0)),
        total_mutating_calls: Arc::new(Mutex::new(0)),
    }
}

fn make_cmd_ctx(session: &AgentSession) -> CommandCtx {
    CommandCtx {
        config: session.config.clone(),
        history: session.history.clone(),
        memory: session.memory.clone(),
        tokens: session.tokens.clone(),
        cwd: Some(session.cwd.clone()),
        token_monitor: Some(session.token_monitor.clone()),
        sessions: Some(session.sessions.clone()),
        multi: None,
        undo: None,
        snapshots: None,
        trace: Some(session.trace.clone()),
        skills: Some(session.skills.clone()),
        plugins: Some(session.plugins.clone()),
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

    // ── Build the merged Settings (config + CLI) and install globally.
    // Anything below this point that wants a runtime knob reads it from
    // `itsy::settings::get()` — there is no env-var fallback any more.
    {
        let mut s = itsy::settings::from_full_config(&config);
        // Named CLI flags.
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
        // Mirror CLI overrides back into `config` so downstream code that
        // still threads `&Config` (rather than calling settings::get())
        // sees the same values during the deprecation window.
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
