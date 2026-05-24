//! Dispatches the 18+ built-in tools and forwards
//! plugin/MCP calls through the registered adapters.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::config::Flags;
use crate::mcp_bridge::McpBridge;
use crate::memory::MemoryStore;
use crate::security::{
    build_command, escape_shell_arg, redact_string, safe_resolve_path, sanitize_tool_output,
    strip_ansi, PathOptions,
};
use crate::session::file_state::get_file_state_tracker;
use crate::session::snapshot::get_snapshot_manager;
use crate::tools_impl::file_tree::format_smart_listing;
use crate::tools_impl::mcp_client::McpClient;
use crate::tools_impl::read_tracker::get_read_tracker;
use crate::tools_impl::shell_session::{get_shell, ShellOptions};
use crate::tools_impl::web_browse::{web_fetch, web_search};
use crate::Config;

/// Shared execution context handed to [`execute_tool`].
pub struct ExecCtx<'a> {
    pub config: &'a Config,
    pub flags: &'a Flags,
    pub memory: Arc<Mutex<MemoryStore>>,
    pub mcp_bridge: Option<Arc<McpBridge>>,
    pub mcp_client: Option<Arc<McpClient>>,
    pub fullscreen: Option<Arc<crate::fullscreen::Fullscreen>>,
}

const MAX_CONTENT_CHARS: usize = 8000;

pub async fn execute_tool(name: &str, mut args: Value, ctx: &ExecCtx<'_>) -> Value {
    sanitize_args(&mut args);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match name {
        "read_file" => exec_read_file(&args, &cwd).await,
        "read_original" => exec_read_original(&args, &cwd).await,
        "write_file" => exec_write_file(&args, &cwd, ctx).await,
        "append_file" => exec_append_file(&args, &cwd).await,
        "patch" => exec_read_and_patch(&args, &cwd).await,
        "bash" => exec_bash(&args, &cwd, ctx).await,
        "search" => exec_search(&args, &cwd).await,
        "find_files" => exec_find_files(&args, &cwd).await,
        "list_projects" => exec_list_projects(ctx, &cwd).await,
        "graph_search" => exec_graph_search(&args, ctx, &cwd).await,
        "explain_symbol" => exec_explain_symbol(&args, ctx, &cwd).await,
        "read_and_patch" => exec_read_and_patch(&args, &cwd).await,
        "create_and_run" => exec_create_and_run(&args, &cwd).await,
        "find_and_read" => exec_find_and_read(&args, &cwd).await,
        "search_and_read" => exec_search_and_read(&args, &cwd).await,
        "run" => exec_run(&args, &cwd).await,
        "memory_load" | "memory_remember" | "memory_list" | "memory_forget" => {
            exec_memory(name, &args, ctx).await
        }
        "web_search" => exec_web_search(&args).await,
        "web_fetch" => exec_web_fetch(&args).await,
        "propose_contract" => exec_propose_contract(&args, &cwd, ctx.config).await,
        "mark_assertion" => exec_mark_assertion(&args, &cwd, ctx.config).await,
        "mark_feature" => exec_mark_feature(&args, &cwd).await,
        "contract_status" => exec_contract_status().await,
        "close_contract" => exec_close_contract(&args, &cwd, ctx.config).await,
        "select_category" => {
            let cat = args.get("category").and_then(|v| v.as_str()).unwrap_or("read");
            json!({"result": format!("Category: {cat}. Proceed with your tool call."), "category": cat})
        }
        _ => {
            if let Some(mcp) = &ctx.mcp_client {
                if mcp.is_mcp_tool(name) {
                    return match mcp.call_tool(name, args).await {
                        Ok(v) => json!({"result": v}),
                        Err(e) => json!({"error": e.to_string()}),
                    };
                }
            }
            json!({"error": format!("Unknown tool: {name}")})
        }
    }
}

fn sanitize_args(args: &mut Value) {
    if let Some(obj) = args.as_object_mut() {
        for (_, v) in obj.iter_mut() {
            if let Some(s) = v.as_str() {
                *v = Value::String(strip_ansi(s));
            }
        }
    }
}

async fn exec_read_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "read_file rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("read_file rejected: {e}")}),
    };
    if !safe.full_path.exists() {
        return json!({"error": format!("File not found: {path}")});
    }
    let read_tracker = get_read_tracker();
    read_tracker.record_read(&safe.full_path, cwd);
    let content = match fs::read_to_string(&safe.full_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let lines: Vec<&str> = content.split('\n').collect();
    let total = lines.len();
    let start = args.get("start_line").and_then(|v| v.as_u64()).map(|n| n.saturating_sub(1) as usize).unwrap_or(0);
    let end = args.get("end_line").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(total);
    let slice = &lines[start.min(total)..end.min(total)];
    let safe_slice: Vec<String> = slice.iter().map(|l| sanitize_tool_output(l)).collect();
    let numbered: String = safe_slice
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>4}│ {l}", start + i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    if args.get("start_line").is_none() && args.get("end_line").is_none() {
        match get_file_state_tracker().record(&safe.full_path, &content) {
            crate::session::file_state::RecordResult::Unchanged => {
                return json!({"result": format!("{path} ({total} lines — unchanged since last read, no diff)")});
            }
            crate::session::file_state::RecordResult::Diff { diff, full_length } => {
                return json!({"result": format!("{path} changes since last read ({full_length} lines total):\n{}", sanitize_tool_output(&diff))});
            }
            _ => {}
        }

        // Summarize large files (>200 lines) via the features adapter.
        // Matches JS Feature 2: context savings on large files with no range.
        if total > 200 && crate::features_adapter::is_features_available() {
            if let Some(summary) = crate::features_adapter::summarize_file_compiled(path, &content, 600).await {
                if summary.len() > 50 {
                    return json!({"result": format!("{path} ({total} lines — summarized):\n{}", sanitize_tool_output(&summary))});
                }
            }
        }
    }
    json!({"result": format!("{path} ({total} lines):\n{numbered}")})
}

