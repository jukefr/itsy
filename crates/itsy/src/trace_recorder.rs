//! Trace recorder — captures agent execution traces (tool calls, model
//! responses, validations) to `.itsy/traces/<id>.json` with redaction
//! applied. Supports replay, debugging, and trace-to-test generation.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::security::{redact_string, redact_value};

const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TraceStep {
    #[serde(rename = "tool_call")]
    ToolCall {
        name: String,
        args: Value,
        result: String,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        timestamp: u128,
    },
    #[serde(rename = "model_response")]
    ModelResponse {
        content: Option<String>,
        #[serde(rename = "toolCalls")]
        tool_calls: Option<Vec<ModelToolCall>>,
        timestamp: u128,
    },
    #[serde(rename = "validation")]
    Validation {
        #[serde(rename = "filePath")]
        file_path: String,
        passed: bool,
        errors: Vec<String>,
        timestamp: u128,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelToolCall {
    pub name: String,
    pub args: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub prompt: u64,
    pub completion: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub id: String,
    pub model: String,
    pub prompt: String,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(rename = "endedAt", default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(rename = "durationMs", default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub steps: Vec<TraceStep>,
    pub tokens: Tokens,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub id: String,
    pub prompt: String,
    pub model: String,
    pub steps: usize,
    pub tokens: Tokens,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

pub struct TraceRecorder {
    pub workdir: PathBuf,
    pub traces_dir: PathBuf,
    pub current: Option<Trace>,
    pub recording: bool,
}

impl TraceRecorder {
    pub fn new(workdir: PathBuf) -> Self {
        let traces_dir = workdir.join(".itsy").join("traces");
        Self { workdir, traces_dir, current: None, recording: false }
    }

    pub fn start(&mut self, prompt: &str, model: &str) -> String {
        let id = short_id();
        self.current = Some(Trace {
            id: id.clone(),
            model: model.to_string(),
            prompt: prompt.to_string(),
            started_at: Utc::now().to_rfc3339(),
            ended_at: None,
            duration_ms: None,
            steps: Vec::new(),
            tokens: Tokens::default(),
        });
        self.recording = true;
        id
    }

    pub fn record_tool_call(&mut self, name: &str, args: &Value, result: &Value, duration_ms: u64) {
        if !self.recording {
            return;
        }
        let safe_args = redact_value(args);
        let safe_result = match result {
            Value::String(s) => {
                let red = redact_string(s);
                truncate(&red, 2000)
            }
            other => truncate(&redact_value(other).to_string(), 2000),
        };
        let Some(trace) = self.current.as_mut() else { return };
        trace.steps.push(TraceStep::ToolCall {
            name: name.to_string(),
            args: safe_args,
            result: safe_result,
            duration_ms,
            timestamp: now_ms(),
        });
    }

    pub fn record_model_response(&mut self, content: Option<&str>, tool_calls: Option<&[Value]>) {
        if !self.recording {
            return;
        }
        let safe_content = content.map(|c| truncate(&redact_string(c), 1000));
        let safe_tools = tool_calls.map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let func = tc.get("function")?;
                    let name = func.get("name")?.as_str()?.to_string();
                    let args_raw = func.get("arguments")?;
                    let args = match args_raw {
                        Value::String(s) => redact_string(s),
                        other => redact_value(other).to_string(),
                    };
                    Some(ModelToolCall { name, args })
                })
                .collect::<Vec<_>>()
        });
        let Some(trace) = self.current.as_mut() else { return };
        trace.steps.push(TraceStep::ModelResponse {
            content: safe_content,
            tool_calls: safe_tools,
            timestamp: now_ms(),
        });
    }

    pub fn record_tokens(&mut self, prompt: u64, completion: u64) {
        if !self.recording {
            return;
        }
        if let Some(t) = self.current.as_mut() {
            t.tokens.prompt += prompt;
            t.tokens.completion += completion;
        }
    }

    pub fn record_validation(&mut self, file_path: &str, passed: bool, errors: &[String]) {
        if !self.recording {
            return;
        }
        let truncated_errors: Vec<String> = errors.iter().take(5).cloned().collect();
        if let Some(t) = self.current.as_mut() {
            t.steps.push(TraceStep::Validation {
                file_path: file_path.to_string(),
                passed,
                errors: truncated_errors,
                timestamp: now_ms(),
            });
        }
    }

    pub fn stop(&mut self) -> Option<Trace> {
        if !self.recording {
            return None;
        }
        let mut trace = self.current.take()?;
        self.recording = false;
        let now = Utc::now();
        let started = chrono::DateTime::parse_from_rfc3339(&trace.started_at).ok().map(|d| d.timestamp_millis() as u64).unwrap_or(0);
        let ended = now.timestamp_millis() as u64;
        trace.ended_at = Some(now.to_rfc3339());
        trace.duration_ms = Some(ended.saturating_sub(started));
        trace.prompt = redact_string(&trace.prompt);

        if !self.traces_dir.exists() {
            let _ = fs::create_dir_all(&self.traces_dir);
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(&self.traces_dir, fs::Permissions::from_mode(DIR_MODE));
            }
        }
        let id: String = trace.id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        if id.is_empty() {
            return None;
        }
        let file_path = self.traces_dir.join(format!("{id}.json"));
        if !file_path.starts_with(&self.traces_dir) {
            return None;
        }
        let tmp_path = self.traces_dir.join(format!("{id}.json.tmp.{}.{}", std::process::id(), now_ms()));
        let body = serde_json::to_string_pretty(&trace).ok()?;
        fs::write(&tmp_path, body).ok()?;
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(FILE_MODE));
        }
        fs::rename(&tmp_path, &file_path).ok()?;
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&file_path, fs::Permissions::from_mode(FILE_MODE));
        }
        Some(trace)
    }

    pub fn list(&self) -> Vec<TraceSummary> {
        if !self.traces_dir.exists() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.traces_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(trace) = serde_json::from_str::<Trace>(&text) {
                        let prompt_short: String = trace.prompt.chars().take(60).collect();
                        out.push(TraceSummary {
                            id: trace.id,
                            prompt: prompt_short,
                            model: trace.model,
                            steps: trace.steps.len(),
                            tokens: trace.tokens,
                            started_at: trace.started_at,
                            duration_ms: trace.duration_ms,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    pub fn load(&self, id: &str) -> Option<Trace> {
        let id_re = regex::Regex::new(r"^[A-Za-z0-9_-]{1,64}$").ok()?;
        if !id_re.is_match(id) {
            return None;
        }
        let path = self.traces_dir.join(format!("{id}.json"));
        if !path.starts_with(&self.traces_dir) || !path.exists() {
            return None;
        }
        let text = fs::read_to_string(&path).ok()?;
        serde_json::from_str::<Trace>(&text).ok()
    }

    /// Generate a Jest-style test file from a trace. Mirrors the JS version's
    /// schema even though we don't actually run JS — useful for `/share` and
    /// for replay in CI.
    pub fn generate_test(&self, trace_id: &str) -> Option<String> {
        let trace = self.load(trace_id)?;
        let tool_steps: Vec<&TraceStep> = trace
            .steps
            .iter()
            .filter(|s| matches!(s, TraceStep::ToolCall { .. }))
            .collect();
        if tool_steps.is_empty() {
            return None;
        }
        let prompt_short_80: String = trace.prompt.chars().take(80).collect();
        let prompt_short_40: String = trace.prompt.chars().take(40).collect();
        let mut lines = vec![
            format!("// Auto-generated from trace {}", trace.id),
            format!("// Original prompt: \"{}\"", prompt_short_80.replace('"', "\\\"")),
            format!(
                "// Model: {} | Steps: {} | Tokens: {}",
                trace.model,
                trace.steps.len(),
                trace.tokens.prompt + trace.tokens.completion
            ),
            String::new(),
            "const { execSync } = require('child_process');".into(),
            "const fs = require('fs');".into(),
            "const path = require('path');".into(),
            String::new(),
            format!(
                "describe('Trace {}: {}', () => {{",
                trace.id,
                prompt_short_40.replace('\'', "\\'")
            ),
        ];
        for (i, step) in tool_steps.iter().enumerate() {
            let TraceStep::ToolCall { name, args, duration_ms, .. } = step else { continue };
            let args_v = if let Value::String(s) = args {
                serde_json::from_str::<Value>(s).unwrap_or(Value::Null)
            } else {
                args.clone()
            };
            match name.as_str() {
                "write_file" | "patch" => {
                    let path_arg = args_v.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let path_short: String = path_arg.chars().take(30).collect();
                    lines.push(format!("  test('step {}: {} {}', () => {{", i + 1, name, path_short));
                    lines.push(format!("    // Tool: {} took {}ms", name, duration_ms));
                    if name == "write_file" {
                        lines.push(format!("    const filePath = path.resolve('{}');", path_arg));
                        lines.push("    expect(fs.existsSync(filePath)).toBe(true);".into());
                    }
                    lines.push("  });".into());
                    lines.push(String::new());
                }
                "bash" => {
                    let cmd = args_v.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    let cmd_short: String = cmd
                        .chars()
                        .take(40)
                        .collect::<String>()
                        .replace(['\'', '`', '\\', '\r', '\n'], " ");
                    let cmd_literal = serde_json::to_string(cmd).unwrap_or_default();
                    lines.push(format!("  test('step {}: bash {}', () => {{", i + 1, cmd_short));
                    lines.push(format!(
                        "    const result = execSync({}, {{ encoding: 'utf-8', timeout: 15000 }});",
                        cmd_literal
                    ));
                    lines.push("    expect(result).toBeDefined();".into());
                    lines.push("  });".into());
                    lines.push(String::new());
                }
                _ => {}
            }
        }
        lines.push("});".into());
        Some(lines.join("\n"))
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(8);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
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

fn _unused(_p: &Path) {} // keep PermissionsExt import live on non-Unix builds
