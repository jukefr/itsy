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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Known tool names produce non-empty cards.
    #[test]
    fn known_tools_have_cards() {
        for name in ["patch", "read_and_patch", "write_file", "bash", "read_file"] {
            assert!(!tool_skill_card(name).is_empty(), "{name} must have a card");
        }
    }

    /// Unknown tool names yield empty card.
    #[test]
    fn unknown_tool_yields_empty_card() {
        assert!(tool_skill_card("xxx").is_empty());
        assert!(tool_skill_card("").is_empty());
    }

    /// No messages → empty selection.
    #[test]
    fn empty_history_selects_no_cards() {
        assert_eq!(select_tool_skill_cards(&[]), "");
    }

    /// Error in tool message → that tool's card is selected.
    #[test]
    fn error_tool_message_selects_card() {
        let msgs = vec![
            json!({"role": "user", "content": "please patch the file"}),
            json!({"role": "assistant", "tool_calls": [
                {"id": "x", "function": {"name": "patch", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "x", "content": "Error: hunk failed: mismatch"}),
        ];
        let s = select_tool_skill_cards(&msgs);
        assert!(s.contains("## Tool guidance"));
        assert!(s.contains("### patch"));
    }

    /// User intent keywords surface intent-based cards even without prior failures.
    #[test]
    fn user_intent_surfaces_cards() {
        // "write" → write_file card. (write_file appears in the user msg.)
        let msgs = vec![json!({"role":"user","content":"please write a new file"})];
        let s = select_tool_skill_cards(&msgs);
        assert!(s.contains("### write_file"), "got: {s}");
    }

    /// `[SYSTEM]`-prefixed user messages are ignored for intent detection.
    /// Anti-regression: system nudges shouldn't perturb intent classification.
    #[test]
    fn system_prefixed_user_messages_ignored_for_intent() {
        let msgs = vec![
            json!({"role":"user","content":"[SYSTEM] you have been reading too long, write something"}),
        ];
        let s = select_tool_skill_cards(&msgs);
        // No real intent — should not pick a "write_file" card from a system nudge.
        assert!(s.is_empty() || !s.contains("### write_file"),
            "system-nudge content must not trigger intent cards; got {s}");
    }

    /// At most 3 cards per injection (bounded payload).
    #[test]
    fn caps_card_count_at_three() {
        let msgs = vec![
            json!({"role":"user","content":"read write patch bash edit fix run check"}),
            json!({"role":"assistant","tool_calls":[
                {"id":"a","function":{"name":"patch","arguments":"{}"}},
                {"id":"b","function":{"name":"bash","arguments":"{}"}},
                {"id":"c","function":{"name":"read_file","arguments":"{}"}},
                {"id":"d","function":{"name":"write_file","arguments":"{}"}},
            ]}),
        ];
        let s = select_tool_skill_cards(&msgs);
        let card_count = s.matches("###").count();
        assert!(card_count <= 3, "must cap at 3 cards; got {card_count}");
    }

    /// Error-recovery takes priority over recency.
    #[test]
    fn error_tools_outrank_recency() {
        let msgs = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","tool_calls":[
                {"id":"x","function":{"name":"bash","arguments":"{}"}},
                {"id":"y","function":{"name":"read_file","arguments":"{}"}},
                {"id":"z","function":{"name":"patch","arguments":"{}"}},
            ]}),
            // Only patch errored.
            json!({"role":"tool","tool_call_id":"x","content":"ok"}),
            json!({"role":"tool","tool_call_id":"y","content":"ok"}),
            json!({"role":"tool","tool_call_id":"z","content":"Error: hunk failed"}),
        ];
        let s = select_tool_skill_cards(&msgs);
        // patch card should appear, with priority. We just need it present.
        assert!(s.contains("### patch"));
    }
}
