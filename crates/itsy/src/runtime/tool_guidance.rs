//! Tool guidance cards — proactive hints injected when a tool fails.
//!
//! The model gets a short, opinionated card for a tool when it has recently
//! failed, when it's been used recently, or when the user's intent keywords
//! match. Cards are kept intentionally brief — at IQ2_XXS every extra sentence
//! costs more than it adds.

use serde_json::Value;

/// Static guidance card for a single tool. Returns `""` for unknown names.
pub fn tool_skill_card(name: &str) -> &'static str {
    match name {
        "patch" => "\
### patch\n\
Applies diffs. Requires EXACT current file content.\n\
- Hunk failed / context mismatch: call read_file first, rebuild diff from current content\n\
- Never patch without reading the file in the same turn\n",

        "read_and_patch" => "\
### read_and_patch (preferred for edits)\n\
Atomic read + patch. Avoids stale-content failures.\n\
- Prefer this over separate read_file + patch\n\
- Context mismatch: call read_file, rebuild diff from fresh content\n",

        "write_file" => "\
### write_file\n\
Creates or overwrites. Requires a prior read_file on the same path.\n\
- \"Prior read required\": call read_file on the path first\n\
- For targeted edits prefer patch or read_and_patch\n",

        "bash" => "\
### bash\n\
Runs shell commands.\n\
- Command not found: verify with `which <cmd>`\n\
- Path errors: verify with `ls <path>` before using it\n\
- Check exit code in the result for test / build commands\n",

        "read_file" => "\
### read_file\n\
Reads file content.\n\
- File not found: verify with bash + ls or find first\n\
- Large file: use offset/limit to target a range — do not read the whole file\n\
- Re-read after writing to confirm the change took effect\n",

        _ => "",
    }
}

/// Select up to 3 tool guidance cards based on recent errors, recently used
/// tools, and intent keywords from the last user message.
/// Priority: error-recovery > recency > intent prediction.
/// Returns a ready-to-inject string (empty when no cards apply).
pub fn select_tool_skill_cards(messages: &[Value]) -> String {
    let mut id_to_name: std::collections::HashMap<&str, &str> = Default::default();
    let mut used_tools: Vec<&str> = Vec::new();
    let mut error_tools: Vec<&str> = Vec::new();
    let mut last_user_text: &str = "";

    for msg in messages.iter().rev().take(16) {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "assistant" => {
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() && !name.is_empty() {
                            id_to_name.insert(id, name);
                            if !used_tools.contains(&name) {
                                used_tools.push(name);
                            }
                        }
                    }
                }
            }
            "tool" => {
                let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let looks_like_error = content.contains("Error")
                    || content.contains("failed")
                    || content.contains("not found")
                    || content.contains("mismatch");
                if looks_like_error {
                    if let Some(name) = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| id_to_name.get(id))
                    {
                        if !error_tools.contains(name) {
                            error_tools.push(name);
                        }
                    }
                }
            }
            "user"
                if last_user_text.is_empty() => {
                    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
                        if !c.starts_with("[SYSTEM]") {
                            last_user_text = c;
                        }
                    }
                }
            _ => {}
        }
    }

    let lower = last_user_text.to_ascii_lowercase();
    let mut intent: Vec<&str> = Vec::new();
    if lower.contains("patch") || lower.contains("edit") || lower.contains("fix") || lower.contains("change") {
        intent.push("patch");
    }
    if lower.contains("write") || lower.contains("create") {
        intent.push("write_file");
    }
    if lower.contains("read") || lower.contains("show") || lower.contains("check") || lower.contains("look") {
        intent.push("read_file");
    }
    if lower.contains("run") || lower.contains("exec") || lower.contains("build") || lower.contains("test") || lower.contains("compile") {
        intent.push("bash");
    }

    let mut selected: Vec<&str> = Vec::new();
    for name in error_tools.iter().chain(used_tools.iter()).chain(intent.iter()) {
        if !selected.contains(name) && !tool_skill_card(name).is_empty() {
            selected.push(name);
        }
        if selected.len() >= 3 {
            break;
        }
    }

    if selected.is_empty() {
        return String::new();
    }

    const MAX_CARD_CHARS: usize = 1200;
    let mut out = String::from("\n\n## Tool guidance\n");
    for name in selected {
        let card = tool_skill_card(name);
        if out.len() + card.len() > MAX_CARD_CHARS {
            break;
        }
        out.push_str(card);
    }
    out
}
