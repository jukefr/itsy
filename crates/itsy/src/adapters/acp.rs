//! ACP (Agent Connect Protocol) JSON-RPC
//! adapter. The JS file converts itsy events to/from the ACP wire
//! format; this Rust port mirrors the conversion functions.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
