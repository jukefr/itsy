//! Lives under `runtime` alongside the other deterministic helpers.

use serde_json::{json, Value};

pub struct CategoryDef {
    pub key: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
}

pub const TOOL_CATEGORIES: &[CategoryDef] = &[
    CategoryDef { key: "read", description: "Read file contents, find files by pattern", tools: &["read_file", "read_original", "find_files", "find_and_read"] },
    CategoryDef { key: "write", description: "Create files, edit files with patch, rewrite files", tools: &["write_file", "patch", "read_and_patch", "create_and_run"] },
    CategoryDef { key: "search", description: "Search code by regex, search code graph, explain symbols", tools: &["search", "search_and_read", "graph_search", "explain_symbol", "list_projects"] },
    CategoryDef { key: "run", description: "Run shell commands, execute scripts", tools: &["bash", "run"] },
    CategoryDef { key: "plan", description: "Load/save project memory", tools: &["memory_load", "memory_remember"] },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    Direct,
    TwoStage,
}

pub fn get_routing_mode(context_window: u32, env_override: Option<&str>) -> RoutingMode {
    match env_override {
        Some("direct") => return RoutingMode::Direct,
        Some("two_stage") => return RoutingMode::TwoStage,
        _ => {}
    }
    if context_window <= 16384 {
        RoutingMode::TwoStage
    } else {
        RoutingMode::Direct
    }
}

pub fn get_category_selector_tool() -> Value {
    let enum_values: Vec<&str> = TOOL_CATEGORIES.iter().map(|c| c.key).collect();
    json!({
        "type": "function",
        "function": {
            "name": "select_category",
            "description": "Pick the tool category you need. Categories: read (read/find files), write (create/edit files), search (grep/code graph), run (shell commands), plan (memory).",
            "parameters": {
                "type": "object",
                "properties": {
                    "category": {
                        "type": "string",
                        "enum": enum_values,
                        "description": "Tool category needed for your next action"
                    }
                },
                "required": ["category"]
            }
        }
    })
}

pub fn get_tools_for_category(category: &str, all_tools: &[Value]) -> Vec<Value> {
    let Some(cat) = TOOL_CATEGORIES.iter().find(|c| c.key == category) else {
        return all_tools.to_vec();
    };
    all_tools
        .iter()
        .filter(|t| {
            t.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(|n| cat.tools.contains(&n))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16384 is the boundary: at-or-below picks TwoStage, above picks Direct.
    /// Pins the routing threshold so a future tweak can't silently flip
    /// large-context models into the wrong mode.
    #[test]
    fn routing_mode_threshold_at_16k() {
        assert_eq!(get_routing_mode(8192, None), RoutingMode::TwoStage);
        assert_eq!(get_routing_mode(16384, None), RoutingMode::TwoStage);
        assert_eq!(get_routing_mode(16385, None), RoutingMode::Direct);
        assert_eq!(get_routing_mode(131072, None), RoutingMode::Direct);
        assert_eq!(get_routing_mode(0, None), RoutingMode::TwoStage,
            "zero context (unknown) defaults to two-stage — safer for small models");
    }

    /// Env override beats the window-based heuristic in both directions.
    #[test]
    fn env_override_takes_precedence() {
        assert_eq!(get_routing_mode(131072, Some("two_stage")), RoutingMode::TwoStage,
            "two_stage override must force two-stage on large contexts");
        assert_eq!(get_routing_mode(8192, Some("direct")), RoutingMode::Direct,
            "direct override must force direct on small contexts");
    }

    /// Unknown env values fall through to the window-based heuristic — no panic.
    #[test]
    fn unknown_env_override_falls_through() {
        assert_eq!(get_routing_mode(8192, Some("garbage")), RoutingMode::TwoStage);
        assert_eq!(get_routing_mode(131072, Some("garbage")), RoutingMode::Direct);
        assert_eq!(get_routing_mode(8192, Some("")), RoutingMode::TwoStage);
    }

    /// `get_tools_for_category` filters strictly: only tools listed in the
    /// matching category survive.
    #[test]
    fn filter_returns_only_category_tools() {
        let all = vec![
            json!({"type":"function","function":{"name":"read_file"}}),
            json!({"type":"function","function":{"name":"write_file"}}),
            json!({"type":"function","function":{"name":"bash"}}),
            json!({"type":"function","function":{"name":"find_files"}}),
        ];
        let read_tools = get_tools_for_category("read", &all);
        let names: Vec<&str> = read_tools.iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"find_files"));
        assert!(!names.contains(&"write_file"), "write_file must NOT leak into read category");
        assert!(!names.contains(&"bash"), "bash must NOT leak into read category");
    }

    /// Unknown category returns the full toolkit (fail-open) — never an
    /// empty set, otherwise the agent would have no tools and stall.
    #[test]
    fn unknown_category_returns_full_set() {
        let all = vec![
            json!({"type":"function","function":{"name":"read_file"}}),
            json!({"type":"function","function":{"name":"write_file"}}),
        ];
        let out = get_tools_for_category("nonexistent_category", &all);
        assert_eq!(out.len(), all.len(),
            "unknown category must fall back to full set, not empty");
    }

    /// The selector tool's schema must include every defined category.
    /// Anti-regression for missing/stale category list in the schema.
    #[test]
    fn selector_schema_lists_all_categories() {
        let sel = get_category_selector_tool();
        let enum_arr = sel
            .pointer("/function/parameters/properties/category/enum")
            .and_then(|v| v.as_array())
            .expect("selector schema must expose an enum");
        let enum_strs: Vec<&str> = enum_arr.iter().filter_map(|v| v.as_str()).collect();
        for cat in TOOL_CATEGORIES {
            assert!(enum_strs.contains(&cat.key),
                "selector enum missing category {:?}", cat.key);
        }
        assert_eq!(enum_strs.len(), TOOL_CATEGORIES.len(),
            "selector enum length must equal the category table");
    }
}
