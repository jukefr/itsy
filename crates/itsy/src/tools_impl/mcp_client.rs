//! Generic MCP JSON-RPC client over a child process's stdin/stdout.
//!
//! Connects TO external MCP servers and exposes their tools (and other
//! capabilities: resources, prompts, sampling, completion, logging, ping)
//! to the agent. Maintains a shared line demuxer per server so concurrent
//! requests can interleave safely. Auto-reconnects on transport failure.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use crate::security::sanitize_tool_output;

/// Ambient env vars that should NOT leak into untrusted MCP servers
/// unless the server's config explicitly opted in via `env`.
const SECRET_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY",
    "OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY", "GEMINI_API_KEY",
    "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN", "GITLAB_TOKEN",
    "ITSY_API_KEY",
];

const PROTOCOL_VERSION: &str = "2024-11-05";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
    pub sampling: bool,
    pub logging: bool,
    pub completion: bool,
}

#[derive(Debug, Clone)]
pub struct McpStatus {
    pub name: String,
    pub connected: bool,
    pub command: String,
    pub tools: Vec<String>,
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
    pub capabilities: ServerCapabilities,
}

pub struct McpClient {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auto_approve: Vec<String>,
    inner: Mutex<Inner>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: PendingMap,
    tools: Vec<McpToolDef>,
    resources: Vec<McpResource>,
    prompts: Vec<McpPrompt>,
    capabilities: ServerCapabilities,
    connected: bool,
    next_id: u64,
}