async fn exec_read_original(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "read_original rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("read_original rejected: {e}")}),
    };
    match get_file_state_tracker().get_original(&safe.full_path) {
        Some(content) => {
            let total = content.split('\n').count();
            let numbered: String = content
                .split('\n')
                .enumerate()
                .map(|(i, l)| format!("{:>4}│ {}", i + 1, sanitize_tool_output(l)))
                .collect::<Vec<_>>()
                .join("\n");
            json!({"result": format!("{path} (original, {total} lines):\n{numbered}")})
        }
        None => {
            json!({"error": format!(
                "No original content recorded for {path}. \
                read_original only works for files that were read with read_file earlier this session."
            )})
        }
    }
}

async fn exec_write_file(args: &Value, cwd: &Path, ctx: &ExecCtx<'_>) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "write_file rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("write_file rejected: {e}")}),
    };
    // write_file is for creating new files only. Edits to existing files must
    // use read_and_patch so the model always works from current content.
    if safe.full_path.exists() {
        return json!({
            "error": format!(
                "write_file rejected: '{path}' already exists. \
                 Use read_and_patch to edit existing files — it reads the current content \
                 atomically so you never accidentally overwrite with stale data."
            )
        });
    }
    if let Some(dir) = safe.full_path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.len() > MAX_CONTENT_CHARS {
        return json!({
            "error": format!(
                "write_file: content too large ({} lines / {}KB). \
                 Keep each write_file under 60 lines; use append_file for additional sections.",
                content.split('\n').count(),
                content.len() / 1024,
            )
        });
    }
    get_snapshot_manager(cwd.to_path_buf()).note(&safe.full_path, None);
    if let Err(e) = fs::write(&safe.full_path, content) {
        return json!({"error": e.to_string()});
    }
    get_read_tracker().record_write(&safe.full_path, cwd);
    get_file_state_tracker().record_write(&safe.full_path, content);
    let lines = content.split('\n').count();
    json!({"result": format!("Created {path} ({lines} lines)"), "action": "Created", "path": path, "lines": lines})
}

async fn exec_append_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "append_file rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("append_file rejected: {e}")}),
    };
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.len() > 8000 {
        return json!({"error": format!("append_file: chunk too large ({} KB). Keep each append under 60 lines.", content.len() / 1024)});
    }
    if !safe.full_path.exists() {
        return json!({"error": format!("append_file: file not found: {path}. Create it first with write_file.")});
    }
    let before = fs::read_to_string(&safe.full_path).unwrap_or_default();
    get_snapshot_manager(cwd.to_path_buf()).note(&safe.full_path, Some(before.clone()));
    let sep = if !before.is_empty() && !before.ends_with('\n') { "\n" } else { "" };
    let new = format!("{before}{sep}{content}");
    if let Err(e) = fs::write(&safe.full_path, &new) {
        return json!({"error": e.to_string()});
    }
    get_file_state_tracker().record_write(&safe.full_path, &new);
    get_read_tracker().record_write(&safe.full_path, cwd);
    let total = new.split('\n').count();
    let added = content.split('\n').count();
    json!({"result": format!("Appended {added} lines to {path} (now {total} lines total)"), "action": "Appended", "path": path})
}

async fn exec_patch(args: &Value, cwd: &Path, ctx: &ExecCtx<'_>) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "patch rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("patch rejected: {e}")}),
    };
    if !safe.full_path.exists() {
        return json!({"error": format!("File not found: {path}")});
    }
    let guard = get_read_tracker().check_write(&safe.full_path, cwd, "patch");
    if !guard.ok {
        return json!({"error": guard.reason});
    }
    let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
    let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
    let content = match fs::read_to_string(&safe.full_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": e.to_string()}),
    };
    get_snapshot_manager(cwd.to_path_buf()).note(&safe.full_path, Some(content.clone()));
    if old_str.is_empty() {
        return json!({"error": "patch: old_str is empty"});
    }
    let count = content.matches(old_str).count();
    if count == 0 {
        // Last-chance recovery: ask the model to merge the intended
        // change into the current file. Gated on config.features.semantic_merge.
        if ctx.config.features.semantic_merge {
            if let Some(merged) = crate::features_adapter::semantic_merge(path, new_str, &content).await {
                if let Err(e) = fs::write(&safe.full_path, &merged) {
                    return json!({"error": e.to_string()});
                }
                get_file_state_tracker().record_write(&safe.full_path, &merged);
                get_read_tracker().record_patch(&safe.full_path, cwd);
                let old_lines = content.split('\n').count();
                let new_lines = merged.split('\n').count();
                return json!({
                    "result": format!("Patched {path} via semantic merge ({old_lines} → {new_lines} lines)"),
                    "action": "Edited",
                    "path": path,
                    "line": 1,
                });
            }
        }
        return json!({"error": format!("old_str not found in {path}")});
    }
    if count > 1 {
        return json!({"error": format!("old_str matches {count} locations. Include more context.")});
    }
    let new_content = content.replacen(old_str, new_str, 1);
    if let Err(e) = fs::write(&safe.full_path, &new_content) {
        return json!({"error": e.to_string()});
    }
    get_file_state_tracker().record_write(&safe.full_path, &new_content);
    get_read_tracker().record_patch(&safe.full_path, cwd);
    let prefix = new_content.split(new_str).next().unwrap_or("");
    let line_num = prefix.split('\n').count();
    let old_lines = old_str.split('\n').count();
    let new_lines = new_str.split('\n').count();
    if let Some(fs_ref) = &ctx.fullscreen {
        fs_ref.add_diff(path, old_str, new_str, line_num as u32);
    } else {
        print!("{}", crate::tui::render_diff(path, old_str, new_str, line_num as u32));
    }
    json!({
        "result": format!("Patched {path}: replaced {old_lines} lines with {new_lines} lines at line {line_num}"),
        "action": "Edited",
        "path": path,
        "line": line_num,
    })
}

