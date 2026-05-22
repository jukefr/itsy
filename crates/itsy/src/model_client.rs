//! Talks to OpenAI-compatible endpoints for
//! chat completions (non-streaming) and streaming final responses, plus the
//! per-file `runValidation` helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
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
    // If the thinking budget is bigger than max_tokens, the model burns
    // its whole output budget on thinking and emits empty content —
    // the classic IQ2_XXS "empty response" failure mode. Give the
    // content at least 4k headroom past the thinking budget, and
    // honour ITSY_MAX_OUTPUT_TOKENS for explicit overrides.
    let max_tokens = std::env::var("ITSY_MAX_OUTPUT_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
        .max(tokens.saturating_add(4096))
        .max(4096);

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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(ctx.config.model.timeout))
        .build()
        .ok()?;

    let url = format!("{}/chat/completions", ctx.config.model.base_url);
    let res = match client.post(&url).headers(build_auth_headers(ctx.config)).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  \x1b[31m✗ {e}\x1b[0m");
            return None;
        }
    };

    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        if status >= 400 && status < 500 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(retry) = client.post(&url).headers(build_auth_headers(ctx.config)).json(&body).send().await {
                if retry.status().is_success() {
                    return retry.json().await.ok();
                }
            }
        }
        let redacted = crate::security::redact_string(&text);
        eprintln!(
            "  \x1b[31m✗ API error {status}: {}\x1b[0m",
            &redacted[..redacted.len().min(200)]
        );
        return None;
    }

    res.json().await.ok()
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
