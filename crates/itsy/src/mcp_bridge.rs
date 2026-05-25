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
        let mut s = stdin_taken.take()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `new()` builds an idle bridge with `next_id == 1`.
    #[test]
    fn new_starts_idle() {
        let b = McpBridge::new();
        let inner = b.inner.lock();
        assert!(inner.proc.is_none());
        assert!(inner.stdin.is_none());
        assert_eq!(inner.next_id, 1, "id counter must start at 1");
    }

    /// `Default` and `new` produce equivalent bridges.
    #[test]
    fn default_matches_new() {
        let a = McpBridge::default();
        let b = McpBridge::new();
        let ai = a.inner.lock();
        let bi = b.inner.lock();
        assert_eq!(ai.next_id, bi.next_id);
        assert!(ai.proc.is_none() && bi.proc.is_none());
    }

    /// `call` without a running server returns None (no panic).
    /// Anti-regression: callers must be able to invoke optimistically.
    #[tokio::test]
    async fn call_without_start_returns_none() {
        let b = McpBridge::new();
        let r = b.call("tools/list", serde_json::json!({})).await;
        assert!(r.is_none(), "call before start must safely return None");
    }

    /// `kill` on an idle bridge is a no-op (no panic).
    #[test]
    fn kill_idle_is_noop() {
        let b = McpBridge::new();
        b.kill();
        // Subsequent kill also safe.
        b.kill();
    }

    /// `find_mcp_path` returns None when no candidates exist (default test env).
    /// Anti-regression: must not panic exploring filesystem candidates.
    #[test]
    fn find_mcp_path_handles_no_candidates() {
        // We can't easily make this return Some without dropping fixtures into
        // the cargo build dir. Just confirm the function returns without panic.
        let _ = McpBridge::find_mcp_path();
    }

    /// `init_code_graph` returns true when native graph indexes successfully,
    /// false when no graph backend and no MCP server is available.
    #[tokio::test]
    async fn init_code_graph_returns_bool() {
        let b = McpBridge::new();
        // Whatever the result, the function must NOT panic and must return a bool.
        let _result: bool = b.init_code_graph("test-version").await;
    }
}
