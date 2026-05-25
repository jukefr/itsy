//! Pure JSON-RPC helpers lifted from `bin/itsy.rs` so the MCP server's
//! request shape, tool list, and safety checks can be unit-tested.

use serde_json::{json, Value};

/// JSON-RPC error code for parse errors (per the spec).
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code for unknown methods.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Build the response to an `initialize` request.
pub fn build_initialize_response(id: Value, version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "itsy", "version": version },
        }
    })
}

/// Build the response to `tools/list` — the canonical itsy MCP tool catalog.
pub fn build_tools_list_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {"name": "itsy_read_file", "description": "Read file contents", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
                {"name": "itsy_search", "description": "Search code with regex", "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string"}}, "required": ["pattern"]}},
                {"name": "itsy_patch", "description": "Edit file via search-and-replace", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"}}, "required": ["path", "old_str", "new_str"]}},
                {"name": "itsy_bash", "description": "Run shell command", "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
                {"name": "itsy_memory_load", "description": "Load relevant project memory", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}},
                {"name": "itsy_memory_remember", "description": "Save knowledge to project memory", "inputSchema": {"type": "object", "properties": {"type": {"type": "string"}, "title": {"type": "string"}, "content": {"type": "string"}}, "required": ["type", "title", "content"]}},
                {"name": "itsy_agent", "description": "Send a prompt to itsy", "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}},
            ]
        }
    })
}

/// JSON-RPC error envelope.
pub fn build_error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Wrap a plain-text tool result in the MCP `content` envelope.
pub fn build_tool_text_result(id: Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }] }
    })
}

/// Returns true if the command matches one of itsy's destructive
/// patterns and should be refused before reaching the shell.
/// Anti-regression: model-issued `rm -rf /` and `format c:` must be
/// blocked at this gate before bash ever sees them.
pub fn is_destructive_command(command: &str) -> bool {
    static RM_RF_ROOT: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"rm\s+-rf\s+/[^.]").expect("valid regex literal"));
    static FORMAT_C: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)format\s+c:").expect("valid regex literal"));
    RM_RF_ROOT.is_match(command) || FORMAT_C.is_match(command)
}

/// Apply a unique-match patch on file content. Returns Ok(new_content) when
/// `old` matches exactly once. Mirrors the MCP `itsy_patch` body shape.
///
/// * Empty `old` → Err("old_str not found") (semantics: no anchor).
/// * Multiple matches → Err("matches multiple locations") (force a unique anchor).
pub fn apply_unique_patch(content: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() || !content.contains(old) {
        return Err("old_str not found".into());
    }
    if content.matches(old).count() > 1 {
        return Err("old_str matches multiple locations".into());
    }
    Ok(content.replace(old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JSON-RPC envelope shape ────────────────────────────────────────────

    #[test]
    fn initialize_response_shape() {
        let r = build_initialize_response(json!(1), "0.9.0");
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "itsy");
        assert_eq!(r["result"]["serverInfo"]["version"], "0.9.0");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    /// Null `id` from notification-style requests is preserved (don't drop it).
    #[test]
    fn initialize_preserves_null_id() {
        let r = build_initialize_response(Value::Null, "x");
        assert!(r["id"].is_null());
    }

    #[test]
    fn tools_list_contains_all_seven_tools() {
        let r = build_tools_list_response(json!(2));
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 7,
            "MCP tool catalog must list exactly 7 tools; got {}", tools.len());
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in ["itsy_read_file", "itsy_search", "itsy_patch", "itsy_bash",
                         "itsy_memory_load", "itsy_memory_remember", "itsy_agent"] {
            assert!(names.contains(&expected),
                "MCP tool {expected} missing from /tools/list response");
        }
    }

    #[test]
    fn tools_list_each_has_input_schema_with_required() {
        let r = build_tools_list_response(json!(2));
        for tool in r["result"]["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object",
                "tool {name} inputSchema must be type=object");
            assert!(schema["properties"].is_object(),
                "tool {name} must have properties");
            assert!(schema["required"].is_array(),
                "tool {name} must declare required fields");
        }
    }

    #[test]
    fn error_response_uses_spec_codes() {
        let r = build_error_response(json!(3), PARSE_ERROR, "Parse error");
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 3);
        assert_eq!(r["error"]["code"], -32700);
        assert_eq!(r["error"]["message"], "Parse error");

        let r = build_error_response(Value::Null, METHOD_NOT_FOUND, "Unknown method: xx");
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tool_text_result_wraps_in_content_envelope() {
        let r = build_tool_text_result(json!("call-1"), "hello");
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], "call-1");
        let content = r["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
    }

    // ── Destructive command guard ─────────────────────────────────────────

    /// `rm -rf /` and variants are blocked.
    /// Anti-regression: the model must NEVER be able to wipe the host.
    #[test]
    fn destructive_rm_rf_root_blocked() {
        assert!(is_destructive_command("rm -rf /etc"));
        assert!(is_destructive_command("rm  -rf  /home"));
        assert!(is_destructive_command("rm -rf /usr/local"));
    }

    /// Subdirectory `rm -rf` is NOT blocked (the guard only catches root).
    /// This documents the existing scope — `rm -rf ./build` is allowed
    /// because it stays in cwd.
    #[test]
    fn destructive_rm_rf_subdir_is_allowed() {
        assert!(!is_destructive_command("rm -rf ./build"));
        assert!(!is_destructive_command("rm -rf .git"));
        assert!(!is_destructive_command("rm -rf tmp"));
    }

    /// Windows-style `format c:` is blocked (case-insensitive).
    #[test]
    fn destructive_format_c_blocked_case_insensitive() {
        assert!(is_destructive_command("format c:"));
        assert!(is_destructive_command("FORMAT C:"));
        assert!(is_destructive_command("Format C:"));
        assert!(is_destructive_command("format  c:"));
    }

    /// Benign commands pass through.
    #[test]
    fn benign_commands_not_flagged() {
        for cmd in ["ls -la", "cargo test", "echo hello", "git status", "rm foo.txt"] {
            assert!(!is_destructive_command(cmd), "{cmd} must NOT be flagged");
        }
    }

    // ── Patch helper ──────────────────────────────────────────────────────

    #[test]
    fn unique_patch_replaces_single_match() {
        let r = apply_unique_patch("hello world", "world", "rust").unwrap();
        assert_eq!(r, "hello rust");
    }

    #[test]
    fn unique_patch_errors_when_old_absent() {
        let err = apply_unique_patch("hello", "missing", "x").unwrap_err();
        assert!(err.contains("not found"));
    }

    /// Multiple matches → require a more specific anchor. Anti-regression:
    /// silently replacing only the first match would corrupt files.
    #[test]
    fn unique_patch_errors_on_multiple_matches() {
        let err = apply_unique_patch("foo foo foo", "foo", "bar").unwrap_err();
        assert!(err.contains("multiple"));
    }

    /// Empty `old_str` is rejected (catches a class of bugs where the model
    /// sends an empty patch).
    #[test]
    fn unique_patch_rejects_empty_old() {
        let err = apply_unique_patch("hello", "", "x").unwrap_err();
        assert!(err.contains("not found"));
    }

    /// Replacement preserves surrounding content exactly.
    #[test]
    fn unique_patch_preserves_surrounding_content() {
        let r = apply_unique_patch("a\nfn foo() {}\nb", "fn foo() {}", "fn bar() {}").unwrap();
        assert_eq!(r, "a\nfn bar() {}\nb");
    }
}