async fn exec_bash(args: &Value, cwd: &Path, ctx: &ExecCtx<'_>) -> Value {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return json!({"error": "bash: command missing"});
    };
    let command = rtk_rewrite(command);

    let blocking = regex::Regex::new(r"(?i)^(node|python|python3|ruby|php|go run|deno run|bun run)\s+.*\b(server\.(js|py|rb|php|ts)|app\.(js|py|rb|php|ts))\b").unwrap();
    let explicit = regex::Regex::new(r"(?i)\b(uvicorn|gunicorn|rails\s+s|npm\s+start|yarn\s+start|npm\s+run\s+dev|python3?\s+-m\s+(flask|django|uvicorn|aiohttp\.web|fastapi)|puma|unicorn|passenger)\b").unwrap();
    if (blocking.is_match(&command) || explicit.is_match(&command))
        && !command.contains("--check")
        && !command.contains("--version")
        && !command.contains("test")
    {
        return json!({
            "result": format!("Refused: \"{command}\" would start a long-running server that blocks. Use \"node --check <file>\" to verify syntax, or describe what you want to test and I'll use a non-blocking approach."),
            "error": "Blocking command detected",
            "command": command,
        });
    }
    // Interactive-stdin guard
    let script_re = regex::Regex::new(r#"(?:^|\s)(python3?|node|ruby)\s+["']?([^\s"']+)"#).unwrap();
    if let Some(caps) = script_re.captures(&command) {
        let target_rel = caps[2].to_string();
        let target = cwd.join(&target_rel);
        if target.exists() && !command.contains("--check") && !command.contains("-c") && !command.contains("-m") {
            if let Ok(fc) = fs::read_to_string(&target) {
                if fc.contains("input(") || fc.contains("readline.question") || fc.contains("process.stdin.on") {
                    return json!({
                        "result": format!("Refused: \"{command}\" — file contains interactive input() calls that block in non-interactive mode. File created successfully. Verify syntax: python -m py_compile {target_rel}"),
                        "error": "Interactive script detected",
                        "command": command,
                    });
                }
            }
        }
    }

    let marker = create_bash_marker();

    let persistent = crate::settings::get().shell_persist;
    if persistent {
        let shell = get_shell(ShellOptions { cwd: cwd.to_path_buf(), ..Default::default() });
        let result = shell.run(&command).await;
        let max_output = if ctx.config.context.detected_window < 64_000 { 1500 } else { 3000 };
        let trimmed = trim_output(&result.stdout, max_output);
        if result.timed_out {
            if let Some(ref m) = marker { let _ = fs::remove_file(m); }
            return json!({"result": if trimmed.is_empty() { "(no output before timeout)".to_string() } else { trimmed.clone() }, "error": "Timed out (killed after 30s)", "command": command});
        }
        if let Some(ref m) = marker { record_bash_mutations(m, cwd); let _ = fs::remove_file(m); }
        if let Some(err) = result.error {
            return json!({"result": trimmed, "error": err, "command": command});
        }
        if result.exit_code != 0 {
            // grep/rg exit 1 with no output = no matches found, not an error.
            if result.exit_code == 1 && trimmed.is_empty() && is_grep_command(&command) {
                return json!({"result": "(no matches found)", "command": command});
            }
            let body = if trimmed.is_empty() { "(no output)".to_string() } else { trimmed.clone() };
            let with_hint = maybe_prepend_error_diagnosis(
                ctx.config.features.error_diagnosis,
                &command,
                &body,
                result.exit_code,
            )
            .await;
            return json!({"result": with_hint, "error": format!("Exit code {}", result.exit_code), "command": command});
        }
        return json!({"result": if trimmed.is_empty() { "(no output)".to_string() } else { trimmed }, "command": command});
    }

    // Fallback: one-shot
    let output = Command::new(if cfg!(windows) { "cmd.exe" } else { "bash" })
        .args(if cfg!(windows) { ["/C"].as_slice() } else { ["-c"].as_slice() })
        .arg(&command)
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) => {
            if let Some(ref m) = marker { record_bash_mutations(m, cwd); let _ = fs::remove_file(m); }
            let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let trimmed = trim_output(&sanitize_tool_output(&combined), 3000);
            if !out.status.success() {
                let code = out.status.code().unwrap_or(-1);
                // grep/rg exit 1 with no output = no matches found, not an error.
                if code == 1 && trimmed.is_empty() && is_grep_command(&command) {
                    return json!({"result": "(no matches found)", "command": command});
                }
                let with_hint = maybe_prepend_error_diagnosis(
                    ctx.config.features.error_diagnosis,
                    &command,
                    &trimmed,
                    code,
                )
                .await;
                json!({"result": with_hint, "error": format!("Exit code {}", code), "command": command})
            } else {
                json!({"result": if trimmed.is_empty() { "(no output)".to_string() } else { trimmed }, "command": command})
            }
        }
        Err(e) => {
            if let Some(ref m) = marker { let _ = fs::remove_file(m); }
            json!({"result": redact_string(&e.to_string()), "error": e.to_string(), "command": command})
        }
    }
}

/// True when `cmd` is a pure grep/rg invocation (exit 1 = no matches, not error).
fn is_grep_command(cmd: &str) -> bool {
    let t = cmd.trim();
    // Walk the last statement in the command, handling `|`, `&&`, and `;` chains.
    // Examples: "grep foo bar", "pdflatex ... | grep Overfull",
    //           "cd /app && grep -n pattern file", "make build; grep err log"
    let after_pipe = t.rsplit('|').next().unwrap_or(t).trim();
    let last_stmt = after_pipe.rsplit("&&").next().unwrap_or(after_pipe).trim();
    let last_stmt = last_stmt.rsplit(';').next().unwrap_or(last_stmt).trim();
    let first_word = last_stmt.split_whitespace().next().unwrap_or("");
    matches!(first_word, "grep" | "rg" | "egrep" | "fgrep")
}

/// Call the LLM error-diagnosis prompt and prepend a `[ERROR-DIAGNOSIS]`
/// block to `body` if a useful hint comes back. Falls back silently
/// when the feature is disabled or the LLM call errors.
async fn maybe_prepend_error_diagnosis(
    enabled: bool,
    command: &str,
    output: &str,
    exit_code: i32,
) -> String {
    if !enabled {
        return output.to_string();
    }
    let Some(diag) = crate::features_adapter::diagnose_error(command, output, exit_code).await else {
        return output.to_string();
    };
    let loc = match (&diag.file, diag.line) {
        (Some(f), Some(l)) => format!(" in {f}:{l}"),
        (Some(f), None) => format!(" in {f}"),
        _ => String::new(),
    };
    format!(
        "[ERROR-DIAGNOSIS] Type: {}{}. Fix: {}\n\n{}",
        diag.kind, loc, diag.suggestion, output
    )
}

