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

use crate::config::build_auth_headers_for;
use crate::governor::early_stop::EarlyStopDetector;
use crate::session::images::{extract_images, format_images_for_api, model_supports_vision};
use crate::Config;

pub struct ChatContext<'a> {
    pub model_name: &'a str,
    pub base_url: &'a str,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub temp_adapt: bool,
    pub conversation: &'a [Value],
    pub tools: Vec<Value>,
    pub current_task_type: Option<&'a str>,
    pub system_prompt: String,
    pub force_disable_thinking: bool,
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
            if images.is_empty() || !model_supports_vision(ctx.model_name) {
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
    let temperature = if ctx.temp_adapt {
        let task = ctx.current_task_type.unwrap_or("coding");
        crate::model::adaptive_temp::adaptive_temperature(task, 0)
    } else {
        0.1
    };

    // Provider-gated reasoning fields (Anthropic `thinking`, OpenAI
    // `reasoning_effort`, Qwen / llama.cpp `chat_template_kwargs`).
    let task = ctx.current_task_type.unwrap_or("coding");
    let tokens = crate::model::thinking_budget::thinking_budget(task, 0);

    // max_tokens. Upstream uses a fixed 4096, but that's not enough
    // headroom for thinking models. With a thinking_budget of N, the
    // model can spend up to N tokens on reasoning; we need additional
    // room for the actual response (tool call args or text). +4096 gives
    // enough room for a write_file with a 200-line file (~3000 tokens).
    // +1024 was too tight — bench runs with --thinking-budget=8000 would
    // hit finish_reason:length mid-thinking, producing zero tool calls.
    // Non-thinking models: tokens=0 → max(0+4096, 4096) = 4096 (same as
    // upstream's fixed value).
    //
    // INTENTIONAL deviation from upstream: necessary for thinking
    // models like Qwen3; without it, the model overflows its budget
    // mid-thinking and returns empty content.
    let explicit_cap = crate::settings::get().max_output_tokens;
    let max_tokens = if explicit_cap > 0 {
        explicit_cap
    } else {
        tokens.saturating_add(4096).max(4096)
    };

    let client = reqwest::Client::builder()
        .timeout(ctx.timeout)
        .build()
        .ok()?;

    let url = format!("{}/chat/completions", ctx.base_url);

    let mut body = json!({
        "model": ctx.model_name,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });
    // INTENTIONAL: only include `tools` when the list is non-empty.
    // Qwen3 via llama-server interprets `"tools": []` as "tool use
    // enabled with no tools", which still triggers the tool-call
    // path in the chat template and causes the model to emit
    // `<tool_call>` XML even for conversational turns. Omitting
    // the key entirely disables tool use for those turns (e.g.
    // the `respond` routing category that handles greetings).
    if !ctx.tools.is_empty() {
        body["tools"] = json!(ctx.tools);
    }
    // INTENTIONAL: apply provider-gated reasoning fields (Anthropic
    // `thinking`, OpenAI `reasoning_effort`, llama.cpp
    // `chat_template_kwargs`). Required for Qwen3-style thinking
    // models; upstream JS doesn't have this because its target
    // providers (Anthropic/OpenAI) handle it differently.
    crate::model::thinking_budget::apply_thinking_budget(
        &mut body,
        ctx.base_url,
        tokens,
        /* disable = */ ctx.force_disable_thinking,
    );

    // Mirrors upstream: one POST, one transient-error retry on 4xx.
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let res = match client
            .post(&url)
            .headers(build_auth_headers_for(ctx.api_key.as_deref(), ctx.base_url))
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
            if (400..500).contains(&status) && attempt == 1 {
                // One transient-error retry on 4xx, matches upstream.
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

        let mut value: Value = res.json().await.ok()?;

        // INTENTIONAL: persist raw request + response for inspection.
        // No behaviour change; just observability.
        crate::model::chat_log::record(&body, &value, attempt);

        // If the model was cut off mid-thinking (finish_reason=length),
        // retry once with thinking disabled so it gets a real response out.
        // This happens when max_tokens is tight relative to the thinking budget.
        let finish_reason = value
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if finish_reason == "length" && tokens > 0 && attempt == 1 {
            eprintln!(
                "  \x1b[33m⚠ finish_reason=length with thinking enabled — \
                 retrying with thinking disabled\x1b[0m"
            );
            crate::model::thinking_budget::apply_thinking_budget(
                &mut body,
                ctx.base_url,
                tokens,
                /* disable = */ true,
            );
            continue;
        }

        // Log thinking token usage so we can verify the budget cap is honored.
        // llama-server puts thinking in reasoning_content (not completion_tokens_details).
        {
            let completion = value
                .pointer("/usage/completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let thinking_chars = value
                .pointer("/choices/0/message/reasoning_content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            if thinking_chars > 0 {
                let thinking_est = thinking_chars / 4; // rough chars-to-tokens estimate
                eprintln!(
                    "  \x1b[90m[tokens] completion={completion} thinking~{thinking_est} ({thinking_chars}chars)\x1b[0m"
                );
            }
        }

        // INTENTIONAL: Qwen3 models served via llama-server sometimes emit
        // tool calls as raw `<tool_call>` XML in the content field instead of
        // structured `tool_calls`. When that happens llama-server's OpenAI
        // adapter doesn't convert them. We normalise here as a fallback so
        // the rest of the harness never sees the raw XML.
        //
        // Also strip stray `<think>…</think>` / `</think>` fragments that
        // occasionally leak into the content field when the model interleaves
        // reasoning and response text.
        if let Some(msg) = value.pointer_mut("/choices/0/message") {
            extract_xml_tool_calls(msg);
            strip_think_tags(msg);
        }

        return Some(value);
    }
}

/// Stream a final text response (no tools, just summarize).
pub async fn stream_final_response(
    model_name: &str,
    base_url: &str,
    api_key: Option<&str>,
    timeout_secs: u64,
    conversation: &[Value],
    early_stop: Option<&mut EarlyStopDetector>,
    mut on_token: impl FnMut(&str),
) -> Option<String> {
    let system = json!({"role": "system", "content": "You are itsy, a coding assistant. Summarize what you just did in 1-2 sentences. Be concise."});
    let mut messages = vec![system];
    let start = conversation.len().saturating_sub(6);
    messages.extend(conversation[start..].iter().cloned());

    let body = json!({
        "model": model_name,
        "messages": messages,
        "stream": true,
        "temperature": 0.1,
        "max_tokens": 256,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .ok()?;
    let url = format!("{}/chat/completions", base_url);
    let res = client
        .post(&url)
        .headers(build_auth_headers_for(api_key, base_url))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let mut stream = res.bytes_stream();
    let mut buffer = String::new();
    let mut full = String::new();
    let mut early_stop = early_stop;
    while let Some(Ok(bytes)) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(nl) = buffer.find('\n') {
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
    // Mirror JS: reject hostile filePaths with embedded null bytes early.
    if file_path.contains('\0') {
        return Some(ValidationOutcome { passed: false, errors: vec!["invalid filePath".into()] });
    }
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

/// Strip Qwen3-style `<think>…</think>` fragments from the content field.
///
/// llama-server normally moves thinking into `reasoning_content`, but
/// occasionally a stray closing `</think>` or a full `<think>…</think>` block
/// leaks into content. Remove them so the TUI never shows raw reasoning XML.
fn strip_think_tags(msg: &mut Value) {
    let content = match msg.get("content").and_then(|v| v.as_str()) {
        Some(c) if c.contains("</think>") || c.contains("<think>") => c.to_string(),
        _ => return,
    };

    // Strip full <think>…</think> blocks first, then any orphaned closing tag.
    let mut s = content.clone();
    loop {
        if let Some(a) = s.find("<think>") {
            if let Some(rel) = s[a..].find("</think>") {
                s.drain(a..a + rel + "</think>".len());
                continue;
            } else {
                s.drain(a..);
                break;
            }
        }
        // No more open tags — remove orphaned </think> if present.
        while let Some(pos) = s.find("</think>") {
            s.drain(pos..pos + "</think>".len());
        }
        break;
    }

    let clean = s.trim().to_string();
    if clean == content.trim() {
        return;
    }
    if let Some(obj) = msg.as_object_mut() {
        if clean.is_empty() {
            obj.remove("content");
        } else {
            obj.insert("content".into(), Value::String(clean));
        }
    }
}

/// Normalise Qwen3-style `<tool_call>` XML that leaks into the content field
/// when llama-server's OpenAI adapter doesn't convert it. Handles two formats:
///
///   - JSON:  `<tool_call>\n{"name":"fn","arguments":{...}}\n</tool_call>`
///   - Attr:  `<tool_call><function=fn>\n<parameter=k>v</parameter>\n</function></tool_call>`
///
/// Also strips `<tool_call>…</tool_call>` blocks from `content` so they
/// never leak to the TUI.  Only populates `tool_calls` on the message when
/// the message doesn't already carry structured tool calls from the server —
/// that way we never duplicate a call that was already parsed upstream.
///
/// Always strips `<tool_call>…</tool_call>` blocks from `content` so they
/// never leak to the TUI.  Only populates `tool_calls` on the message when
/// the message doesn't already carry structured tool calls from the server —
/// that way we never duplicate a call that was already parsed upstream.
fn extract_xml_tool_calls(msg: &mut Value) {
    let content = match msg.get("content").and_then(|v| v.as_str()) {
        Some(c) if c.contains("<tool_call>") => c.to_string(),
        _ => return,
    };

    // When the server already provided structured tool_calls we still need to
    // strip the XML from content, but we must not add duplicate entries.
    let strip_only = msg
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tc_id: u32 = 0;
    let mut remaining = content.as_str();

    while let Some(start) = remaining.find("<tool_call>") {
        remaining = &remaining[start + "<tool_call>".len()..];
        let end = remaining.find("</tool_call>").unwrap_or(remaining.len());
        let inner = remaining[..end].trim();
        remaining = if end < remaining.len() {
            &remaining[end + "</tool_call>".len()..]
        } else {
            ""
        };

        if strip_only {
            // Don't bother parsing — we only need the stripping pass below.
            continue;
        }

        tc_id += 1;
        let id = format!("call_{tc_id}");

        // JSON format: {"name":"fn","arguments":{...}}
        if let Ok(obj) = serde_json::from_str::<Value>(inner) {
            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                let args = obj.get("arguments").cloned().unwrap_or(json!({}));
                let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args_str}
                }));
                continue;
            }
        }

        // Attribute format: <function=NAME>\n<parameter=K>V</parameter>…\n</function>
        if let Some(fn_off) = inner.find("<function=") {
            let after = &inner[fn_off + "<function=".len()..];
            let name_end = after.find('>').unwrap_or(after.len());
            let fn_name = after[..name_end].trim().trim_end_matches('/');
            let fn_body = if name_end < after.len() { &after[name_end + 1..] } else { "" };

            let mut params = serde_json::Map::new();
            let mut p = fn_body;
            while let Some(pp) = p.find("<parameter=") {
                p = &p[pp + "<parameter=".len()..];
                let ke = p.find('>').unwrap_or(p.len());
                let key = p[..ke].trim();
                p = if ke < p.len() { &p[ke + 1..] } else { "" };
                let ve = p.find("</parameter>").unwrap_or(p.len());
                let val = p[..ve].trim();
                p = if ve < p.len() { &p[ve + "</parameter>".len()..] } else { "" };
                params.insert(key.to_string(), Value::String(val.to_string()));
            }

            if !fn_name.is_empty() {
                let args_str = serde_json::to_string(&Value::Object(params))
                    .unwrap_or_else(|_| "{}".into());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": fn_name, "arguments": args_str}
                }));
            }
        }
    }

    // Always strip <tool_call>…</tool_call> blocks from content.
    let clean = {
        let mut s = content.clone();
        while let Some(a) = s.find("<tool_call>") {
            if let Some(rel) = s[a..].find("</tool_call>") {
                s.drain(a..a + rel + "</tool_call>".len());
            } else {
                s.drain(a..);
                break;
            }
        }
        s.trim().to_string()
    };

    if let Some(obj) = msg.as_object_mut() {
        if !strip_only && !tool_calls.is_empty() {
            obj.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        if clean.is_empty() {
            obj.remove("content");
        } else {
            obj.insert("content".into(), Value::String(clean));
        }
    }
}

