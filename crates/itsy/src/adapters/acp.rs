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
