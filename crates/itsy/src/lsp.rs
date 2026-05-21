//! LSP client.
//!
//! Spawns a per-project language server over stdio and exposes a small
//! surface of LSP calls used by the agent:
//!   * `textDocument/didOpen`, `didChange`, `didClose`
//!   * `textDocument/publishDiagnostics` (notification → diagnostics map)
//!   * `textDocument/hover`
//!   * `textDocument/definition`
//!   * `textDocument/references`
//!   * `textDocument/completion` (and `completionItem/resolve`)
//!
//! Language-server detection mirrors the JS version:
//!   * `tsconfig.json` or `package.json` → `typescript-language-server --stdio`
//!   * `pyproject.toml` or `setup.py`   → `pyright-langserver --stdio`
//!   * `Cargo.toml`                     → `rust-analyzer`
//!   * `go.mod`                         → `gopls serve`

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// Detected language server for a workspace.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub cmd: String,
    pub args: Vec<String>,
    pub language: String,
}

/// Auto-detect a language server based on project marker files.
pub fn detect_server(cwd: &Path) -> Option<ServerInfo> {
    if cwd.join("tsconfig.json").exists() || cwd.join("package.json").exists() {
        return Some(ServerInfo {
            cmd: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            language: "typescript".into(),
        });
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("setup.py").exists() {
        return Some(ServerInfo {
            cmd: "pyright-langserver".into(),
            args: vec!["--stdio".into()],
            language: "python".into(),
        });
    }
    if cwd.join("Cargo.toml").exists() {
        return Some(ServerInfo {
            cmd: "rust-analyzer".into(),
            args: vec![],
            language: "rust".into(),
        });
    }
    if cwd.join("go.mod").exists() {
        return Some(ServerInfo {
            cmd: "gopls".into(),
            args: vec!["serve".into()],
            language: "go".into(),
        });
    }
    None
}

/// LSP diagnostic (subset of fields).
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub uri: String,
    pub line: u32,
    pub character: u32,
    /// 1 = error, 2 = warning, 3 = info, 4 = hint.
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
type DiagMap = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

struct Inner {
    proc: Option<Child>,
    stdin: Option<ChildStdin>,
    next_id: u64,
}

pub struct LspClient {
    inner: Mutex<Inner>,
    pending: PendingMap,
    diagnostics: DiagMap,
    server: Mutex<Option<ServerInfo>>,
    cwd: Mutex<PathBuf>,
    initialized: Mutex<bool>,
}

impl Default for LspClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LspClient {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner { proc: None, stdin: None, next_id: 1 }),
            pending: Arc::new(Mutex::new(HashMap::new())),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            server: Mutex::new(None),
            cwd: Mutex::new(PathBuf::from(".")),
            initialized: Mutex::new(false),
        }
    }

    /// Auto-detect & start the language server for `cwd`. Returns `false`
    /// if no supported server is detected or the spawn failed.
    pub async fn start_auto(&self, cwd: &Path) -> bool {
        let Some(server) = detect_server(cwd) else { return false };
        *self.cwd.lock() = cwd.to_path_buf();
        *self.server.lock() = Some(server.clone());
        self.spawn_and_init(&server, cwd).await.is_ok()
    }

    /// Explicit start (e.g. when the caller already knows which server).
    pub async fn start(&self, command: &str, args: Vec<String>) -> Result<()> {
        let server = ServerInfo {
            cmd: command.to_string(),
            args: args.clone(),
            language: command.to_string(),
        };
        let cwd = self.cwd.lock().clone();
        *self.server.lock() = Some(server.clone());
        self.spawn_and_init(&server, &cwd).await
    }

    async fn spawn_and_init(&self, server: &ServerInfo, cwd: &Path) -> Result<()> {
        let mut child = Command::new(&server.cmd)
            .args(&server.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        {
            let mut inner = self.inner.lock();
            inner.proc = Some(child);
            inner.stdin = Some(stdin);
        }

        // Read loop: parse framed messages, route responses to pending, route
        // notifications (publishDiagnostics) to the diagnostics map.
        let pending = self.pending.clone();
        let diagnostics = self.diagnostics.clone();
        tokio::spawn(read_loop(stdout, pending, diagnostics));

        let root_uri = path_to_uri(cwd);
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": cwd.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            }],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {},
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": false },
                    "references": {},
                    "completion": {
                        "completionItem": { "snippetSupport": false, "resolveSupport": { "properties": ["documentation", "detail"] } }
                    },
                    "synchronization": { "didSave": true }
                },
                "workspace": { "workspaceFolders": true }
            },
        });
        let _ = self.request("initialize", init_params).await?;
        self.notify("initialized", json!({}))?;
        *self.initialized.lock() = true;
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        *self.initialized.lock()
    }

    pub async fn stop(&self) {
        // Best-effort shutdown handshake.
        if *self.initialized.lock() {
            let _ = self.request("shutdown", Value::Null).await;
            let _ = self.notify("exit", Value::Null);
        }
        let mut inner = self.inner.lock();
        if let Some(mut proc) = inner.proc.take() {
            let _ = proc.start_kill();
        }
        inner.stdin = None;
        *self.initialized.lock() = false;
    }

    /// JSON-RPC request with a 10-second timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            self.pending.lock().insert(id, tx);
            id
        };
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_message(&msg).await?;
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!("LSP reply dropped")),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(anyhow!("LSP timeout: {method}"))
            }
        }
    }

    /// JSON-RPC notification (no response expected).
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        // Fire-and-forget: we don't await stdout-flushing here.
        let body = serde_json::to_string(&msg)?;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut inner = self.inner.lock();
        if let Some(stdin) = inner.stdin.as_mut() {
            // Use try_write where possible — falls back to blocking write_all.
            futures::executor::block_on(async {
                let _ = stdin.write_all(framed.as_bytes()).await;
                let _ = stdin.flush().await;
            });
        }
        Ok(())
    }

    async fn write_message(&self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut inner = self.inner.lock();
        let stdin = inner.stdin.as_mut().ok_or_else(|| anyhow!("LSP not started"))?;
        stdin.write_all(framed.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    // ── High-level document lifecycle ───────────────────────────────────────

    fn language_id(&self) -> String {
        self.server.lock().as_ref().map(|s| s.language.clone()).unwrap_or_else(|| "plaintext".into())
    }

    pub fn did_open(&self, file_path: &Path, text: &str) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": self.language_id(),
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    pub fn did_change(&self, file_path: &Path, version: i64, text: &str) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    pub fn did_close(&self, file_path: &Path) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
    }

    /// Open the file (if not already), wait up to `timeout` for diagnostics
    /// to be published, then close it again so the server doesn't hold the
    /// document in memory forever.
    pub async fn get_diagnostics(&self, file_path: &Path) -> Result<Vec<Diagnostic>> {
        let abs = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
        let uri = path_to_uri(&abs);
        let content = std::fs::read_to_string(&abs)?;
        self.did_open(&abs, &content)?;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let diags = self.diagnostics.lock();
                if let Some(d) = diags.get(&uri) {
                    if !d.is_empty() {
                        let out = d.clone();
                        drop(diags);
                        let _ = self.did_close(&abs);
                        return Ok(out);
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let out = self.diagnostics.lock().get(&uri).cloned().unwrap_or_default();
        let _ = self.did_close(&abs);
        Ok(out)
    }

    pub fn format_diagnostics(&self, diags: &[Diagnostic]) -> Vec<String> {
        diags
            .iter()
            .filter(|d| d.severity <= 2)
            .map(|d| {
                let kind = if d.severity == 1 { "error" } else { "warning" };
                format!("{kind} line {}: {}", d.line + 1, d.message)
            })
            .collect()
    }

    // ── Language queries ────────────────────────────────────────────────────

    pub async fn hover(&self, file_path: &Path, line: u32, character: u32) -> Result<Value> {
        let uri = path_to_uri(file_path);
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    pub async fn definition(&self, file_path: &Path, line: u32, character: u32) -> Result<Value> {
        let uri = path_to_uri(file_path);
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    pub async fn references(
        &self,
        file_path: &Path,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Value> {
        let uri = path_to_uri(file_path);
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": include_declaration },
            }),
        )
        .await
    }

    pub async fn completion(&self, file_path: &Path, line: u32, character: u32) -> Result<Value> {
        let uri = path_to_uri(file_path);
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    pub async fn completion_item_resolve(&self, item: Value) -> Result<Value> {
        self.request("completionItem/resolve", item).await
    }
}

fn path_to_uri(p: &Path) -> String {
    let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // Normalize path separators and percent-encode minimally (LSP servers
    // generally accept `file://` URIs with raw paths).
    let s = abs.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

async fn read_loop(
    mut stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    diagnostics: DiagMap,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match stdout.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&chunk[..n]);
        loop {
            // Find header terminator.
            let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") else { break };
            let header = &buf[..hdr_end];
            let content_length = match parse_content_length(header) {
                Some(v) => v,
                None => {
                    // Malformed: drop the header to resync.
                    buf.drain(..hdr_end + 4);
                    continue;
                }
            };
            let body_start = hdr_end + 4;
            if buf.len() < body_start + content_length {
                break; // need more bytes
            }
            let body_slice = buf[body_start..body_start + content_length].to_vec();
            buf.drain(..body_start + content_length);

            let Ok(msg) = serde_json::from_slice::<Value>(&body_slice) else { continue };
            handle_message(msg, &pending, &diagnostics);
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(header: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(header).ok()?;
    for line in s.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn handle_message(msg: Value, pending: &PendingMap, diagnostics: &DiagMap) {
    // Response to a request.
    if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
        if let Some(tx) = pending.lock().remove(&id) {
            let result = msg
                .get("result")
                .cloned()
                .or_else(|| msg.get("error").cloned())
                .unwrap_or(Value::Null);
            let _ = tx.send(result);
            return;
        }
    }
    // Notification.
    if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics") {
        let Some(params) = msg.get("params") else { return };
        let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else { return };
        let mut out = Vec::new();
        if let Some(arr) = params.get("diagnostics").and_then(|d| d.as_array()) {
            for d in arr {
                let line = d
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32;
                let character = d
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("character"))
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32;
                let severity = d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8;
                let message =
                    d.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let source = d.get("source").and_then(|s| s.as_str()).map(String::from);
                out.push(Diagnostic {
                    uri: uri.to_string(),
                    line,
                    character,
                    severity,
                    message,
                    source,
                });
            }
        }
        diagnostics.lock().insert(uri.to_string(), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let s = detect_server(dir.path()).unwrap();
        assert_eq!(s.language, "rust");
        assert_eq!(s.cmd, "rust-analyzer");
    }

    #[test]
    fn parses_content_length() {
        assert_eq!(
            parse_content_length(b"Content-Length: 42\r\n"),
            Some(42)
        );
    }
}
