//! Manages the built-in code-graph MCP server
//! lifecycle.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[allow(unused_imports)]
use tokio::io::AsyncWrite;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

#[derive(Default)]
struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: Arc<Mutex<std::collections::HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: u64,
}

pub struct McpBridge {
    inner: Mutex<Inner>,
}

impl McpBridge {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { next_id: 1, ..Default::default() }) }
    }

    fn find_mcp_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        let candidates = [
            dir.join("..").join("..").join("code-graph-mcp").join("dist").join("index.js"),
            dir.join("..").join("node_modules").join("budget-aware-mcp").join("dist").join("index.js"),
            PathBuf::from(std::env::var("BUDGET_AWARE_MCP_PATH").unwrap_or_default()),
        ];
        candidates.into_iter().find(|p| p.exists())
    }

    pub async fn start(&self) -> Result<bool> {
        let mcp_path = match Self::find_mcp_path() {
            Some(p) => p,
            None => return Ok(false),
        };
        let mut cmd = Command::new("node");
        cmd.arg(&mcp_path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
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
        Ok(true)
    }

    pub async fn call(&self, method: &str, params: Value) -> Option<Value> {
        let (tx, rx) = oneshot::channel();
        let request = {
            let mut inner = self.inner.lock();
            inner.stdin.as_ref()?;
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
        let write_res = s.write_all(line.as_bytes()).await;
        {
            let mut inner = self.inner.lock();
            inner.stdin = Some(s);
        }
        if write_res.is_err() {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(v)) => Some(v),
            _ => None,
        }
    }

    pub async fn init_code_graph(&self, version: &str) -> bool {
        // ── Native (Rust) code graph: preferred, always attempted ──
        // Spawn off the indexing onto a blocking thread so the synchronous
        // tree-sitter walk doesn't stall the tokio reactor.
        let native_ok = tokio::task::spawn_blocking(|| {
            let Some(graph) = crate::code_graph::try_get_code_graph() else {
                return false;
            };
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            // Skip if this repo is already indexed.
            if let Ok(repos) = graph.list_repos() {
                if !repos.is_empty() {
                    return true;
                }
            }
            let name = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into());
            graph.index_repo(&cwd, &name).is_ok()
        })
        .await
        .unwrap_or(false);

        // ── Legacy MCP path: only used if the JS server is reachable ──
        let mcp_started = self.inner.lock().stdin.is_some();
        if mcp_started {
            let _ = self
                .call(
                    "initialize",
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "itsy", "version": version}
                    }),
                )
                .await;
            let list = self.call("tools/call", json!({"name": "list_repos", "arguments": {}})).await;
            let already = list
                .as_ref()
                .and_then(|v| v.pointer("/content/0/text"))
                .and_then(|t| t.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| v.get("total").cloned())
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if already == 0 {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let _ = self
                    .call(
                        "tools/call",
                        json!({
                            "name": "index_repo",
                            "arguments": {"path": cwd.to_string_lossy(), "name": cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()}
                        }),
                    )
                    .await;
            }
        }

        native_ok || mcp_started
    }

    pub fn kill(&self) {
        if let Some(mut c) = self.inner.lock().proc.take() {
            let _ = c.start_kill();
        }
    }
}

impl Default for McpBridge {
    fn default() -> Self {
        Self::new()
    }
}
