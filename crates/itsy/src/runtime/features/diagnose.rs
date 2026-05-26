//! Diagnose a failing shell command — parses the compiled
//! `error_diagnosis` prompt's JSON response into a structured hint.

use serde_json::{json, Value};

use super::prompts::{call_prompt, strip_fences, truncate};

#[derive(Debug, Clone)]
pub struct ErrorDiagnosis {
    pub kind: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub suggestion: String,
}

pub async fn diagnose_error(command: &str, stderr: &str, exit_code: i32) -> Option<ErrorDiagnosis> {
    let r = call_prompt(
        "error_diagnosis",
        json!({
            "command": truncate(command, 500),
            "stderr": truncate(stderr, 1500),
            "exit_code": exit_code.to_string(),
        }),
    )
    .await
    .ok()?;
    let cleaned = strip_fences(&r);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    Some(ErrorDiagnosis {
        kind: parsed.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        file: parsed.get("file").and_then(|v| v.as_str()).map(String::from),
        line: parsed.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
        suggestion: truncate(parsed.get("suggestion").and_then(|v| v.as_str()).unwrap_or(""), 200),
    })
}