fn rtk_rewrite(command: &str) -> String {
    if !crate::settings::get().rtk {
        return command.to_string();
    }
    if which::which("rtk").is_err() {
        return command.to_string();
    }
    let trimmed = command.trim_start();
    if trimmed.starts_with("rtk ") {
        return command.to_string();
    }
    let rewrites = [
        r"^git\s+(status|log|diff|add|commit|push|pull|fetch|branch|show)\b",
        r"^(cargo\s+test|jest|vitest|pytest|go\s+test|npm\s+test|yarn\s+test|pnpm\s+test|rake\s+test|rspec)\b",
        r"^(cargo\s+build|cargo\s+clippy|tsc\b|eslint|ruff\s+check|golangci-lint|rubocop)\b",
        r"^(ls|find\s|grep\s|rg\s)",
        r"^docker\s+(ps|images|logs|compose\s+ps)\b",
        r"^kubectl\s+(get\s+pods|logs|get\s+services)\b",
        r"^(npm\s+list|pnpm\s+list|yarn\s+list)\b",
    ];
    for pat in rewrites {
        if regex::Regex::new(pat).map(|r| r.is_match(trimmed)).unwrap_or(false) {
            return format!("rtk {trimmed}");
        }
    }
    command.to_string()
}

fn trim_output(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head_len = max.saturating_sub(500);
    let head = &s[..head_len.min(s.len())];
    let tail = &s[s.len().saturating_sub(300)..];
    format!("{head}\n...(truncated)...\n{tail}")
}

async fn exec_search(args: &Value, cwd: &Path) -> Value {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("path").and_then(|v| v.as_str()).map(|p| p.to_string());
    let safe_path: String = match path {
        Some(p) => match safe_resolve_path(&p, cwd, PathOptions::default()) {
            Ok(s) => s.full_path.to_string_lossy().into_owned(),
            Err(e) => return json!({"error": format!("search rejected: {e}")}),
        },
        None => ".".into(),
    };
    let cmd = format!(
        "{} {}",
        build_command("rg", &["--line-number", "--max-count", "10", "-C", "1"], &[pattern]),
        escape_shell_arg(&safe_path),
    );
    let out = run_shell(&cmd, cwd);
    json!({"result": truncate(&sanitize_tool_output(&out.unwrap_or_else(|_| "No matches found.".into())), 3000)})
}

async fn exec_find_files(args: &Value, cwd: &Path) -> Value {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    if pattern.is_empty() || pattern == "*" || pattern == "**" {
        let hint = args.get("hint").and_then(|v| v.as_str()).unwrap_or("");
        let listing = format_smart_listing(cwd, hint, 50);
        return json!({"result": listing});
    }
    let cmd = format!(
        "rg --files --glob {} --glob {} --glob {}",
        escape_shell_arg(pattern),
        escape_shell_arg("!node_modules"),
        escape_shell_arg("!.git"),
    );
    match run_shell(&cmd, cwd) {
        Ok(output) => {
            let files: Vec<&str> = output.lines().take(30).filter(|l| !l.is_empty()).collect();
            if files.is_empty() {
                json!({"result": "No files found."})
            } else {
                json!({"result": format!("Found {} files:\n{}", files.len(), files.join("\n"))})
            }
        }
        Err(_) => json!({"result": "No files found."}),
    }
}

async fn exec_list_projects(ctx: &ExecCtx<'_>, cwd: &Path) -> Value {
    // Native code graph: preferred path.
    if let Some(graph) = crate::code_graph::try_get_code_graph() {
        if let Ok(repos) = graph.list_repos() {
            if !repos.is_empty() {
                let mut out = format!("Workspace: {} indexed projects\n\n", repos.len());
                for r in &repos {
                    let langs = if r.languages.is_empty() {
                        "?".to_string()
                    } else {
                        r.languages.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
                    };
                    out.push_str(&format!(
                        "• {} — {} files, {} symbols, {}\n",
                        r.name, r.file_count, r.symbol_count, langs
                    ));
                }
                return json!({"result": out});
            }
        }
    }

    // Legacy MCP path: still honoured while the JS server is around.
    if let Some(bridge) = &ctx.mcp_bridge {
        if let Some(result) = bridge.call("tools/call", json!({"name": "list_repos", "arguments": {}})).await {
            if let Some(text) = result.pointer("/content/0/text").and_then(|t| t.as_str()) {
                if let Ok(data) = serde_json::from_str::<Value>(text) {
                    let repos = data.get("repos").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                    if repos.is_empty() {
                        return json!({"result": "No projects indexed yet. The code graph is empty."});
                    }
                    let mut out = format!("Workspace: {} indexed projects\n\n", repos.len());
                    for r in &repos {
                        out.push_str(&format!(
                            "• {} — {} files, {} symbols, {}\n",
                            r.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                            r.get("file_count").and_then(|v| v.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                            r.get("symbol_count").and_then(|v| v.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                            r.get("languages").and_then(|v| v.as_array()).map(|arr| arr.iter().filter_map(|v| v.as_str()).take(4).collect::<Vec<_>>().join(", ")).unwrap_or_else(|| "?".into()),
                        ));
                    }
                    return json!({"result": out});
                }
            }
        }
    }
    json!({"result": format!("Files in {}:\n{}", cwd.display(), format_smart_listing(cwd, "", 40))})
}

async fn exec_graph_search(args: &Value, ctx: &ExecCtx<'_>, cwd: &Path) -> Value {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let max_tokens = args.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4000);

    // Native code graph.
    if let Some(graph) = crate::code_graph::try_get_code_graph() {
        if let Ok(hits) = graph.search_graph(query, max_tokens as u32) {
            if !hits.is_empty() {
                let mut out = String::new();
                for h in &hits {
                    out.push_str(&format!("{} ({}, {}:{})\n", h.name, h.kind, h.file, h.line));
                    if let Some(sig) = &h.signature {
                        out.push_str(&format!("  {}\n", sig));
                    }
                    if let Some(snip) = &h.snippet {
                        out.push_str(snip);
                        if !snip.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    out.push('\n');
                }
                return json!({"result": truncate(&sanitize_tool_output(&out), 3000)});
            }
        }
    }

    if let Some(bridge) = &ctx.mcp_bridge {
        if let Some(result) = bridge.call("tools/call", json!({"name": "search_graph", "arguments": {"query": query, "max_tokens": max_tokens}})).await {
            if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
                let text: String = content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str()).map(String::from)).collect::<Vec<_>>().join("\n");
                return json!({"result": sanitize_tool_output(&text)});
            }
        }
    }
    let cmd = build_command("rg", &["--line-number", "--max-count", "5"], &[query]) + " .";
    let out = run_shell(&cmd, cwd).unwrap_or_else(|_| "No matches found in code graph or files.".into());
    json!({"result": truncate(&sanitize_tool_output(&out), 3000)})
}

