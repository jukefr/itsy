//! ACP (Agent Context Protocol) adapter.
//!
//! Exposes itsy as an ACP-compatible agent for Zed IDE integration.
//! See: <https://zed.dev/acp>
//!
//! The adapter translates between ACP's wire format and itsy's internal tool
//! system. Communication happens over stdio (JSON-RPC, similar to MCP/LSP):
//!   - the agent receives context (files, selections, diagnostics) from the IDE
//!   - the agent responds with actions (edits, commands, messages)

use std::io::{BufRead, BufReader, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Event helpers — these mirror the conversion functions consumed by the
// cognition layer when translating internal events to/from the ACP wire shape.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpEvent {
    pub kind: String,
    pub data: Value,
}

pub fn from_tool_call(name: &str, args: &Value) -> AcpEvent {
    AcpEvent {
        kind: "tool_call".into(),
        data: json!({ "name": name, "arguments": args }),
    }
}

pub fn from_tool_result(name: &str, result: &Value) -> AcpEvent {
    AcpEvent {
        kind: "tool_result".into(),
        data: json!({ "name": name, "result": result }),
    }
}

pub fn from_assistant_text(text: &str) -> AcpEvent {
    AcpEvent {
        kind: "assistant_text".into(),
        data: json!({ "text": text }),
    }
}

pub fn from_error(message: &str) -> AcpEvent {
    AcpEvent {
        kind: "error".into(),
        data: json!({ "message": message }),
    }
}

pub fn from_metadata(metadata: &Value) -> AcpEvent {
    AcpEvent {
        kind: "metadata".into(),
        data: metadata.clone(),
    }
}

// ---------------------------------------------------------------------------
// Capabilities and adapter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpCapabilities {
    pub edit: bool,
    pub command: bool,
    pub chat: bool,
    pub diagnostics: bool,
}

impl Default for AcpCapabilities {
    fn default() -> Self {
        Self {
            edit: true,
            command: true,
            chat: true,
            diagnostics: true,
        }
    }
}

/// Result of running a prompt through the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptResult {
    pub actions: Vec<Value>,
    pub message: String,
}

/// Trait implemented by the host agent loop. Kept minimal — the real
/// integration carries conversation state and tool execution behind the same
/// trait, which keeps the adapter mocking-friendly in tests.
pub trait AgentLoop {
    fn run_prompt(&mut self, text: &str, context: Option<&Value>) -> PromptResult;
}

/// ACP adapter: drives a stdio JSON loop, translating IDE messages into
/// agent-loop invocations and adapter responses.
pub struct AcpAdapter<L: AgentLoop> {
    pub agent_loop: L,
    pub config: Value,
    pub capabilities: AcpCapabilities,
    pub name: String,
    pub version: String,
    context: Option<Value>,
}

impl<L: AgentLoop> AcpAdapter<L> {
    pub fn new(agent_loop: L, config: Value) -> Self {
        Self {
            agent_loop,
            config,
            capabilities: AcpCapabilities::default(),
            name: "itsy".into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            context: None,
        }
    }

