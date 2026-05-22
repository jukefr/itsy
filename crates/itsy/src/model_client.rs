//! Talks to OpenAI-compatible endpoints for
//! chat completions (non-streaming) and streaming final responses, plus the
//! per-file `runValidation` helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::build_auth_headers;
use crate::governor::early_stop::EarlyStopDetector;
use crate::session::images::{extract_images, format_images_for_api, model_supports_vision};
use crate::Config;

pub struct ChatContext<'a> {
    pub config: &'a Config,
    pub conversation: &'a [Value],
    pub tools: Vec<Value>,
    pub current_task_type: Option<&'a str>,
    pub system_prompt: String,
}

/// Make a chat completion request (non-streaming, for tool use).
pub async fn chat_completion(ctx: &ChatContext<'_>) -> Option<Value> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let processed: Vec<Value> = ctx
        .conversation
        .iter()
        .map(|msg| {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "user" {
                return msg.clone();
            }
            let Some(content) = msg.get("content").and_then(|c| c.as_str()) else {
                return msg.clone();
            };
            let images = extract_images(content, &cwd);
            if images.is_empty() || !model_supports_vision(&ctx.config.model.name) {
                return msg.clone();
            }
            let mut parts = vec![json!({"type": "text", "text": content})];
            parts.extend(format_images_for_api(&images));
            json!({"role": "user", "content": parts})
        })
        .collect();

    let mut messages = vec![json!({"role": "system", "content": ctx.system_prompt})];
    messages.extend(processed);

    // Adaptive temperature: bumps with repair history if enabled.
    let temperature = if ctx.config.features.temp_adapt {
        let task = ctx.current_task_type.unwrap_or("coding");
        crate::model::adaptive_temp::adaptive_temperature(task, 0)
    } else {
        0.1
    };

    // Provider-gated reasoning fields (Anthropic `thinking`, OpenAI
    // `reasoning_effort`, Qwen / llama.cpp `chat_template_kwargs`).
    let task = ctx.current_task_type.unwrap_or("coding");
    let tokens = crate::model::thinking_budget::thinking_budget(task, 0);

    // max_tokens is a hard cap on TOTAL output (thinking + content).
    // We start small for speed, then double on detected overflow —
    // see the retry loop below. The explicit `max_output_tokens`
    // setting forces a single fixed cap if set (>0).
    let explicit_cap = crate::settings::get().max_output_tokens;
    let initial_max_tokens = if explicit_cap > 0 {
        explicit_cap
    } else {
        // Start with thinking headroom + small content slack. Most
        // turns end well below this, especially short tool-call
        // exchanges. The retry loop expands it when the model
        // actually runs out of room.
        tokens.saturating_add(1024).max(4096)
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(ctx.config.model.timeout))
        .build()
        .ok()?;

    let url = format!("{}/chat/completions", ctx.config.model.base_url);

    // Adaptive max_tokens: most turns finish within the small cap, so
    // we save wall-clock there. When the response comes back empty
    // because the model exhausted its budget mid-think (finish_reason
    // == "length" with no content + no tool calls), we double the cap
    // and re-issue the same request. Hard ceiling at ~32k so we never
    // spend forever on a single call.
    let absolute_max = explicit_cap.max(32768);
    let mut max_tokens = initial_max_tokens.min(absolute_max);
    let mut attempt: u32 = 0;
    let max_attempts: u32 = if explicit_cap > 0 { 1 } else { 3 };

    loop {
        attempt += 1;

        let mut body = json!({
            "model": ctx.config.model.name,
            "messages": messages,
            "tools": ctx.tools,
            "temperature": temperature,
            "max_tokens": max_tokens,
        });
        crate::model::thinking_budget::apply_thinking_budget(
            &mut body,
            &ctx.config.model.base_url,
            tokens,
            /* disable = */ false,
        );

        let res = match client
            .post(&url)
            .headers(build_auth_headers(ctx.config))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  \x1b[31m✗ {e}\x1b[0m");
                return None;
            }
        };

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            if status >= 400 && status < 500 && attempt == 1 {
                // One transient-error retry on 4xx, mirroring the
                // pre-adaptive behaviour.
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            let redacted = crate::security::redact_string(&text);
            eprintln!(
                "  \x1b[31m✗ API error {status}: {}\x1b[0m",
                &redacted[..redacted.len().min(200)]
            );
            return None;
        }

        let parsed: Option<Value> = res.json().await.ok();
        let Some(value) = parsed else { return None };

        // Persist the raw request + response (best-effort; never
        // blocks the agent loop). This is the only place we can see
        // `reasoning_content` / `<think>` blocks / finish_reason /
        // token usage verbatim — the rest of the code only looks at
        // the parsed-out content + tool_calls.
        crate::model::chat_log::record(&body, &value, attempt);

        // Was this an overflow? Empty content + no tool calls +
        // finish_reason="length" means the model ran out of room
        // before producing anything useful. Bump and retry.
        if attempt < max_attempts && looks_like_budget_overflow(&value) {
            let next = max_tokens.saturating_mul(2).min(absolute_max);
            if next > max_tokens {
                eprintln!(
                    "  \x1b[90m… token budget overflow at {max_tokens} → retrying at {next}\x1b[0m"
                );
                max_tokens = next;
                continue;
            }
        }
        return Some(value);
    }
}

