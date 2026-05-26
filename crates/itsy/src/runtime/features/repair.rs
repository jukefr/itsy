//! Repair a malformed tool call. Asks the compiled `repair_tool_call`
//! prompt to fix a JSON tool invocation the model produced wrong, given the
//! tool's schema and the parser error.

use serde_json::json;

use super::prompts::{call_prompt, truncate};

#[derive(Debug, Clone)]
pub struct RepairResult {
    pub ok: bool,
    pub repaired_call: Option<String>,
    pub error: Option<String>,
}

pub async fn repair_tool_call(original_call: &str, error: &str, tool_schema: &str) -> RepairResult {
    let result = call_prompt(
        "repair_tool_call",
        json!({
            "original_call": truncate(original_call, 2000),
            "error": truncate(error, 500),
            "tool_schema": truncate(tool_schema, 1000),
        }),
    )
    .await;
    match result {
        Ok(s) => RepairResult { ok: true, repaired_call: Some(s), error: None },
        Err(e) => RepairResult { ok: false, repaired_call: None, error: Some(e.to_string()) },
    }
}
