//! Cognition-layer prompt dispatcher — ported from
//! upstream JS `cognition/prompts.js`. Three prompts back the cognition
//! adapter: `classify_task_type`, `code_assist`, `compress_history`.
//!
//! Each goes through the full pipeline: model lookup → template render →
//! cache lookup → provider call with retries → schema validate → cache put
//! → return. The JS layer copy-pastes the machinery per prompt; we collapse
//! it into one shared async [`dispatch`] helper.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::runtime::cognition::budget::{Budget, CostClass};
use crate::runtime::cognition::cache::{derive_key, DeriveKeyArgs, PromptCache};
use crate::runtime::cognition::router::{coding_router_route, RouteDecision};
use crate::runtime::cognition::traces::TraceBuffer;
use crate::runtime::cognition::validate::validate_type;
use crate::runtime::providers::openai_compat::OpenAICompatProvider;
use crate::runtime::providers::types::{ChatMessage, ChatRequest};

/// Dispatch a cognition-layer prompt by name. Returns `serde_json::Value`
/// (a string for string-typed prompts; parsed value for JSON-typed).
pub async fn call_prompt(name: &str, input: Value) -> Result<Value> {
    match name {
        "classify_task_type" => classify_task_type(input).await,
        "code_assist" => code_assist(input).await,
        "compress_history" => compress_history(input).await,
        _ => Err(anyhow!("Unknown prompt: {name}")),
    }
}

pub fn get_prompt_names() -> &'static [&'static str] {
    &["classify_task_type", "code_assist", "compress_history"]
}

/// Convention: any prompt name containing "summar" is treated as a
/// summariser fallback. The cognition layer has none built-in; reserved.
pub fn find_summarizer() -> Option<&'static str> {
    get_prompt_names().iter().find(|n| n.contains("summar")).copied()
}