/// Heuristic: does this chat-completion response look like the model
/// hit the `max_tokens` cap mid-thinking? Three signals together:
///   - `finish_reason` == `"length"`
///   - empty `content` (none, or only whitespace)
///   - no `tool_calls` array (or empty)
/// All three are needed because a "length"-truncated long reply WITH
/// content is still useful — we don't want to discard those.
fn looks_like_budget_overflow(value: &Value) -> bool {
    let Some(choice) = value.pointer("/choices/0") else { return false };
    let finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if finish != "length" {
        return false;
    }
    let content_empty = choice
        .pointer("/message/content")
        .map(|c| c.as_str().map(|s| s.trim().is_empty()).unwrap_or(true))
        .unwrap_or(true);
    let no_tools = choice
        .pointer("/message/tool_calls")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    content_empty && no_tools
}

/// Stream a final text response (no tools, just summarize).
pub async fn stream_final_response(
    config: &Config,
    conversation: &[Value],
    early_stop: Option<&mut EarlyStopDetector>,
    mut on_token: impl FnMut(&str),
) -> Option<String> {
    let system = json!({"role": "system", "content": "You are itsy, a coding assistant. Summarize what you just did in 1-2 sentences. Be concise."});
    let mut messages = vec![system];
    let start = conversation.len().saturating_sub(6);
    messages.extend(conversation[start..].iter().cloned());

    let body = json!({
        "model": config.model.name,
        "messages": messages,
        "stream": true,
        "temperature": 0.1,
        "max_tokens": 256,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.model.timeout))
        .build()
        .ok()?;
    let url = format!("{}/chat/completions", config.model.base_url);
    let res = client.post(&url).headers(build_auth_headers(config)).json(&body).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let mut stream = res.bytes_stream();
    let mut buffer = String::new();
    let mut full = String::new();
    let mut early_stop = early_stop;
    while let Some(Ok(bytes)) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        loop {
            let Some(nl) = buffer.find('\n') else { break };
            let line = buffer[..nl].to_string();
            buffer.drain(..=nl);
            let line = line.trim();
            if !line.starts_with("data: ") {
                continue;
            }
            let payload = &line[6..];
            if payload == "[DONE]" {
                return Some(full);
            }
            let Ok(chunk) = serde_json::from_str::<Value>(payload) else { continue };
            if let Some(delta) = chunk.pointer("/choices/0/delta/content").and_then(|v| v.as_str()) {
                on_token(delta);
                full.push_str(delta);
                if let Some(es) = early_stop.as_deref_mut() {
                    if let Some(_signal) = es.check_repetition(&full) {
                        return Some(full);
                    }
                }
            }
        }
    }
    Some(full)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ValidationOutcome {
    pub passed: bool,
    pub errors: Vec<String>,
}

/// Mirror of `runValidation` — per-language compile/lint checks.
pub fn run_validation(file_path: &str) -> Option<ValidationOutcome> {
    let cwd = std::env::current_dir().ok()?;
    let ext = Path::new(file_path).extension().and_then(|s| s.to_str()).unwrap_or("");
    let p = cwd.join(file_path);

    let run_args = |cmd: &str, args: &[&str], parser: fn(&str, &str) -> Vec<String>| -> ValidationOutcome {
        match Command::new(cmd).args(args).current_dir(&cwd).output() {
            Ok(out) => {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if out.status.success() {
                    ValidationOutcome { passed: true, errors: Vec::new() }
                } else {
                    let errors = parser(&combined, file_path);
                    if errors.is_empty() {
                        ValidationOutcome { passed: true, errors: Vec::new() }
                    } else {
                        ValidationOutcome { passed: false, errors }
                    }
                }
            }
            Err(e) => ValidationOutcome { passed: false, errors: vec![e.to_string()] },
        }
    };

    if (ext == "ts" || ext == "tsx") && cwd.join("tsconfig.json").exists() {
        return Some(run_args("npx", &["tsc", "--noEmit", "--pretty", "false"], |out, fp| {
            out.lines()
                .filter(|l| l.contains(fp) && l.contains("error"))
                .take(5)
                .map(String::from)
                .collect()
        }));
    }
    if ext == "py" {
        return Some(run_args("python", &["-m", "py_compile", file_path], |out, _| {
            if out.trim().is_empty() {
                Vec::new()
            } else {
                vec![out.trim().to_string()]
            }
        }));
    }
    if ext == "rs" && cwd.join("Cargo.toml").exists() {
        return Some(run_args("cargo", &["check", "--message-format", "short"], |out, _| {
            out.lines().filter(|l| l.starts_with("error")).take(5).map(String::from).collect()
        }));
    }
    if ext == "go" && cwd.join("go.mod").exists() {
        return Some(run_args("go", &["build", "./..."], |out, fp| {
            out.lines().filter(|l| l.contains(fp)).take(5).map(String::from).collect()
        }));
    }
    if ext == "js" || ext == "mjs" {
        return Some(run_args("node", &["--check", file_path], |out, _| {
            if out.trim().is_empty() {
                Vec::new()
            } else {
                vec![out.trim().to_string()]
            }
        }));
    }
    if ext == "json" {
        let content = fs::read_to_string(&p).ok()?;
        return Some(match serde_json::from_str::<Value>(&content) {
            Ok(_) => ValidationOutcome { passed: true, errors: Vec::new() },
            Err(e) => ValidationOutcome { passed: false, errors: vec![e.to_string()] },
        });
    }
    None
}

