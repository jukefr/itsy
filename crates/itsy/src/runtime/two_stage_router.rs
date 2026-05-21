//! Lives under `runtime` alongside the other deterministic helpers.

use serde_json::{json, Value};

pub struct CategoryDef {
    pub key: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
}

pub const TOOL_CATEGORIES: &[CategoryDef] = &[
    CategoryDef { key: "read", description: "Read file contents, find files by pattern", tools: &["read_file", "find_files", "find_and_read"] },
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