async fn exec_explain_symbol(args: &Value, ctx: &ExecCtx<'_>, cwd: &Path) -> Value {
    let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");

    // Native code graph.
    if let Some(graph) = crate::code_graph::try_get_code_graph() {
        if let Ok(Some(exp)) = graph.explain_symbol(symbol) {
            let mut out = format!("{} ({}) at {}:{}\n", exp.name, exp.kind, exp.file, exp.line);
            if let Some(sig) = &exp.signature {
                out.push_str(&format!("Signature: {}\n", sig));
            }
            if exp.callers.is_empty() {
                out.push_str("Callers: (none)\n");
            } else {
                out.push_str("Callers:\n");
                for c in exp.callers.iter().take(10) {
                    out.push_str(&format!("  - {} ({}:{})\n", c.name, c.file, c.line));
                }
            }
            if exp.callees.is_empty() {
                out.push_str("Callees: (none)\n");
            } else {
                out.push_str("Callees:\n");
                for c in exp.callees.iter().take(10) {
                    out.push_str(&format!("  - {} ({}:{})\n", c.name, c.file, c.line));
                }
            }
            if let Some(snip) = &exp.snippet {
                out.push_str("Snippet:\n");
                out.push_str(snip);
                if !snip.ends_with('\n') {
                    out.push('\n');
                }
            }
            return json!({"result": sanitize_tool_output(&out)});
        }
    }

    if let Some(bridge) = &ctx.mcp_bridge {
        if let Some(result) = bridge.call("tools/call", json!({"name": "explain_symbol", "arguments": {"symbol": symbol}})).await {
            if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
                let text: String = content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str()).map(String::from)).collect::<Vec<_>>().join("\n");
                return json!({"result": sanitize_tool_output(&text)});
            }
        }
    }
    let sym_re = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_:.$-]*$").unwrap();
    if !sym_re.is_match(symbol) {
        return json!({"result": format!("Symbol \"{symbol}\" is not a valid identifier.")});
    }
    let cmd = format!("rg --line-number {} . --max-count 10", escape_shell_arg(&format!(r"\b{symbol}\b")));
    let out = run_shell(&cmd, cwd).unwrap_or_default();
    json!({"result": sanitize_tool_output(&format!("References to {symbol}:\n{}", truncate(&out, 2000)))})
}

async fn exec_read_and_patch(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "read_and_patch rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("read_and_patch rejected: {e}")}),
    };
    if !safe.full_path.exists() {
        return json!({"error": format!("File not found: {path}")});
    }
    let content = match fs::read_to_string(&safe.full_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
    let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
    let count = content.matches(old_str).count();
    if count == 0 {
        return json!({"error": format!("old_str not found in {path}")});
    }
    if count > 1 {
        return json!({"error": format!("old_str matches {count} locations. Be more specific.")});
    }
    let new_content = content.replacen(old_str, new_str, 1);
    if let Err(e) = fs::write(&safe.full_path, &new_content) {
        return json!({"error": e.to_string()});
    }
    let prefix = new_content.split(new_str).next().unwrap_or("");
    let line_num = prefix.split('\n').count();
    json!({"result": format!("Read and patched {path} at line {line_num}"), "action": "Edited", "path": path, "line": line_num})
}

async fn exec_create_and_run(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({"error": "create_and_run rejected: path missing"});
    };
    let safe = match safe_resolve_path(path, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("create_and_run rejected: {e}")}),
    };
    if let Some(dir) = safe.full_path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.len() > 8000 {
        return json!({"error": format!("create_and_run: content too large ({} lines). Use write_file (skeleton) + append_file (sections) + bash to run.", content.split('\n').count())});
    }
    if let Err(e) = fs::write(&safe.full_path, content) {
        return json!({"error": e.to_string()});
    }
    let lines = content.split('\n').count();
    let mut output = format!("Created {path} ({lines} lines)");
    let mut cmd_error = false;
    if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
        let interactive = content.contains("input(")
            || content.contains("readline")
            || content.contains("process.stdin")
            || content.contains("Scanner(")
            || content.contains("gets")
            || content.contains("read()");
        if interactive {
            output.push_str("\n⚠ File contains interactive input calls. Skipping execution.");
            return json!({"result": output, "action": "Created", "path": path, "lines": lines});
        }
        match run_shell(cmd, cwd) {
            Ok(out) => output.push_str(&format!("\n$ {cmd}\n{}", truncate(&out, 2000))),
            Err(e) => {
                cmd_error = true;
                output.push_str(&format!("\n$ {cmd}\nFAILED:\n{}", truncate(&e, 2000)));
            }
        }
    }
    if cmd_error {
        json!({"result": output, "action": "Created", "path": path, "lines": lines, "error": format!("Command failed")})
    } else {
        json!({"result": output, "action": "Created", "path": path, "lines": lines})
    }
}

