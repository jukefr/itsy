//! Minimal LSP client placeholder. The JS
//! version drives a per-project LSP via stdio; the Rust port exposes a
//! request/response API surface so callers can be wired up incrementally.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

#[derive(Default)]
struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: u64,
}

pub struct LspClient {
    inner: Mutex<Inner>,
}

impl LspClient {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { next_id: 1, ..Default::default() }) }
    }

    pub async fn start(&self, command: &str, args: Vec<String>) -> Result<()> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        {
            let mut inner = self.inner.lock();
            inner.proc = Some(child);
            inner.stdin = Some(stdin);
        }
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let request = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pending.lock().insert(id, tx);
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        };
        let body = serde_json::to_string(&request)?;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        {
            let mut inner = self.inner.lock();
            let Some(stdin) = inner.stdin.as_mut() else {
                return Err(anyhow!("LSP not started"));
            };
            stdin.write_all(framed.as_bytes()).await?;
        }
        Ok(tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await?
            .map_err(|_| anyhow!("LSP reply dropped"))?)
    }
}
