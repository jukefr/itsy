//! Pure helpers lifted from `bin/itsy.rs` so they can be unit-tested without
//! standing up the whole binary.

/// Actionable hint injected when the same tool fails twice in a row.
/// Generic across tasks — no task-specific knowledge, just tool usage tips.
pub fn recency_tool_hint(tool_name: &str) -> &'static str {
    match tool_name {
        "patch" => "Try `read_and_patch` instead — it reads the current file first then patches atomically, avoiding stale-content mismatches.",
        "bash" => "Double-check that commands and paths exist. Use `which <cmd>` or `ls <path>` to probe before running complex commands.",
        "write_file" => "`write_file` only creates NEW files. If the path already exists, use `read_and_patch` to edit it. If your content is too large, split it: `write_file` for the first chunk, then `append_file` for subsequent sections.",
        "read_file" => "Verify the path exists with `bash` before reading.",
        _ => "Try a different approach, a different tool, or verify your assumptions with a simpler command first.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_get_specific_hints() {
        // Patch → steered to `read_and_patch`.
        let h = recency_tool_hint("patch");
        assert!(h.contains("read_and_patch"), "patch hint must steer to read_and_patch; got: {h}");

        // Bash → suggests probing commands.
        let h = recency_tool_hint("bash");
        assert!(h.contains("which") || h.contains("ls"), "bash hint must suggest probing; got: {h}");

        // write_file is create-only: hint must steer to read_and_patch for existing
        // files and to append_file for size overflow.
        let h = recency_tool_hint("write_file");
        assert!(h.contains("read_and_patch"),
            "write_file hint must steer existing-file edits to read_and_patch; got: {h}");
        assert!(h.contains("append_file"),
            "write_file hint must steer oversized writes to append_file; got: {h}");
        assert!(!h.contains("read_file "),
            "write_file is create-only — must NOT suggest read_file first; got: {h}");

        // read_file → reminds to verify path with bash.
        let h = recency_tool_hint("read_file");
        assert!(h.contains("bash"), "read_file hint must reference bash; got: {h}");
    }

    /// Unknown tools fall through to a generic hint — never empty, never panics.
    /// Anti-regression: a new tool with no hint must not silently produce nothing.
    #[test]
    fn unknown_tool_returns_generic_non_empty_hint() {
        let h = recency_tool_hint("brand_new_tool_xyz");
        assert!(!h.is_empty(), "fallback hint must not be empty");
        assert!(h.contains("different") || h.contains("approach"),
            "fallback hint should suggest an alternative; got: {h}");
    }

    /// Every specific hint is meaningfully different from the fallback —
    /// pin the "no-tool-aliased-to-generic" invariant.
    #[test]
    fn specific_hints_differ_from_fallback() {
        let fallback = recency_tool_hint("");
        for tool in ["patch", "bash", "write_file", "read_file"] {
            assert_ne!(recency_tool_hint(tool), fallback,
                "tool {tool} hint must not collapse to the fallback");
        }
    }
}
