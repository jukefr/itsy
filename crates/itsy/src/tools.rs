//! Tool schemas sent to the model + the 2-stage
//! routing entry point. Tool execution lives in [`crate::executor`].

use std::env;

use once_cell::sync::Lazy;
use serde_json::{json, Value};

use crate::runtime::two_stage_router::{
    get_category_selector_tool, get_routing_mode, get_tools_for_category as two_stage_for_category,
    RoutingMode,
};
use crate::runtime::tool_router::{category_needs_tools, get_tools_for_category as compiled_for_category};
use crate::Config;

pub static TOOLS: Lazy<Vec<Value>> = Lazy::new(|| vec![
    func_tool("list_projects",
        "List all indexed projects/repos in the workspace with stats: file count, symbol count, lines of code, languages. Use this FIRST when asked about \"the projects\", \"the codebase\", or \"what's in this workspace\".",
        json!({"type":"object","properties":{},"required":[]})),
    func_tool("graph_search",
        "Search the code graph for a symbol, function, or class name. Returns connected code with context. Use for \"how does X work\" or \"find the auth logic\" — NOT for listing projects.",
        json!({"type":"object","properties":{"query":{"type":"string","description":"Symbol name or concept to search for"},"max_tokens":{"type":"integer","description":"Max tokens to return (default 4000)"}},"required":["query"]})),
    func_tool("explain_symbol",
        "Get full explanation of a symbol: signature, location, callers, callees, and where it fits in the architecture. Use for \"what does X do\" questions.",
        json!({"type":"object","properties":{"symbol":{"type":"string","description":"Symbol name to explain"}},"required":["symbol"]})),
    func_tool("memory_load",
        "Load relevant project memory for a task. Returns past decisions, workflows, conventions, and gotchas. Call this before starting complex work.",
        json!({"type":"object","properties":{"task":{"type":"string","description":"Task description to find relevant context for"}},"required":["task"]})),
    func_tool("read_file",
        "Read a file. Returns content with line numbers.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to cwd"},"start_line":{"type":"integer","description":"Start line (optional)"},"end_line":{"type":"integer","description":"End line (optional)"}},"required":["path"]})),
    func_tool("read_original",
        "Read a file as it was when first read this session — before any edits. \
        Use to check what you changed or to recover original content. \
        Only works for files that were already read with read_file.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to cwd"}},"required":["path"]})),
    func_tool("write_file",
        "Create a NEW file. Fails if the file already exists — use read_and_patch to edit existing files. LIMIT: 60 lines / 8KB max; use append_file for additional sections.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path (must not already exist)"},"content":{"type":"string","description":"File content — keep under 60 lines"}},"required":["path","content"]})),
    func_tool("append_file",
        "Append content to the end of an existing file. Use this to build large files in chunks — write_file for the first 50 lines, then append_file for each subsequent section.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path to append to"},"content":{"type":"string","description":"Content to append — keep under 60 lines per call"}},"required":["path","content"]})),
    func_tool("bash",
        "Run a shell command. Returns stdout/stderr.",
        json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command"}},"required":["command"]})),
    func_tool("search",
        "Search file contents using regex (ripgrep). Returns matching lines.",
        json!({"type":"object","properties":{"pattern":{"type":"string","description":"Regex pattern"},"path":{"type":"string","description":"Directory to search (default: .)"}},"required":["pattern"]})),
    func_tool("find_files",
        "Find files matching a glob pattern.",
        json!({"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern e.g. **/*.ts"}},"required":["pattern"]})),
    func_tool("memory_remember",
        "Save durable knowledge to project memory. Only save facts that should persist: decisions, workflows, gotchas, conventions. NOT task transcripts.",
        json!({"type":"object","properties":{"type":{"type":"string","enum":["decision","workflow","gotcha","convention","context"],"description":"Knowledge type"},"title":{"type":"string","description":"Short title"},"content":{"type":"string","description":"The knowledge"},"tags":{"type":"array","items":{"type":"string"},"description":"Tags"}},"required":["type","title","content"]})),
    func_tool("web_search",
        "Search the internet for information. Requires ITSY_WEB_BROWSE=true.",
        json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]})),
    func_tool("web_fetch",
        "Fetch and extract readable text content from a URL. Requires ITSY_WEB_BROWSE=true.",
        json!({"type":"object","properties":{"url":{"type":"string","description":"URL to fetch"}},"required":["url"]})),
    func_tool("memory_list",
        "List all stored memory objects. Optionally filter by type.",
        json!({"type":"object","properties":{"type":{"type":"string","description":"Filter by type: decision, workflow, gotcha, convention, context (optional)"}},"required":[]})),
    func_tool("memory_forget",
        "Delete a memory object by ID.",
        json!({"type":"object","properties":{"id":{"type":"string","description":"Memory object ID to delete"}},"required":["id"]})),

    // ── contract: the agent's definition-of-done for the current task ──
    func_tool("propose_contract",
        "MUST be your first tool call on any action task. Records the definition-of-done as a list of \
        testable assertions, each ≤120 chars. After this returns, `write_file` / `read_and_patch` / mutating \
        `bash` become available.\n\n\
        EXAMPLE — for the prompt \"write /app/foo.py that prints 42\":\n\
        {\n\
          \"title\": \"foo.py prints 42\",\n\
          \"brief\": \"Create /app/foo.py so that `python3 /app/foo.py` writes `42` to stdout.\",\n\
          \"assertions\": [\n\
            {\"id\": \"A.001\", \"text\": \"/app/foo.py exists\"},\n\
            {\"id\": \"A.002\", \"text\": \"running /app/foo.py prints exactly '42'\"}\n\
          ]\n\
        }\n\n\
        Rules: 2-6 assertions, each a single concrete check (file exists / a command exits 0 / output \
        contains X). NO vague ones like \"works correctly\". Skip wrapping prose.",
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Short human title (≤60 chars)"},
                "brief": {"type": "string", "description": "1-3 paragraph plan: what you'll do, the constraints, the shape of done."},
                "assertions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Stable ID, e.g. \"A.001\""},
                            "text": {"type": "string", "description": "Single testable statement, ≤120 chars"}
                        },
                        "required": ["id", "text"]
                    }
                },
                "features": {
                    "type": "array",
                    "description": "Optional sub-tasks. Each can fulfill one or more assertion IDs.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "description": {"type": "string"},
                            "fulfills": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["id", "description"]
                    }
                }
            },
            "required": ["title", "brief", "assertions"]
        })),
    func_tool("mark_assertion",
        "Mark a contract assertion `passed` or `failed` with evidence. \
        `passed` requires evidence + a concrete verification command. If an external verifier script exists, \
        plan to use that project-native check before the final `close_contract` — do not rely only on ad-hoc spot checks. \
        Lazy observations like \"OK\" or \"tests passed\" are rejected — write what you actually saw.",
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Assertion ID, e.g. \"A.001\""},
                "state": {"type": "string", "enum": ["passed", "failed"]},
                "evidence": {"type": "string", "description": "Plain-text explanation of how you verified (≥10 chars)"},
                "command": {"type": "string", "description": "Command you ran (optional but encouraged for `passed`)"},
                "exit_code": {"type": "integer", "description": "Exit code of the command"},
                "observation": {"type": "string", "description": "Specific output you saw — NOT \"passed\""}
            },
            "required": ["id", "state", "evidence"]
        })),
    func_tool("mark_feature",
        "Mark a contract feature `in_progress` / `done` / `cancelled`. Optional bookkeeping; not required.",
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "state": {"type": "string", "enum": ["in_progress", "done", "cancelled"]}
            },
            "required": ["id", "state"]
        })),
    func_tool("contract_status",
        "Read the current contract state — which assertions are passed / failed / pending.",
        json!({"type": "object", "properties": {}})),
    func_tool("close_contract",
        "Finalize the active contract as `completed`. Refused unless every assertion is `passed`, and when an external verifier command exists you must have run it on the final state before closing.",
        json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["completed"]}
            },
            "required": ["status"]
        })),
]);