/// Build the canonical system prompt used by [`chat_completion`].
pub fn build_system_prompt(
    config: &Config,
    memory_context: &str,
    skill_context: &str,
    plugin_context: &str,
    current_task_type: Option<&str>,
) -> String {
    let os = if cfg!(windows) { "Windows (cmd.exe shell)" } else if cfg!(target_os = "macos") { "macOS (zsh)" } else { "Linux (bash)" };
    let win_extra = if cfg!(windows) {
        "- Use \"dir\" not \"ls\", \"type\" not \"cat\", \"del\" not \"rm\"\n- Do NOT use bash-specific commands (touch, export, chmod)"
    } else {
        ""
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let mut prompt = format!(
        "You are itsy, a coding assistant that operates in the user's project directory.\n\n\
         You have tools to read, write, and edit files, run shell commands, and search code.\n\
         You also have project memory and compound tools that do multiple operations in one call.\n\
         You have a CODE GRAPH indexed for this project — use it for understanding questions.\n\n\
         IMPORTANT — Code Graph (use these FIRST for understanding/analysis questions):\n\
         - list_projects: Lists ALL projects in the workspace with stats. Use FIRST when asked \"what projects are here\".\n\
         - graph_search: Search for a specific symbol/function/class in the graph.\n\
         - explain_symbol: Get full explanation of a function/class.\n\
         - memory_load: Load relevant project memory.\n\n\
         IMPORTANT — Environment:\n\
         - OS: {os}\n\
         {win_extra}\n\n\
         Rules:\n\
         - PREFER compound tools to reduce back-and-forth.\n\
         - Use \"patch\" for edits. Do NOT rewrite whole files.\n\
         - Be concise — show what you did, not lengthy explanations.\n\
         - If a tool fails, explain what went wrong. Do NOT output a greeting.\n\
         - Create files with write_file directly. Do NOT run mkdir first."
    );

    let _ = current_task_type;
    prompt.push_str(&format!("\nWorking directory: {cwd}"));
    prompt.push_str(memory_context);
    prompt.push_str(skill_context);
    prompt.push_str(plugin_context);
    // tie current model/base to the prompt so future readers know the binding
    if !config.model.name.is_empty() {
        prompt.push_str(&format!("\nModel: {}", config.model.name));
    }
    prompt
}

#[cfg(test)]
mod budget_overflow_tests {
    use super::looks_like_budget_overflow;
    use serde_json::json;

    #[test]
    fn empty_content_no_tools_finish_length_is_overflow() {
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"role": "assistant", "content": ""}
            }]
        });
        assert!(looks_like_budget_overflow(&v));
    }

    #[test]
    fn whitespace_content_counts_as_empty() {
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"role": "assistant", "content": "  \n  "}
            }]
        });
        assert!(looks_like_budget_overflow(&v));
    }

    #[test]
    fn null_content_counts_as_empty() {
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"role": "assistant", "content": null}
            }]
        });
        assert!(looks_like_budget_overflow(&v));
    }

    #[test]
    fn length_with_partial_content_is_not_overflow() {
        // We do NOT discard a partial-but-useful reply that hit max_tokens.
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"role": "assistant", "content": "Half a thought…"}
            }]
        });
        assert!(!looks_like_budget_overflow(&v));
    }

    #[test]
    fn tool_calls_with_empty_content_is_not_overflow() {
        // The model produced a real tool call — keep it.
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "x", "type": "function",
                                    "function": {"name": "bash", "arguments": "{}"}}]
                }
            }]
        });
        assert!(!looks_like_budget_overflow(&v));
    }

    #[test]
    fn stop_with_empty_is_not_overflow() {
        // Empty body but finish_reason=stop — model genuinely gave up
        // (and our existing empty-response retry path handles that).
        let v = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": ""}
            }]
        });
        assert!(!looks_like_budget_overflow(&v));
    }
}