async fn exec_find_and_read(args: &Value, cwd: &Path) -> Value {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let cmd = format!(
        "rg --files --glob {} --glob {} --glob {}",
        escape_shell_arg(pattern),
        escape_shell_arg("!node_modules"),
        escape_shell_arg("!.git"),
    );
    let found = match run_shell(&cmd, cwd) {
        Ok(s) => s,
        Err(_) => return json!({"result": format!("No files found matching: {pattern}")}),
    };
    let files: Vec<&str> = found.lines().filter(|l| !l.is_empty()).collect();
    if files.is_empty() {
        return json!({"result": format!("No files found matching: {pattern}")});
    }
    let target = files[0];
    let safe = match safe_resolve_path(target, cwd, PathOptions::default()) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("find_and_read rejected: {e}")}),
    };
    let content = match fs::read_to_string(&safe.full_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let max_lines = args.get("read_lines").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let lines: Vec<&str> = content.split('\n').take(max_lines).collect();
    let numbered: String = lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>4}| {}", i + 1, sanitize_tool_output(l)))
        .collect::<Vec<_>>()
        .join("\n");
    let mut output = format!(
        "Found {} files. Reading {target} ({} lines):\n{numbered}",
        files.len(),
        content.split('\n').count(),
    );
    if files.len() > 1 {
        output.push_str(&format!("\n\nOther matches: {}", files[1..5.min(files.len())].join(", ")));
    }
    json!({"result": output})
}

async fn exec_search_and_read(args: &Value, cwd: &Path) -> Value {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let read_ctx = args.get("read_context").and_then(|v| v.as_u64()).map(|n| n as usize).filter(|n| *n > 0 && *n < 200).unwrap_or(10);
    let cmd = format!(
        "{} . --glob {} --glob {}",
        build_command("rg", &["--line-number", "-C", &read_ctx.to_string(), "--max-count", "3"], &[pattern]),
        escape_shell_arg("!node_modules"),
        escape_shell_arg("!.git"),
    );
    let out = run_shell(&cmd, cwd).unwrap_or_default();
    let clean = sanitize_tool_output(&out);
    let result = if clean.trim().is_empty() {
        "No matches.".to_string()
    } else {
        truncate(&clean, 4000)
    };
    json!({"result": result})
}

async fn exec_run(args: &Value, cwd: &Path) -> Value {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return json!({"error": "run: command missing"});
    };
    let script_re = regex::Regex::new(r#"^(python3?|node|ruby)\s+["']?([^\s"']+)"#).unwrap();
    if let Some(caps) = script_re.captures(command) {
        let target = cwd.join(&caps[2]);
        if target.exists() {
            if let Ok(content) = fs::read_to_string(&target) {
                if content.contains("input(") || content.contains("readline") || content.contains("process.stdin") {
                    return json!({"result": format!("Refused: \"{command}\" — interactive input detected."), "error": "Interactive script", "command": command});
                }
            }
        }
    }
    let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
    match run_shell_with_timeout(command, cwd, std::time::Duration::from_secs(timeout)) {
        Ok(out) => json!({"result": truncate(&sanitize_tool_output(&out), 3000), "command": command}),
        Err((out, code)) => json!({"result": format!("EXIT {code}: {}", truncate(&out, 2500)), "error": format!("Exit {code}"), "command": command}),
    }
}

