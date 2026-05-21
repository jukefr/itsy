//! Exports the current session as a single
//! markdown transcript that can be pasted into a code-review tool.

use serde_json::Value;

pub fn export_markdown(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
        out.push_str(&format!("### {}\n\n{}\n\n", role, content));
    }
    out
}