/// Light template renderer kept for callers that just want `{{var}}`
/// interpolation against a JSON map (e.g. plugin-injected templates).
pub fn render(template: &str, vars: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'{') {
            chars.next();
            let mut key = String::new();
            while let Some(&nc) = chars.peek() {
                if nc == '}' {
                    break;
                }
                key.push(nc);
                chars.next();
            }
            if chars.peek() == Some(&'}') {
                chars.next();
            }
            if chars.peek() == Some(&'}') {
                chars.next();
            }
            let key_t = key.trim();
            let lookup = vars.pointer(&format!("/{}", key_t)).or_else(|| vars.get(key_t));
            if let Some(v) = lookup {
                match v {
                    Value::String(s) => out.push_str(s),
                    other => out.push_str(&other.to_string()),
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn get_template(name: &str) -> Option<&'static str> {
    Some(match name {
        "classify_task" => include_str!("../../assets/prompts/classify_task.txt"),
        "summarize_file" => include_str!("../../assets/prompts/summarize_file.txt"),
        _ => return None,
    })
}

// ─── Templates (extension points in upstream JS `extensions.ts`) ────────────

fn tmpl_classify_task(user_message: &str) -> String {
    format!(
        "Classify this user message into ONE of these categories. Reply with ONLY the category name, nothing else.\n\nCategories:\n- coding: creating new code/files\n- editing: modifying existing files\n- search: finding files or symbols\n- shell: running commands\n- explanation: answering questions, explaining concepts\n- multi_step: tasks with multiple sequential parts\n- debugging: fixing errors or bugs\n- backend: building backend services / APIs\n\nUser message: \"{user_message}\"\n\nCategory:"
    )
}

fn tmpl_compress_history(history: &str, max_tokens: u64) -> String {
    format!(
        "Compress the following conversation history into a concise summary of at most {max_tokens} tokens. Preserve key decisions, open questions, and the current task state. Omit small talk and resolved sub-steps.\n\nHistory:\n{history}\n\nSummary:"
    )
}

fn tmpl_code_assist(task: &str, context: &str) -> String {
    format!(
        "You are a coding assistant. Solve the task using the provided context.\n\nTask: {task}\n\nContext:\n{context}\n\nReply with code only, no explanation."
    )
}

// ─── Provider + model registry ──────────────────────────────────────────────

struct ModelSpec {
    name: &'static str,
    model_name: String,
    max_output: u32,
    temperature: f64,
    cost_class: CostClass,
}

fn medium_coder() -> ModelSpec {
    let m = std::env::var("ITSY_MODEL_STRONG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("ITSY_MODEL").ok())
        .unwrap_or_else(|| "default".into());
    ModelSpec { name: "MediumCoder", model_name: m, max_output: 8192, temperature: 0.1, cost_class: CostClass::Medium }
}

fn small_coder() -> ModelSpec {
    let m = std::env::var("ITSY_MODEL").ok().unwrap_or_else(|| "default".into());
    ModelSpec { name: "SmallCoder", model_name: m, max_output: 4096, temperature: 0.1, cost_class: CostClass::Small }
}

fn tiny_classifier() -> ModelSpec {
    let m = std::env::var("ITSY_MODEL").ok().unwrap_or_else(|| "default".into());
    ModelSpec { name: "TinyClassifier", model_name: m, max_output: 64, temperature: 0.0, cost_class: CostClass::Tiny }
}

fn model_for_tier(tier: &str) -> ModelSpec {
    match tier {
        "trivial" => tiny_classifier(),
        "simple" => small_coder(),
        _ => medium_coder(),
    }
}

fn base_url() -> String {
    let raw = std::env::var("ITSY_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("OLLAMA_HOST").ok().map(|h| format!("{h}/v1")))
        .unwrap_or_else(|| "http://localhost:1234/v1".to_string());
    crate::config::normalize_base_url(&raw)
}

fn provider() -> Result<OpenAICompatProvider> {
    OpenAICompatProvider::new(&base_url(), None)
}

// ─── Shared cache + traces ──────────────────────────────────────────────────

static CACHE: once_cell::sync::Lazy<PromptCache> = once_cell::sync::Lazy::new(PromptCache::new);
static TRACES: once_cell::sync::Lazy<TraceBuffer> = once_cell::sync::Lazy::new(|| TraceBuffer::new(4096));

pub fn cache() -> &'static PromptCache {
    &CACHE
}

pub fn traces() -> &'static TraceBuffer {
    &TRACES
}

// ─── Shared dispatch helper ─────────────────────────────────────────────────

struct DispatchOpts {
    prompt_name: &'static str,
    expected_type: &'static str,
    timeout: Duration,
    max_attempts: u32,
    ttl_ms: u128,
    parse_json: bool,
}

async fn dispatch(rendered: &str, input: &Value, model: ModelSpec, opts: DispatchOpts) -> Result<Value> {
    let trace_id = uuid_like();
    let provider = provider()?;

    let template_hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(rendered.as_bytes());
        let hex = format!("{:x}", h.finalize());
        hex[..16].to_string()
    };
    let cache_key = derive_key(DeriveKeyArgs {
        prompt_name: opts.prompt_name,
        model_id: model.name,
        template_hash: &template_hash,
        output_type: opts.expected_type,
        input,
    });
    if opts.ttl_ms > 0 {
        if let Some(hit) = CACHE.get(&cache_key) {
            TRACES.record(&trace_id, "cache_hit", json!({ "prompt": opts.prompt_name, "model": model.name }));
            return Ok(hit.value);
        }
    }

    let mut budget = Budget::new(&trace_id);
    let start = Instant::now();
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=opts.max_attempts {
        let estimated = (rendered.len() as u64 / 4) + model.max_output as u64;
        if let Err(e) = budget.assert_can_spend(estimated, model.cost_class) {
            return Err(anyhow!(e));
        }

        let req = ChatRequest {
            model: model.model_name.clone(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Value::String(rendered.to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: Some(model.temperature),
            top_p: None,
            max_output: Some(model.max_output),
            stop: None,
            json: false,
            tools: None,
            tool_choice: None,
        };

        let resp_res = tokio::time::timeout(opts.timeout, provider.chat(&req)).await;
        let resp = match resp_res {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                last_err = Some(e);
                continue;
            }
            Err(_) => {
                last_err = Some(anyhow!("timeout after {:?}", opts.timeout));
                continue;
            }
        };

        let raw = resp.content.clone();
        let value: Value = if opts.parse_json {
            let cleaned = raw
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim()
                .to_string();
            match serde_json::from_str::<Value>(&cleaned) {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(anyhow!("JSON parse: {e}"));
                    continue;
                }
            }
        } else {
            Value::String(raw)
        };

        if validate_type(&value, opts.expected_type, opts.prompt_name).is_err() {
            last_err = Some(anyhow!("validation failed"));
            continue;
        }

        let actual_tokens = (resp.usage.prompt_tokens.unwrap_or(0) + resp.usage.completion_tokens.unwrap_or(0)) as u64;
        budget.charge(actual_tokens.max(estimated), model.cost_class);

        if opts.ttl_ms > 0 {
            CACHE.put(
                &cache_key,
                value.clone(),
                opts.ttl_ms,
                resp.usage.prompt_tokens.map(|n| n as u64),
                resp.usage.completion_tokens.map(|n| n as u64),
                None,
            );
        }
        TRACES.record(
            &trace_id,
            "prompt_call",
            json!({
                "prompt": opts.prompt_name,
                "model": model.name,
                "attempt": attempt,
                "latency_ms": start.elapsed().as_millis() as u64,
            }),
        );
        return Ok(value);
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Prompt {} failed after retries", opts.prompt_name)))
}

// ─── classify_task_type ─────────────────────────────────────────────────────

async fn classify_task_type(input: Value) -> Result<Value> {
    let user_message = input.get("user_message").and_then(|v| v.as_str()).unwrap_or("");
    let rendered = tmpl_classify_task(user_message);
    dispatch(
        &rendered,
        &input,
        tiny_classifier(),
        DispatchOpts {
            prompt_name: "classify_task_type",
            expected_type: "string",
            timeout: Duration::from_millis(3000),
            max_attempts: 2,
            ttl_ms: 600_000,
            parse_json: false,
        },
    )
    .await
}

// ─── code_assist (router-tier driven) ───────────────────────────────────────

async fn code_assist(input: Value) -> Result<Value> {
    let RouteDecision { tier, .. } = coding_router_route(&input);
    let task = input.get("task").and_then(|v| v.as_str()).unwrap_or("");
    let context = input.get("context").and_then(|v| v.as_str()).unwrap_or("");
    let rendered = tmpl_code_assist(task, context);
    dispatch(
        &rendered,
        &input,
        model_for_tier(tier),
        DispatchOpts {
            prompt_name: "code_assist",
            expected_type: "string",
            timeout: Duration::from_millis(120_000),
            max_attempts: 3,
            ttl_ms: 0,
            parse_json: false,
        },
    )
    .await
}

// ─── compress_history ───────────────────────────────────────────────────────

async fn compress_history(input: Value) -> Result<Value> {
    let history = input.get("history").and_then(|v| v.as_str()).unwrap_or("");
    let max_tokens = input.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(500);
    let rendered = tmpl_compress_history(history, max_tokens);
    dispatch(
        &rendered,
        &input,
        small_coder(),
        DispatchOpts {
            prompt_name: "compress_history",
            expected_type: "string",
            timeout: Duration::from_millis(30_000),
            max_attempts: 2,
            ttl_ms: 300_000,
            parse_json: false,
        },
    )
    .await
}

fn uuid_like() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(b[0..4].try_into().unwrap()),
        u16::from_be_bytes(b[4..6].try_into().unwrap()),
        u16::from_be_bytes(b[6..8].try_into().unwrap()),
        u16::from_be_bytes(b[8..10].try_into().unwrap()),
        ((b[10] as u64) << 40) | ((b[11] as u64) << 32) | ((b[12] as u64) << 24) | ((b[13] as u64) << 16) | ((b[14] as u64) << 8) | (b[15] as u64),
    )
}