async fn exec_memory(name: &str, args: &Value, ctx: &ExecCtx<'_>) -> Value {
    let mut store = ctx.memory.lock();
    match name {
        "memory_load" => {
            let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let objects = store.load_for_task(task);
            if objects.is_empty() {
                return json!({"result": "No relevant memory found."});
            }
            let formatted: String = objects
                .iter()
                .map(|o| format!("[{}] {}: {}", o.kind, o.title, o.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            let tokens_used = objects.len() * 50;
            json!({"result": format!("Loaded {} memories ({} tokens):\n\n{}", objects.len(), tokens_used, formatted)})
        }
        "memory_remember" => {
            let kind = args.get("type").and_then(|v| v.as_str()).unwrap_or("context");
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let tags: Vec<String> = args.get("tags").and_then(|v| v.as_array()).map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            let obj = store.remember(kind, title, content, tags);
            json!({"result": format!("Remembered [{}] \"{}\" ({})", obj.kind, obj.title, obj.id)})
        }
        "memory_list" => {
            let kind = args.get("type").and_then(|v| v.as_str());
            let objects = match kind {
                Some(t) => store.by_type(t),
                None => store.all(),
            };
            if objects.is_empty() {
                return json!({"result": "No memory stored."});
            }
            let list = objects.iter().map(|o| format!("[{}] ({}) {}", o.id, o.kind, o.title)).collect::<Vec<_>>().join("\n");
            json!({"result": list})
        }
        "memory_forget" => {
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = store.forget(id);
            json!({"result": if ok { format!("Deleted {id}") } else { format!("Not found: {id}") }})
        }
        _ => json!({"result": ""}),
    }
}

async fn exec_web_search(args: &Value) -> Value {
    if !crate::settings::get().web_browse {
        return json!({"error": "Web browsing disabled. Set `[tools].web_browse = true` (or pass --web-browse)."});
    }
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    match web_search(query, 5).await {
        Ok(results) => {
            if results.is_empty() {
                json!({"result": "No results found."})
            } else {
                let formatted = results
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                json!({"result": formatted})
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn exec_web_fetch(args: &Value) -> Value {
    if !crate::settings::get().web_browse {
        return json!({"error": "Web browsing disabled. Set `[tools].web_browse = true` (or pass --web-browse)."});
    }
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
    match web_fetch(url, 5).await {
        Ok(content) => json!({"result": content}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ─── Bash write detection ───────────────────────────────────────────────────

/// Touch a temp marker file, run `f`, then use `find -newer marker` to discover
/// every file the bash command actually modified — regardless of how it did so.
/// Returns the marker path; caller must clean it up.
fn create_bash_marker() -> Option<PathBuf> {
    let p = std::env::temp_dir().join(format!(".itsy-watch-{}", std::process::id()));
    fs::write(&p, "").ok()?;
    Some(p)
}

/// After a bash command completes, find all files newer than `marker` under `cwd`
/// and mark them dirty in the read tracker so subsequent patch/write_file calls
/// know a re-read is required.
fn record_bash_mutations(marker: &Path, cwd: &Path) {
    let out = Command::new("find")
        .args([
            cwd.as_os_str(),
            std::ffi::OsStr::new("-newer"),
            marker.as_os_str(),
            std::ffi::OsStr::new("-not"),
            std::ffi::OsStr::new("-path"),
            std::ffi::OsStr::new("*/.git/*"),
            std::ffi::OsStr::new("-not"),
            std::ffi::OsStr::new("-path"),
            std::ffi::OsStr::new("*/node_modules/*"),
            std::ffi::OsStr::new("-type"),
            std::ffi::OsStr::new("f"),
        ])
        .output();
    if let Ok(out) = out {
        let tracker = get_read_tracker();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if !line.is_empty() {
                tracker.record_write(Path::new(line), cwd);
            }
        }
    }
}

// ─── Shell helpers ──────────────────────────────────────────────────────────

fn run_shell(cmd: &str, cwd: &Path) -> Result<String, String> {
    run_shell_with_timeout(cmd, cwd, std::time::Duration::from_secs(30)).map_err(|(out, _)| out)
}

fn run_shell_with_timeout(cmd: &str, cwd: &Path, _timeout: std::time::Duration) -> Result<String, (String, i32)> {
    let (program, args): (&str, Vec<&str>) = if cfg!(windows) { ("cmd.exe", vec!["/C", cmd]) } else { ("bash", vec!["-c", cmd]) };
    match Command::new(program).args(args).current_dir(cwd).output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            if out.status.success() {
                Ok(combined)
            } else {
                Err((combined, out.status.code().unwrap_or(-1)))
            }
        }
        Err(e) => Err((e.to_string(), -1)),
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
        s[..end].to_string()
    }
}

// ── contract tools ─────────────────────────────────────────────────────────

async fn exec_propose_contract(args: &Value, cwd: &Path, config: &Config) -> Value {
    use crate::session::contract;
    let title = match args.get("title").and_then(|v| v.as_str()) {
        Some(t) if !t.trim().is_empty() && t.len() <= 200 => t.to_string(),
        Some(_) => return json!({"error": "title must be 1-200 chars"}),
        None => return json!({"error": "missing `title`"}),
    };
    let brief = match args.get("brief").and_then(|v| v.as_str()) {
        Some(b) if b.trim().len() >= 20 => b.to_string(),
        Some(_) => {
            return json!({
                "error": "`brief` must be at least 20 characters — describe what you'll do and what done looks like"
            })
        }
        None => return json!({"error": "missing `brief`"}),
    };
    let assertions_val = args.get("assertions").and_then(|v| v.as_array());
    let Some(arr) = assertions_val else {
        return json!({"error": "missing `assertions` array (need at least one)"});
    };
    if arr.is_empty() {
        return json!({"error": "`assertions` must contain at least one item"});
    }
    if arr.len() > 24 {
        return json!({
            "error": "more than 24 assertions — too many to track. Group related ones or split the contract."
        });
    }
    let mut assertions: Vec<(String, String)> = Vec::with_capacity(arr.len());
    let mut seen_ids = std::collections::HashSet::new();
    for (i, a) in arr.iter().enumerate() {
        let id = match a.get("id").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return json!({"error": format!("assertions[{i}]: missing or empty `id`")}),
        };
        if !seen_ids.insert(id.clone()) {
            return json!({"error": format!("duplicate assertion id `{id}`")});
        }
        let text = match a.get("text").and_then(|v| v.as_str()) {
            Some(s) if s.trim().len() >= 5 && s.len() <= 200 => s.trim().to_string(),
            Some(_) => {
                return json!({
                    "error": format!("assertions[{i}].text must be 5-200 chars; keep it to one testable statement")
                })
            }
            _ => return json!({"error": format!("assertions[{i}]: missing `text`")}),
        };
        assertions.push((id, text));
    }
    let features = args
        .get("features")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let id = f.get("id").and_then(|v| v.as_str())?.to_string();
                    let description = f.get("description").and_then(|v| v.as_str())?.to_string();
                    let fulfills: Vec<String> = f
                        .get("fulfills")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((id, description, fulfills))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Dual-model negotiation: if a second-opinion model is configured, run an
    // independent assertion proposal + reconciliation loop before creating the
    // contract. Falls back to the main model's assertions on any failure.
    let (assertions, negotiated) =
        crate::features_adapter::negotiate_assertions(&brief, &title, assertions, config).await;

    if assertions.is_empty() {
        return json!({"error": "assertion negotiation produced an empty list — please retry"});
    }

    match contract::create(cwd, title.clone(), brief, assertions, features) {
        Ok(c) => {
            let note = if negotiated {
                " Assertions were negotiated between main and second-opinion models."
            } else {
                ""
            };
            json!({
                "result": format!(
                    "Contract `{}` created with {} assertions.{} \
                     Work the assertions, marking each with mark_assertion as you go. \
                     Call close_contract(\"completed\") when every assertion is non-pending.",
                    c.id,
                    c.assertions.len(),
                    note
                ),
                "contract_id": c.id,
                "title": c.title,
                "assertion_ids": c.assertions.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
                "negotiated": negotiated,
            })
        }
        Err(e) => json!({"error": format!("propose_contract: {e}")}),
    }
}

async fn exec_mark_assertion(args: &Value, cwd: &Path, config: &Config) -> Value {
    use crate::session::contract::{self, AssertionState, CommandEvidence};
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return json!({"error": "missing assertion `id`"});
    };
    let Some(state_s) = args.get("state").and_then(|v| v.as_str()) else {
        return json!({"error": "missing `state` (passed | failed)"});
    };
    let state = match state_s {
        "passed" => AssertionState::Passed,
        "failed" => AssertionState::Failed,
        "skipped" => return json!({"error": "`skipped` is not allowed — assertions must be passed or failed. If verification is genuinely impossible, keep trying or explain why in the `evidence` field and mark `failed`."}),
        _ => return json!({"error": "`state` must be `passed` or `failed`"}),
    };
    let evidence = match args.get("evidence").and_then(|v| v.as_str()) {
        Some(s) if s.trim().len() >= 10 => s.trim().to_string(),
        _ => {
            return json!({
                "error": "`evidence` must be at least 10 chars — describe how you verified"
            })
        }
    };
    // Optional command run details.
    let check = match (
        args.get("command").and_then(|v| v.as_str()),
        args.get("exit_code").and_then(|v| v.as_i64()),
        args.get("observation").and_then(|v| v.as_str()),
    ) {
        (Some(cmd), Some(ec), Some(obs)) if !cmd.is_empty() && obs.len() >= 5 => {
            let lower = obs.trim().to_lowercase();
            if matches!(
                lower.as_str(),
                "ok" | "passed" | "passes" | "done" | "good" | "success" | "tests passed"
            ) {
                return json!({
                    "error": format!(
                        "observation \"{obs}\" is too vague — write the specific output you saw"
                    )
                });
            }
            Some(CommandEvidence {
                command: cmd.to_string(),
                exit_code: ec,
                observation: obs.to_string(),
                timestamp: contract::now_iso(),
            })
        }
        _ => None,
    };
    // For `passed`, we want at least *some* anchored evidence. Either
    // a command result or a meaty evidence string (≥30 chars).
    if state == AssertionState::Passed && check.is_none() && evidence.len() < 30 {
        return json!({
            "error": "marking `passed` without a verification command requires substantive evidence (≥30 chars). \
                     Re-run the check and include command + observation."
        });
    }

    // Second opinion: before accepting "passed", ask the second model to
    // independently verify the evidence. Catches the model lying to itself.
    if state == AssertionState::Passed {
        let assertion_text = contract::current()
            .and_then(|c| c.assertions.iter().find(|a| a.id == id).map(|a| a.text.clone()))
            .unwrap_or_default();
        if !assertion_text.is_empty() {
            let cmd_str = check.as_ref().map(|c| c.command.as_str());
            let ec = check.as_ref().map(|c| c.exit_code);
            let obs_str = check.as_ref().map(|c| c.observation.as_str());
            if let Some(reason) = crate::features_adapter::verify_assertion_passed(
                &assertion_text,
                &evidence,
                cmd_str,
                ec,
                obs_str,
                config,
            )
            .await
            {
                return json!({
                    "error": format!(
                        "[SECOND OPINION] Assertion {id} may not be passed: {reason}. \
                         Re-verify with a concrete command and clear output before marking passed."
                    )
                });
            }
        }
    }

    match contract::mark_assertion(cwd, id, state, evidence, check) {
        Ok(c) => {
            let counts = c.counts();
            json!({
                "result": format!(
                    "Marked {id} as {} ({} passed / {} failed / {} pending).",
                    state.as_str(),
                    counts.passed,
                    counts.failed,
                    counts.pending,
                ),
                "remaining_pending": counts.pending,
            })
        }
        Err(e) => json!({"error": format!("mark_assertion: {e}")}),
    }
}

async fn exec_mark_feature(args: &Value, cwd: &Path) -> Value {
    use crate::session::contract::{self, FeatureState};
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return json!({"error": "missing feature `id`"});
    };
    let Some(state_s) = args.get("state").and_then(|v| v.as_str()) else {
        return json!({"error": "missing `state` (in_progress | done | cancelled)"});
    };
    let state = match state_s {
        "in_progress" => FeatureState::InProgress,
        "done" => FeatureState::Done,
        "cancelled" => FeatureState::Cancelled,
        _ => return json!({"error": "`state` must be in_progress | done | cancelled"}),
    };
    match contract::mark_feature(cwd, id, state) {
        Ok(_) => json!({"result": format!("Feature {id} marked {}", state.as_str())}),
        Err(e) => json!({"error": format!("mark_feature: {e}")}),
    }
}

async fn exec_contract_status() -> Value {
    use crate::session::contract;
    match contract::current() {
        Some(c) => json!({
            "result": contract::render_for_prompt(&c),
            "contract_id": c.id,
            "status": c.status.as_str(),
            "counts": {
                "total": c.counts().total,
                "passed": c.counts().passed,
                "failed": c.counts().failed,
                "pending": c.counts().pending,
                "skipped": c.counts().skipped,
            },
        }),
        None => json!({"result": "(no active contract)"}),
    }
}

async fn exec_close_contract(args: &Value, cwd: &Path, config: &Config) -> Value {
    use crate::session::contract::{self, AssertionState, ContractStatus};
    let Some(status_s) = args.get("status").and_then(|v| v.as_str()) else {
        return json!({"error": "missing `status` — only `completed` is accepted"});
    };
    let status = match status_s {
        "completed" => ContractStatus::Completed,
        "aborted" => return json!({"error": "`aborted` is not available — fix failing assertions and close as `completed`"}),
        _ => return json!({"error": "`status` must be `completed`"}),
    };
    // Second opinion: final gate — ask the second model to review all passed
    // assertions together before allowing the contract to close.
    if let Some(c) = contract::current() {
        let passed: Vec<(String, String, String)> = c
            .assertions
            .iter()
            .filter(|a| a.state == AssertionState::Passed)
            .map(|a| (a.id.clone(), a.text.clone(), a.evidence.clone().unwrap_or_default()))
            .collect();
        if !passed.is_empty() {
            if let Some(disputed) =
                crate::features_adapter::verify_contract_complete(&c.brief, &passed, config).await
            {
                return json!({
                    "error": format!(
                        "[SECOND OPINION] Final review doubts these assertions are truly satisfied: {}. \
                         Re-verify them before closing.",
                        disputed.join(", ")
                    )
                });
            }
        }
    }
    match contract::close(cwd, status) {
        Ok(c) => {
            let counts = c.counts();
            json!({
                "result": format!(
                    "Contract `{}` closed as {} — {} passed / {} failed / {} skipped of {}.",
                    c.id, c.status.as_str(), counts.passed, counts.failed, counts.skipped, counts.total,
                ),
            })
        }
        Err(e) => json!({"error": format!("close_contract: {e}")}),
    }
}
