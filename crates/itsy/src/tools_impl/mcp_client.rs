//! Generic MCP JSON-RPC client over a
//! child process's stdin/stdout.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[allow(unused_imports)]
use tokio::io::AsyncWrite;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub struct McpClient {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    inner: Mutex<Inner>,
}

struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    tools: Vec<McpToolDef>,
    next_id: u64,
}

impl McpClient {
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env: HashMap::new(),
            inner: Mutex::new(Inner {
                proc: None,
                stdin: None,
                pending: Arc::new(Mutex::new(HashMap::new())),
                tools: Vec::new(),
                next_id: 1,
            }),
        }
    }

    pub async fn start(&self) -> Result<()> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending = self.inner.lock().pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(id) = v.get("id").and_then(|n| n.as_u64()) {
                        let waiter = pending.lock().remove(&id);
                        if let Some(tx) = waiter {
                            let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
            }
        });
        {
            let mut inner = self.inner.lock();
            inner.proc = Some(child);
            inner.stdin = Some(stdin);
        }
        // initialize
        let _ = self.call("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "itsy", "version": env!("CARGO_PKG_VERSION")}})).await;
        // discover tools
        if let Some(result) = self.call("tools/list", json!({})).await {
            if let Some(tools_arr) = result.get("tools").and_then(|v| v.as_array()) {
                let parsed: Vec<McpToolDef> = tools_arr
                    .iter()
                    .filter_map(|t| serde_json::from_value(t.clone()).ok())
                    .collect();
                self.inner.lock().tools = parsed;
            }
        }
        Ok(())
    }

    pub async fn call(&self, method: &str, params: Value) -> Option<Value> {
        let (tx, rx) = oneshot::channel();
        let request = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pending.lock().insert(id, tx);
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        };
        let line = format!("{}\n", serde_json::to_string(&request).ok()?);
        let mut stdin_taken = {
            let mut inner = self.inner.lock();
            inner.stdin.take()
        };
        let Some(mut s) = stdin_taken.take() else { return None; };
        let bytes = line.into_bytes();
        let write_res = s.write_all(&bytes).await;
        {
            let mut inner = self.inner.lock();
            inner.stdin = Some(s);
        }
        if write_res.is_err() {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(v)) => Some(v),
            _ => None,
        }
    }

    pub fn get_tool_defs(&self) -> Vec<Value> {
        let inner = self.inner.lock();
        inner.tools.iter().map(|t| json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }
        })).collect()
    }

    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.inner.lock().tools.iter().any(|t| t.name == name)
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.call("tools/call", json!({"name": name, "arguments": args}))
            .await
            .ok_or_else(|| anyhow!("MCP tool call timed out"))
    }
}