pub static COMPOUND_TOOLS: Lazy<Vec<Value>> = Lazy::new(|| vec![
    func_tool("read_and_patch",
        "Edit an existing file: reads its current content, then replaces old_str with new_str. old_str must match exactly ONE location. Chain multiple calls for sequential edits — no read_file needed between them.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"old_str":{"type":"string","description":"Exact text to find and replace"},"new_str":{"type":"string","description":"Replacement text"}},"required":["path","old_str","new_str"]})),
    func_tool("create_and_run",
        "Create a file and then run a command (like running the file or running tests). Equivalent to write_file + bash in one call.",
        json!({"type":"object","properties":{"path":{"type":"string","description":"File to create"},"content":{"type":"string","description":"File content"},"command":{"type":"string","description":"Command to run after creating"}},"required":["path","content"]})),
    func_tool("find_and_read",
        "Find files matching a pattern and read the first match. Equivalent to find_files + read_file in one call.",
        json!({"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern (e.g. **/main.ts, src/**/*.py)"},"read_lines":{"type":"integer","description":"Max lines to show from matched file. Default: 50"}},"required":["pattern"]})),
    func_tool("search_and_read",
        "Search for a pattern in code, then read the most relevant file found. Equivalent to search + read_file in one call.",
        json!({"type":"object","properties":{"pattern":{"type":"string","description":"Regex to search for"},"read_context":{"type":"integer","description":"Lines of context around matches. Default: 10"}},"required":["pattern"]})),
    func_tool("run",
        "Run an existing file (python, node, etc). Use this instead of create_and_run when the file already exists.",
        json!({"type":"object","properties":{"command":{"type":"string","description":"Command to run e.g. \"python game.py\" or \"node server.js\""},"timeout":{"type":"integer","description":"Timeout in seconds. Default: 30"}},"required":["command"]})),
]);