    /// Run the adapter to completion on the supplied stdio handles.
    /// Reads JSON messages line-by-line; writes responses as `\n`-delimited
    /// JSON to `output`.
    pub fn run<R: std::io::Read, W: Write>(&mut self, input: R, mut output: W) {
        // Emit the initial capabilities frame.
        let init = json!({
            "type": "agent.capabilities",
            "capabilities": self.capabilities,
            "name": self.name,
            "version": self.version,
        });
        let _ = writeln!(output, "{init}");

        let reader = BufReader::new(input);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(msg) => {
                    if let Some(reply) = self.handle_message(&msg) {
                        let _ = writeln!(output, "{reply}");
                    }
                    // `shutdown` requests terminate the loop cleanly.
                    if msg.get("type").and_then(|v| v.as_str()) == Some("shutdown") {
                        break;
                    }
                }
                Err(e) => {
                    let err = json!({ "type": "error", "message": e.to_string() });
                    let _ = writeln!(output, "{err}");
                }
            }
        }
    }

    fn handle_message(&mut self, msg: &Value) -> Option<Value> {
        let ty = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "context.update" => {
                self.context = Some(msg.clone());
                None
            }
            "prompt" => {
                let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let context = msg.get("context");
                let result = self.run_prompt(text, context);
                let mut payload = json!({
                    "type": "response",
                    "id": msg.get("id").cloned().unwrap_or(Value::Null),
                    "actions": result.actions,
                    "message": result.message,
                });
                // Carry over any extra fields that a richer agent loop populates.
                if let Value::Object(map) = &mut payload {
                    map.insert("ok".into(), Value::Bool(true));
                }
                Some(payload)
            }
            "action.confirm" => {
                // User approved/rejected a proposed action. Currently a no-op;
                // future versions will route this back into the agent loop.
                None
            }
            "shutdown" => None,
            other => Some(json!({
                "type": "error",
                "message": format!("Unknown message type: {other}"),
            })),
        }
    }

    fn run_prompt(&mut self, text: &str, context: Option<&Value>) -> PromptResult {
        let mut augmented = text.to_string();

        if let Some(ctx) = context {
            if let Some(file) = ctx.get("file") {
                if let Some(path) = file.get("path").and_then(|v| v.as_str()) {
                    augmented.push_str(&format!("\n\nCurrent file: {path}"));
                }
                if let Some(sel) = file.get("selection") {
                    let start = sel.get("start").and_then(|v| v.as_i64()).unwrap_or(0);
                    let end = sel.get("end").and_then(|v| v.as_i64()).unwrap_or(0);
                    let stext = sel.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    augmented.push_str(&format!(
                        "\nSelected text (lines {start}-{end}):\n{stext}"
                    ));
                }
            }
            if let Some(diags) = ctx.get("diagnostics").and_then(|v| v.as_array()) {
                if !diags.is_empty() {
                    augmented.push_str("\n\nIDE diagnostics:");
                    for d in diags {
                        let sev =
                            d.get("severity").and_then(|v| v.as_str()).unwrap_or("info");
                        let f = d.get("file").and_then(|v| v.as_str()).unwrap_or("");
                        let line = d.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
                        let m = d.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        augmented.push_str(&format!("\n  {sev} {f}:{line}: {m}"));
                    }
                }
            }
        }

        self.agent_loop.run_prompt(&augmented, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Event helpers ──────────────────────────────────────────────────────

    #[test]
    fn tool_call_event_shape() {
        let ev = from_tool_call("bash", &json!({"command":"ls"}));
        assert_eq!(ev.kind, "tool_call");
        assert_eq!(ev.data["name"], "bash");
        assert_eq!(ev.data["arguments"]["command"], "ls");
    }

    #[test]
    fn tool_result_event_shape() {
        let ev = from_tool_result("bash", &json!({"stdout":"hi"}));
        assert_eq!(ev.kind, "tool_result");
        assert_eq!(ev.data["name"], "bash");
        assert_eq!(ev.data["result"]["stdout"], "hi");
    }

    #[test]
    fn assistant_text_event_shape() {
        let ev = from_assistant_text("hello world");
        assert_eq!(ev.kind, "assistant_text");
        assert_eq!(ev.data["text"], "hello world");
    }

    #[test]
    fn error_event_shape() {
        let ev = from_error("oops");
        assert_eq!(ev.kind, "error");
        assert_eq!(ev.data["message"], "oops");
    }

    #[test]
    fn metadata_event_preserves_payload() {
        let meta = json!({"foo":"bar","count":3});
        let ev = from_metadata(&meta);
        assert_eq!(ev.kind, "metadata");
        assert_eq!(ev.data, meta);
    }

    // ── Capabilities default ───────────────────────────────────────────────

    #[test]
    fn default_capabilities_enable_all_features() {
        let c = AcpCapabilities::default();
        assert!(c.edit);
        assert!(c.command);
        assert!(c.chat);
        assert!(c.diagnostics);
    }

    // ── Adapter round-trip via stdin/stdout buffers ────────────────────────

    struct EchoAgent;
    impl AgentLoop for EchoAgent {
        fn run_prompt(&mut self, text: &str, _ctx: Option<&Value>) -> PromptResult {
            PromptResult { actions: vec![], message: format!("echo: {text}") }
        }
    }

    /// Adapter emits capabilities on startup and `response` for `prompt` messages,
    /// then exits on `shutdown`.
    #[test]
    fn adapter_responds_to_prompt_and_shutdown() {
        let input = b"{\"type\":\"prompt\",\"id\":\"1\",\"text\":\"hello\"}\n{\"type\":\"shutdown\"}\n".to_vec();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(EchoAgent, json!({}));
        a.run(input.as_slice(), &mut output);
        let s = String::from_utf8_lossy(&output);
        // First line: capabilities.
        assert!(s.contains("agent.capabilities"));
        assert!(s.contains("itsy"));
        // Second: response.
        assert!(s.contains("\"type\":\"response\""));
        assert!(s.contains("echo: hello"));
    }

    /// Malformed JSON lines produce an `error` reply but DON'T abort the loop.
    #[test]
    fn adapter_recovers_from_malformed_json() {
        let input = b"not json {{\n{\"type\":\"shutdown\"}\n".to_vec();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(EchoAgent, json!({}));
        a.run(input.as_slice(), &mut output);
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains("agent.capabilities"));
        assert!(s.contains("\"type\":\"error\""), "must emit error on bad JSON; got: {s}");
    }

    /// Unknown message type → error reply (but loop continues).
    #[test]
    fn adapter_reports_unknown_message_type() {
        let input = b"{\"type\":\"frobnicate\"}\n{\"type\":\"shutdown\"}\n".to_vec();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(EchoAgent, json!({}));
        a.run(input.as_slice(), &mut output);
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains("Unknown message type: frobnicate"));
    }

    /// `context.update` produces no reply but stores context for later prompts.
    /// The next prompt's augmented text includes the file path from the context.
    #[test]
    fn context_update_then_prompt_augments_text() {
        struct CaptureAgent { captured: std::sync::Arc<std::sync::Mutex<String>> }
        impl AgentLoop for CaptureAgent {
            fn run_prompt(&mut self, text: &str, _ctx: Option<&Value>) -> PromptResult {
                *self.captured.lock().unwrap() = text.to_string();
                PromptResult::default()
            }
        }
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let input = format!(
            "{}\n{}\n{}\n",
            json!({"type":"prompt","id":"1","text":"fix the bug","context":{"file":{"path":"src/foo.rs"}}}),
            json!({"type":"shutdown"}),
            ""
        ).into_bytes();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(
            CaptureAgent { captured: captured.clone() },
            json!({}),
        );
        a.run(input.as_slice(), &mut output);
        let augmented = captured.lock().unwrap().clone();
        assert!(augmented.contains("fix the bug"));
        assert!(augmented.contains("src/foo.rs"),
            "augmented text must include file path; got {augmented:?}");
    }

    /// Diagnostic context is folded into the augmented prompt.
    #[test]
    fn diagnostics_are_folded_into_prompt() {
        struct CaptureAgent { captured: std::sync::Arc<std::sync::Mutex<String>> }
        impl AgentLoop for CaptureAgent {
            fn run_prompt(&mut self, text: &str, _ctx: Option<&Value>) -> PromptResult {
                *self.captured.lock().unwrap() = text.to_string();
                PromptResult::default()
            }
        }
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let prompt_msg = json!({
            "type":"prompt","id":"1","text":"fix",
            "context":{"diagnostics":[{"severity":"error","file":"x.rs","line":42,"message":"type mismatch"}]}
        });
        let input = format!("{prompt_msg}\n{}\n", json!({"type":"shutdown"})).into_bytes();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(
            CaptureAgent { captured: captured.clone() },
            json!({}),
        );
        a.run(input.as_slice(), &mut output);
        let augmented = captured.lock().unwrap().clone();
        assert!(augmented.contains("IDE diagnostics:"));
        assert!(augmented.contains("type mismatch"));
        assert!(augmented.contains("x.rs:42"));
    }

    /// Empty input lines are skipped (no reply, no panic).
    #[test]
    fn adapter_skips_empty_lines() {
        let input = b"\n\n   \n{\"type\":\"shutdown\"}\n".to_vec();
        let mut output = Vec::new();
        let mut a = AcpAdapter::new(EchoAgent, json!({}));
        a.run(input.as_slice(), &mut output);
        // Only capabilities frame should appear.
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains("agent.capabilities"));
        // No "error" lines from blank inputs.
        assert!(!s.contains("\"type\":\"error\""));
    }
}