/// Build the canonical system prompt used by [`chat_completion`].
/// Mirror of upstream `buildCompactSystemPrompt` (bin/smallcode.js).
///
/// ### Upstream vs port — deviations
/// INTENTIONAL:
///   - Agent name is "itsy" not "SmallCode" — product name.
///   - Bootstrap detector omitted — Rust-side feature not yet ported; upstream
///     injects a compact project summary (runtime, build/test commands, entry
///     point) so the model doesn't burn tool calls on discovery. Marked
///     UNVERIFIED until ported or confirmed not needed.
///   - BoneScript `backend` task hint omitted — `bone_check`/`bone_compile`
///     tools are not implemented in itsy; including the hint would cause the
///     model to call non-existent tools.
///   - `taskType !== 'explanation'` guard kept (matches upstream).
///
/// ACCIDENTAL (fixed in 3322058):
///   - Previous port had verbose IMPORTANT headers and bullet lists instead of
///     upstream's compact single-paragraph Rules line. Verbose prompts degrade
///     small-model behaviour — the model reads more text and acts less.
///   - Missing "ACT immediately" directive — upstream explicitly tells the
///     model not to ask for confirmation; absence caused hesitation loops.
///   - Missing large-file write_file cap (60 lines / ~8KB) — upstream warns
///     that llama.cpp JSON parser crashes on large tool calls.
///   - Working directory was appended after the rules instead of inline in the
///     first line, matching upstream's `Working directory: ${process.cwd()}`.
///
/// UNVERIFIED:
///   - Bootstrap detector prompt line (upstream: `_bootstrapDetector.formatForPrompt()`).
pub fn build_system_prompt(
    _config: &Config,
    memory_context: &str,
    skill_context: &str,
    plugin_context: &str,
    current_task_type: Option<&str>,
) -> String {
    let os = if cfg!(windows) { "Windows" } else if cfg!(target_os = "macos") { "macOS" } else { "Linux" };
    let os_hint = if cfg!(windows) {
        "\nUse \"dir\" not \"ls\", \"type\" not \"cat\". No bash-only commands."
    } else {
        ""
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let mut prompt = format!(
        include_str!("assets/prompts/system.txt"),
        cwd = cwd,
        os = os,
        os_hint = os_hint,
    );

    let task_type = current_task_type.unwrap_or("");
    if task_type != "explanation" {
        prompt.push_str(
            "\nUse graph_search/explain_symbol for \"how does X work\" questions. \
             Use list_projects for workspace overview.",
        );
    }
    prompt.push_str(memory_context);
    prompt.push_str(skill_context);
    prompt.push_str(plugin_context);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_think_tags ────────────────────────────────────────────────────

    #[test]
    fn test_strip_think_tags_removes_think_block() {
        let mut msg = json!({"role": "assistant", "content": "Hello <think>I should help</think> world"});
        strip_think_tags(&mut msg);
        assert_eq!(msg["content"], "Hello  world");
    }

    #[test]
    fn test_strip_think_tags_removes_orphan_close() {
        let mut msg = json!({"role": "assistant", "content": "Hello </think> world"});
        strip_think_tags(&mut msg);
        assert_eq!(msg["content"], "Hello  world");
    }

    #[test]
    fn test_strip_think_tags_preserves_no_think() {
        let mut msg = json!({"role": "assistant", "content": "Hello world"});
        strip_think_tags(&mut msg);
        assert_eq!(msg["content"], "Hello world");
    }

    #[test]
    fn test_strip_think_tags_removes_missing_content_key() {
        let mut msg = json!({"role": "system"});
        strip_think_tags(&mut msg);  // should not panic
        assert!(msg.get("content").is_none());
    }

    #[test]
    fn test_strip_think_tags_handles_only_think() {
        let mut msg = json!({"role": "assistant", "content": "  <think>only thinking</think>  "});
        strip_think_tags(&mut msg);
        // content may be removed or set to whitespace — either is acceptable.
        assert!(msg.get("content").map(|v| v.as_str().unwrap().trim().is_empty()).unwrap_or(true));
    }

    #[test]
    fn test_strip_think_tags_multiple_blocks() {
        let mut msg = json!({"role": "assistant", "content": "a<think>one</think>b<think>two</think>c"});
        strip_think_tags(&mut msg);
        assert_eq!(msg["content"], "abc");
    }

    // ── extract_xml_tool_calls ───────────────────────────────────────────────

    #[test]
    fn test_extract_tool_calls_json_format() {
        let content = "Let me check.\n<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"src/main.rs\"}}\n</tool_call>";
        let mut msg = json!({"role": "assistant", "content": content});
        extract_xml_tool_calls(&mut msg);
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        // XML should be stripped from content.
        assert!(!msg["content"].as_str().unwrap().contains("<tool_call>"));
    }

    #[test]
    fn test_extract_tool_calls_attr_format() {
        let content = "<tool_call>\n<function=read_and_patch>\n<parameter=path>src/main.rs</parameter>\n<parameter=old_str>foo</parameter>\n<parameter=new_str>bar</parameter>\n</function>\n</tool_call>";
        let mut msg = json!({"role": "assistant", "content": content});
        extract_xml_tool_calls(&mut msg);
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_and_patch");
        let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn test_extract_tool_calls_strip_only_when_server_provided() {
        let content = "<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"x\"}}\n</tool_call>";
        let mut msg = json!({
            "role": "assistant",
            "content": content,
            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]
        });
        extract_xml_tool_calls(&mut msg);
        // Should not add duplicate tool_calls.
        assert_eq!(msg.get("tool_calls").and_then(|v| v.as_array()).map(|a| a.len()), Some(1));
        // Content is fully consumed by the tool_call XML — may be removed entirely.
        assert!(msg.get("content").map(|v| !v.as_str().unwrap_or("").contains("<tool_call>")).unwrap_or(true));
    }

    #[test]
    fn test_extract_tool_calls_handles_no_tool_call() {
        let mut msg = json!({"role": "assistant", "content": "Just text, no tool calls."});
        extract_xml_tool_calls(&mut msg);
        assert!(msg.get("tool_calls").is_none());
        assert_eq!(msg["content"], "Just text, no tool calls.");
    }

    // ── run_validation (JSON path — no subprocess needed) ────────────────────

    #[test]
    fn test_run_validation_json_invalid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), b"{invalid json}").unwrap();
        std::env::set_current_dir(dir.path()).ok();
        let result = run_validation("bad.json");
        assert!(result.is_some());
        assert!(!result.unwrap().passed);
    }

    #[test]
    fn test_run_validation_json_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.json"), b"{\"key\": \"value\"}").unwrap();
        std::env::set_current_dir(dir.path()).ok();
        let result = run_validation("good.json");
        assert!(result.is_some());
        assert!(result.unwrap().passed);
    }

    #[test]
    fn test_run_validation_rejects_null_bytes() {
        let result = run_validation("bad\0file.json");
        assert!(result.is_some());
        assert!(!result.unwrap().passed);
    }

    // ── chat_completion via wiremock ──────────────────────────────────────
    //
    // These tests drive the real `chat_completion` function against a fake
    // OpenAI-compatible HTTP server. They pin retry behavior, transient-error
    // recovery, tool-call extraction, and finish_reason=length retry — all
    // logic that would otherwise only get exercised against a live llama-server.

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Boilerplate to build a ChatContext pointing at a wiremock server.
    fn chat_ctx<'a>(base_url: &'a str, tools: Vec<Value>, conversation: &'a [Value]) -> ChatContext<'a> {
        ChatContext {
            model_name: "test-model",
            base_url,
            api_key: None,
            timeout: Duration::from_secs(5),
            temp_adapt: false,
            conversation,
            tools,
            current_task_type: Some("coding"),
            system_prompt: "You are a test agent.".into(),
            force_disable_thinking: false,
        }
    }

    /// Successful 200 with a normal tool_calls response is parsed and returned.
    #[tokio::test]
    async fn chat_completion_returns_parsed_response_on_200() {
        let server = MockServer::start().await;
        let response = json!({
            "id": "x", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","content":"hello","tool_calls":[]},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&response))
            .mount(&server).await;

        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;
        let v = r.expect("must return Some on 200");
        assert_eq!(v["choices"][0]["message"]["content"], "hello");
    }

    /// 4xx on first attempt triggers ONE retry. The mock asserts that the
    /// endpoint was hit exactly twice — first failure + retry success.
    #[tokio::test]
    async fn chat_completion_retries_once_on_4xx() {
        let server = MockServer::start().await;
        // First call: 400. Subsequent calls: 200.
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .up_to_n_times(1)
            .mount(&server).await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":"recovered"},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;
        let v = r.expect("must recover after one retry");
        assert_eq!(v["choices"][0]["message"]["content"], "recovered");
    }

    /// 5xx error fails immediately without retry — anti-regression for
    /// double-retry that would amplify backend overload.
    #[tokio::test]
    async fn chat_completion_does_not_retry_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .expect(1) // wiremock will FAIL the test if called more or fewer times
            .mount(&server).await;
        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;
        assert!(r.is_none(), "5xx must return None without retry");
        // wiremock asserts `expect(1)` on drop.
    }

    /// Sustained 4xx (both attempts) returns None — anti-regression for the
    /// retry loop running forever.
    #[tokio::test]
    async fn chat_completion_returns_none_after_4xx_retries_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .expect(2) // exactly two attempts: original + one retry
            .mount(&server).await;
        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;
        assert!(r.is_none());
    }

    /// Network connect error returns None — model_client never panics on transport failure.
    #[tokio::test]
    async fn chat_completion_returns_none_on_network_error() {
        // Port 1 should reject connections immediately.
        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx("http://127.0.0.1:1", vec![], &convo)).await;
        assert!(r.is_none());
    }

    /// Request body must NOT include a `tools` key when tools list is empty
    /// (avoids Qwen3 misinterpreting `tools: []` as tool-mode-with-no-tools).
    /// We assert this by capturing the request body via wiremock's matcher.
    #[tokio::test]
    async fn chat_completion_omits_tools_key_when_list_empty() {
        let server = MockServer::start().await;

        // Match a POST whose body does NOT contain "tools".
        Mock::given(method("POST")).and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(json!({"model":"test-model"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        let convo = vec![json!({"role": "user", "content": "hi"})];
        let _ = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;

        // Inspect the captured request to confirm "tools" was NOT serialised.
        let received = server.received_requests().await.expect("requests captured");
        let req = received.iter().find(|r| r.url.path().contains("chat/completions")).expect("hit chat endpoint");
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert!(body.get("tools").is_none(),
            "body must omit `tools` key entirely when list is empty; got body={body}");
    }

    /// Tools key IS present (as array) when ctx.tools is non-empty.
    #[tokio::test]
    async fn chat_completion_includes_tools_when_list_non_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        let tools = vec![json!({"type":"function","function":{"name":"bash","parameters":{}}})];
        let convo = vec![json!({"role": "user", "content": "hi"})];
        let _ = chat_completion(&chat_ctx(&server.uri(), tools, &convo)).await;

        let received = server.received_requests().await.unwrap();
        let req = &received[0];
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let body_tools = body.get("tools").and_then(|v| v.as_array()).expect("tools must be present");
        assert!(!body_tools.is_empty(), "non-empty input list must serialise to non-empty tools");
    }

    /// Authorization header is set from api_key.
    #[tokio::test]
    async fn chat_completion_sends_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        let convo = vec![json!({"role": "user", "content": "hi"})];
        let uri = server.uri();
        let mut ctx = chat_ctx(&uri, vec![], &convo);
        ctx.api_key = Some("sk-test-secret".into());
        let _ = chat_completion(&ctx).await;

        let received = server.received_requests().await.unwrap();
        let req = &received[0];
        let auth = req.headers.get("authorization");
        // Env may inject API keys; we only assert when env is clean.
        if std::env::var("OPENAI_API_KEY").is_err()
            && std::env::var("ANTHROPIC_API_KEY").is_err()
            && std::env::var("DEEPSEEK_API_KEY").is_err() {
            assert!(auth.is_some(), "must send Authorization header");
            assert!(auth.unwrap().to_str().unwrap().starts_with("Bearer "));
        }
    }

    /// Invalid JSON in 200 response returns None (parse failure).
    /// Anti-regression: an upstream that returns malformed JSON must not crash.
    #[tokio::test]
    async fn chat_completion_returns_none_on_invalid_json_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json {{"))
            .mount(&server).await;
        let convo = vec![json!({"role": "user", "content": "hi"})];
        let r = chat_completion(&chat_ctx(&server.uri(), vec![], &convo)).await;
        assert!(r.is_none());
    }

    // ── stream_final_response ──────────────────────────────────────────────

    /// SSE stream is parsed and `on_token` is called per delta. The final
    /// concatenated string is returned.
    #[tokio::test]
    async fn stream_response_accumulates_tokens() {
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
                   data: [DONE]\n";
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server).await;

        let convo = vec![json!({"role":"user","content":"hi"})];
        let mut tokens: Vec<String> = Vec::new();
        let r = stream_final_response("test-model", &server.uri(), None, 5, &convo, None,
            |t| tokens.push(t.to_string())).await;
        assert_eq!(r, Some("Hello world".into()));
        assert_eq!(tokens, vec!["Hello".to_string(), " world".to_string()]);
    }

    /// 4xx on streaming endpoint returns None (no panic on non-200).
    #[tokio::test]
    async fn stream_response_returns_none_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server).await;
        let convo = vec![json!({"role":"user","content":"hi"})];
        let r = stream_final_response("test", &server.uri(), None, 5, &convo, None, |_| {}).await;
        assert!(r.is_none());
    }

    /// Lines without the `data:` prefix are ignored.
    #[tokio::test]
    async fn stream_response_ignores_non_data_lines() {
        let server = MockServer::start().await;
        let sse = ": comment line\n\
                   event: something\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: [DONE]\n";
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server).await;
        let convo = vec![json!({"role":"user","content":"x"})];
        let r = stream_final_response("test", &server.uri(), None, 5, &convo, None, |_| {}).await;
        assert_eq!(r, Some("hi".into()));
    }

    /// Malformed JSON lines are skipped (logged) without aborting the stream.
    /// Anti-regression: one bad chunk shouldn't lose the whole response.
    #[tokio::test]
    async fn stream_response_skips_bad_json_chunks() {
        let server = MockServer::start().await;
        let sse = "data: not a json\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"survives\"}}]}\n\
                   data: [DONE]\n";
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server).await;
        let convo = vec![json!({"role":"user","content":"x"})];
        let r = stream_final_response("test", &server.uri(), None, 5, &convo, None, |_| {}).await;
        assert_eq!(r, Some("survives".into()));
    }
}