fn func_tool(name: &str, description: &str, params: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": params,
        }
    })
}

#[derive(Default)]
pub struct ToolDeps {
    pub plugin_tools: Vec<Value>,
    pub mcp_tools: Vec<Value>,
}

/// Mirror of `getAllTools` — routing-aware selection of tool schemas.
pub fn get_all_tools(config: &Config, stage2_category: Option<&str>, deps: &ToolDeps) -> Vec<Value> {
    let mut all_tools: Vec<Value> = TOOLS.clone();
    all_tools.extend(COMPOUND_TOOLS.iter().cloned());
    all_tools.extend(deps.plugin_tools.iter().cloned());
    all_tools.extend(deps.mcp_tools.iter().cloned());

    if let Some(cat) = stage2_category {
        if !category_needs_tools(cat) {
            return Vec::new();
        }
        let allowed = compiled_for_category(cat);
        if !allowed.is_empty() {
            let filtered: Vec<Value> = all_tools
                .iter()
                .filter(|t| {
                    t.pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .map(|n| allowed.contains(&n))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        }
    }

    let context_window = config.context.detected_window;
    let routing_override = env::var("ITSY_TOOL_ROUTING").ok();
    let mode = get_routing_mode(context_window, routing_override.as_deref());

    if context_window <= 16384 && mode == RoutingMode::TwoStage && stage2_category.is_none() {
        return vec![get_category_selector_tool()];
    }
    if mode == RoutingMode::TwoStage && stage2_category.is_none() {
        return vec![get_category_selector_tool()];
    }
    if mode == RoutingMode::TwoStage {
        if let Some(cat) = stage2_category {
            return two_stage_for_category(cat, &all_tools);
        }
    }
    all_tools
}