impl McpClient {
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env: HashMap::new(),
            auto_approve: Vec::new(),
            inner: Mutex::new(Inner {
                proc: None,
                stdin: None,
                pending: Arc::new(Mutex::new(HashMap::new())),
                tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
                capabilities: ServerCapabilities::default(),
                connected: false,
                next_id: 1,
            }),
        }
    }

    /// Start (or restart) the server process and run the JSON-RPC
    /// initialize handshake. Discovers tools/resources/prompts based on
    /// advertised capabilities.
    pub async fn start(&self) -> Result<()> {
        // Build env: inherit but strip host secrets unless explicitly re-exported
        let mut env_map: HashMap<String, String> = std::env::vars().collect();
        for k in SECRET_ENV_VARS {
            if !self.env.contains_key(*k) {
                env_map.remove(*k);
            }
        }
        for (k, v) in &self.env {
            env_map.insert(k.clone(), v.clone());
        }

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear();
        for (k, v) in &env_map {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending = self.inner.lock().pending.clone();

        // Shared line demuxer: every line is parsed and dispatched to the
        // matching waiter by id. Unknown lines are dropped silently.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v.get("id").and_then(|n| n.as_u64()) {
                    let waiter = pending.lock().remove(&id);
                    if let Some(tx) = waiter {
                        if v.get("error").is_some() {
                            // Surface errors as Null result; callers see no result.
                            let _ = tx.send(Value::Null);
                        } else {
                            let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
                // Notifications (no id) are accepted but ignored — we don't
                // yet route server->client requests like sampling/createMessage.
            }
        });

        {
            let mut inner = self.inner.lock();
            inner.proc = Some(child);
            inner.stdin = Some(stdin);
            inner.connected = false;
        }

        // Advertise client capabilities: we support sampling so MCP servers
        // know they may issue createMessage requests back to us.
        let init = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "sampling": {},
                        "roots": { "listChanged": false },
                    },
                    "clientInfo": { "name": "itsy", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await;

        let Some(init_result) = init else {
            self.disconnect();
            return Err(anyhow!("MCP initialize failed for {}", self.name));
        };

        // Parse server capabilities so we know what to discover.
        let caps = init_result.get("capabilities").cloned().unwrap_or(Value::Null);
        let server_caps = ServerCapabilities {
            tools: caps.get("tools").is_some(),
            resources: caps.get("resources").is_some(),
            prompts: caps.get("prompts").is_some(),
            sampling: caps.get("sampling").is_some(),
            logging: caps.get("logging").is_some(),
            completion: caps.get("completion").is_some(),
        };

        // Notify the server that init is complete.
        self.send_notification("notifications/initialized", json!({})).await;

        {
            let mut inner = self.inner.lock();
            inner.capabilities = server_caps.clone();
            inner.connected = true;
        }

        // Capability-gated discovery.
        if server_caps.tools {
            if let Some(result) = self.call("tools/list", json!({})).await {
                if let Some(arr) = result.get("tools").and_then(|v| v.as_array()) {
                    let parsed: Vec<McpToolDef> = arr
                        .iter()
                        .filter_map(|t| serde_json::from_value(t.clone()).ok())
                        .collect();
                    self.inner.lock().tools = parsed;
                }
            }
        }
        if server_caps.resources {
            if let Some(result) = self.call("resources/list", json!({})).await {
                if let Some(arr) = result.get("resources").and_then(|v| v.as_array()) {
                    let parsed: Vec<McpResource> = arr
                        .iter()
                        .filter_map(|r| serde_json::from_value(r.clone()).ok())
                        .collect();
                    self.inner.lock().resources = parsed;
                }
            }
        }
        if server_caps.prompts {
            if let Some(result) = self.call("prompts/list", json!({})).await {
                if let Some(arr) = result.get("prompts").and_then(|v| v.as_array()) {
                    let parsed: Vec<McpPrompt> = arr
                        .iter()
                        .filter_map(|p| serde_json::from_value(p.clone()).ok())
                        .collect();
                    self.inner.lock().prompts = parsed;
                }
            }
        }

        Ok(())
    }

    /// Issue a JSON-RPC request and await the response (10s timeout).
    /// Returns None if the call times out, the server is unreachable,
    /// or the response contains an error.
    pub async fn call(&self, method: &str, params: Value) -> Option<Value> {
        let (tx, rx) = oneshot::channel();
        let request_line = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pending.lock().insert(id, tx);
            let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            format!("{}\n", serde_json::to_string(&req).ok()?)
        };

        // Pull stdin out briefly to write (parking_lot mutex can't be held
        // across await). Put it back regardless of outcome.
        let mut stdin_taken = self.inner.lock().stdin.take();
        let mut s = stdin_taken.take()?;
        let write_res = s.write_all(request_line.as_bytes()).await;
        let flush_res = s.flush().await;
        self.inner.lock().stdin = Some(s);
        if write_res.is_err() || flush_res.is_err() {
            return None;
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) if !v.is_null() => Some(v),
            _ => None,
        }
    }

    /// Fire-and-forget JSON-RPC notification (no `id`, no response).
    pub async fn send_notification(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let line = match serde_json::to_string(&msg) {
            Ok(s) => format!("{s}\n"),
            Err(_) => return,
        };
        let mut stdin_taken = self.inner.lock().stdin.take();
        let Some(mut s) = stdin_taken.take() else { return };
        let _ = s.write_all(line.as_bytes()).await;
        let _ = s.flush().await;
        self.inner.lock().stdin = Some(s);
    }

    /// Call a tool by name. Auto-reconnects once on transport failure.
    /// Tool output is sanitized before returning.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        for attempt in 0..=MAX_RECONNECT_ATTEMPTS {
            let connected = self.inner.lock().connected;
            if !connected {
                if attempt == MAX_RECONNECT_ATTEMPTS {
                    return Err(anyhow!("MCP server '{}' not connected", self.name));
                }
                let _ = self.start().await;
                continue;
            }

            let result = self.call("tools/call", json!({"name": name, "arguments": args})).await;
            if let Some(v) = result {
                // Extract text content + isError flag, mirroring JS.
                let content = v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                let text = content
                    .iter()
                    .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                let is_error = v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
                let clean = sanitize_tool_output(&text);
                if is_error {
                    return Err(anyhow!(if clean.is_empty() { "MCP tool returned error".into() } else { clean }));
                }
                return Ok(json!({ "result": if clean.is_empty() { "(no output)".into() } else { clean } }));
            }

            // Transport failure / timeout — mark disconnected and retry.
            self.inner.lock().connected = false;
            if attempt == MAX_RECONNECT_ATTEMPTS {
                return Err(anyhow!("MCP call timed out after {} attempts", attempt + 1));
            }
        }
        Err(anyhow!("MCP call failed"))
    }

    // ── Resources ────────────────────────────────────────────────────

    pub async fn list_resources(&self) -> Vec<McpResource> {
        if let Some(result) = self.call("resources/list", json!({})).await {
            if let Some(arr) = result.get("resources").and_then(|v| v.as_array()) {
                let parsed: Vec<McpResource> = arr
                    .iter()
                    .filter_map(|r| serde_json::from_value(r.clone()).ok())
                    .collect();
                self.inner.lock().resources = parsed.clone();
                return parsed;
            }
        }
        self.inner.lock().resources.clone()
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.call("resources/read", json!({"uri": uri}))
            .await
            .ok_or_else(|| anyhow!("resource read failed for {uri}"))
    }

    // ── Prompts ──────────────────────────────────────────────────────

    pub async fn list_prompts(&self) -> Vec<McpPrompt> {
        if let Some(result) = self.call("prompts/list", json!({})).await {
            if let Some(arr) = result.get("prompts").and_then(|v| v.as_array()) {
                let parsed: Vec<McpPrompt> = arr
                    .iter()
                    .filter_map(|p| serde_json::from_value(p.clone()).ok())
                    .collect();
                self.inner.lock().prompts = parsed.clone();
                return parsed;
            }
        }
        self.inner.lock().prompts.clone()
    }

    pub async fn get_prompt(&self, name: &str, args: Value) -> Result<Value> {
        self.call("prompts/get", json!({"name": name, "arguments": args}))
            .await
            .ok_or_else(|| anyhow!("prompt fetch failed for {name}"))
    }

    // ── Sampling / Completion / Logging / Ping ───────────────────────

    /// Forward a sampling/createMessage request to the LLM. The caller
    /// supplies a closure that does the actual chat completion; this
    /// keeps this module free of model-client coupling.
    pub async fn handle_sampling<F, Fut>(&self, params: Value, sampler: F) -> Result<Value>
    where
        F: FnOnce(Value) -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        sampler(params).await
    }

    /// Issue a completion/complete request — used by some MCP servers
    /// for argument autocompletion in prompts.
    pub async fn complete(&self, reference: Value, argument: Value) -> Option<Value> {
        self.call("completion/complete", json!({"ref": reference, "argument": argument})).await
    }

    /// Adjust the log level on the server side (logging capability).
    pub async fn set_log_level(&self, level: &str) -> bool {
        self.call("logging/setLevel", json!({"level": level})).await.is_some()
    }

    /// Ping the server. Returns true if the pong arrived within the
    /// request timeout. Suitable for keepalive loops.
    pub async fn ping(&self) -> bool {
        self.call("ping", json!({})).await.is_some()
    }

    // ── Inspection ───────────────────────────────────────────────────

    pub fn get_tool_defs(&self) -> Vec<Value> {
        let inner = self.inner.lock();
        let server_name = &self.name;
        inner
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": format!("mcp__{server_name}__{}", t.name),
                        "description": format!("[{server_name}] {}", t.description),
                        "parameters": if t.input_schema.is_null() {
                            json!({"type": "object", "properties": {}})
                        } else {
                            t.input_schema.clone()
                        },
                    }
                })
            })
            .collect()
    }

    pub fn is_mcp_tool(&self, name: &str) -> bool {
        if name.starts_with("mcp__") {
            return true;
        }
        self.inner.lock().tools.iter().any(|t| t.name == name)
    }

    pub fn capabilities(&self) -> ServerCapabilities {
        self.inner.lock().capabilities.clone()
    }

    pub fn status(&self) -> McpStatus {
        let inner = self.inner.lock();
        McpStatus {
            name: self.name.clone(),
            connected: inner.connected,
            command: self.command.clone(),
            tools: inner.tools.iter().map(|t| t.name.clone()).collect(),
            resources: inner.resources.iter().map(|r| r.uri.clone()).collect(),
            prompts: inner.prompts.iter().map(|p| p.name.clone()).collect(),
            capabilities: inner.capabilities.clone(),
        }
    }

    pub fn disconnect(&self) {
        let mut inner = self.inner.lock();
        if let Some(mut child) = inner.proc.take() {
            let _ = child.start_kill();
        }
        inner.stdin = None;
        inner.connected = false;
        inner.pending.lock().clear();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}
