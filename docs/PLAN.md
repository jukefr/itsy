# Plan: Fix Remaining Debt

Four items still open. Estimated effort from smallest to largest.

---

## D1 — Wire `exec_close_contract` (already done, 5m)

**Already fixed** during warning cleanup earlier — the dispatch entry was added
to `executor.rs`. No remaining work.

---

## D2 — Heuristic token estimation (2-4h)

**Problem:** `estimate_message_tokens` in `session/tokens.rs` uses chars/4.
Compaction decisions (`maybe_compact`, `mid_turn_evict`) fire at wrong times.

**Solution:** Replace with a real tokenizer so compaction matches actual API usage.

**Options:**
- **A** (recommended): `tiktoken-rs` — same tokenizer OpenAI/Qwen use. Adds one
  dependency, ~100 lines of integration. Handles all common model families.
- **B:** `tokenizers` — HuggingFace tokenizers, heavier dependency, supports
  arbitrary models but overkill for this use case.

### Steps

1. Add `tiktoken-rs` to `Cargo.toml`
2. Create `crates/itsy/src/session/tokenizer.rs`:
   - `pub fn estimate_tokens(text: &str, model: &str) -> u64`
   - Caches the tokenizer instance per model name
   - Falls back to chars/4 when model is unknown
3. Update `estimate_message_tokens` to call the new function instead of chars/4
4. Verify `maybe_compact` and `mid_turn_evict` make better decisions
5. `cargo check && cargo test`

---

## D3 — Global static singletons → testable interfaces (4-6h)

**Problem:** `get_read_tracker()`, `get_snapshot_manager()`,
`get_file_state_tracker()`, `get_knowledge_loader()` are all `OnceLock`
globals. No way to isolate tests.

**Solution:** Replace each with a trait + injectable instance on `AgentSession`.

### Steps

**D3a. Define traits**

```rust
// session/file_state.rs
pub trait FileStateProvider: Send + Sync {
    fn record(&self, path: &Path, content: &str) -> RecordResult;
    fn record_write(&self, path: &Path, content: &str);
    fn get_original(&self, path: &Path) -> Option<String>;
}
```

Same pattern for `ReadTracker`, `SnapshotManager`, `KnowledgeLoader`.

**D3b. Move instance to AgentSession**

Add fields to `AgentSessionShared`:
```rust
pub read_tracker: Box<dyn ReadTracker>,
pub snapshot_manager: Box<dyn SnapshotManager>,
pub file_state: Box<dyn FileStateProvider>,
pub knowledge_loader: Box<dyn KnowledgeLoader>,
```

Remove the global `OnceLock` + `get_*()` functions. Update every call site
from `get_read_tracker().record_read(...)` to
`session.shared.read().read_tracker.record_read(...)`.

**D3c. Provide default implementations**

The existing structs become the default implementations. A test can swap in
a mock:

```rust
struct MockReadTracker { ... }
impl ReadTracker for MockReadTracker { ... }
```

**D3d. Update call sites**

`executor.rs`, `tools_impl/*.rs`, and `knowledge.rs` all call the globals.
Replace with compartment access threaded through `ExecCtx` or passed as
parameters.

**D3e. Delete `OnceLock` static + `get_*()` functions.**

---

## D4 — Inline JSON tool schemas → typed structs (6-8h)

**Problem:** `TOOLS` and `COMPOUND_TOOLS` in `tools.rs` are `Lazy<Vec<Value>>`
with giant `json!()` literals. No compile-time checking, hard to diff,
duplicated for compound tools.

**Solution:** Define each tool as a typed struct with `#[derive(Serialize)]`,
derive JSON schema from the type.

### Steps

**D4a. Define tool types**

```rust
// tools.rs or tools/schemas.rs
pub struct BashTool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value, // still Value for now (complex nested schema)
}

pub struct ReadFileTool { ... }
// one struct per tool (~20 tools)
```

**D4b. Define a proc-macro or helper**

The plan from the debt entry suggested deriving JSON schema from the type.
Since `parameters` are deeply nested JSON with `oneOf`, `enum`, `pattern`, etc.,
the pure-Rust approach is to keep `parameters: Value` but define the tool
metadata (name, description) as typed constants.

**Simpler approach (recommended):**

```rust
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_fn: fn() -> Value,  // lazy init
}
```

Replace `json!()` literals with `serde_json::from_str(include_str!("..."))` or
lazy builders. The key win is compile-time checking of the metadata.

**D4c. Merge TOOLS + COMPOUND_TOOLS**

Compound tools reference the same schemas. Merge into a single registry that's
easier to maintain.

**D4d. Add a test that validates all tool schemas are well-formed**

```rust
#[test]
fn all_tools_have_valid_schemas() {
    for tool in TOOLS.iter() {
        let name = tool.pointer("/function/name").and_then(|v| v.as_str());
        assert!(name.is_some(), "each tool needs a name");
        // validate structure
    }
}
```

---

## D5 — Unauthenticated subprocess execution (4-6h)

**Problem:** `verify_code` and `evaluator_run_bash` launch subprocesses on
user-supplied paths with no sandboxing. Dead timeout parameter.

### Steps

**D5a. Wire the timeout parameter**

```rust
// governor.rs - run_with_timeout
use std::time::Duration;

fn run_with_timeout(cmd: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<(), String> {
    let child = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .spawn()
        .map_err(|e| e.to_string())?;
    
    // Use tokio::time::timeout or nix timer
    match wait_with_timeout(child, timeout) {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err("non-zero exit".into()),
        Err(_) => Err("timed out".into()),
    }
}
```

**D5b. Merge evaluator_run_bash into the main exec_bash path**

`evaluator_run_bash` in `bin/itsy.rs` is a standalone duplicate of `exec_bash`
with no safety checks. Replace it with a call through `execute_tool("bash", ...)`
or through the persistent shell session.

**D5c. Restrict verification compilers to a temp sandbox**

```rust
fn verify_code(file_path: &str) -> VerifyResult {
    let sandbox = tempfile::tempdir().ok()?;
    // Copy file to sandbox
    // Run compiler in sandbox
    // Delete sandbox
}
```

---

## D6 — Consolidate tool routing (3-5h)

**Problem:** 5 modules contribute to the "what tools does the model see?"
decision, with the affirmation guard in `bin/itsy.rs` able to silently
override the classifier.

### Steps

**D6a. Create single entry point in `runtime/tool_router.rs`**

```rust
pub struct RoutingDecision {
    pub category: String,
    pub tools: Vec<&'static str>,
    pub confidence: f64,
}

pub fn classify_and_filter(
    message: &str,
    prior_category: Option<&str>,
) -> RoutingDecision {
    // 1. Affirmation guard
    if is_affirmation(message) {
        if let Some(cat) = prior_category {
            if cat != "respond" {
                return RoutingDecision { category: cat.into(), tools: get_tools_for_category(cat), confidence: 1.0 };
            }
        }
        return RoutingDecision { category: "plan".into(), tools: get_tools_for_category("plan"), confidence: 1.0 };
    }
    // 2. Respond override
    let cls = classify_tool_category(message);
    if cls.category == "respond" && cls.confidence > 0.0 {
        return RoutingDecision { category: "respond".into(), tools: vec![], confidence: cls.confidence };
    }
    // 3. Normal classification
    RoutingDecision { category: cls.category, tools: get_tools_for_category(&cls.category), confidence: cls.confidence }
}
```

**D6b. Remove inline logic from `bin/itsy.rs`**

Replace the ~60-line routing block in `handle_turn` with a single call:

```rust
let route = itsy::runtime::tool_router::classify_and_filter(&user_msg, session.current_tool_category.as_deref());
```

**D6c. Keep task-type classification separate**

`classify_task` / `classify_task_compiled` determines the *task type* for
system-prompt selection, not the *tool set*. These are different concerns
and can stay in `cognition_adapter.rs` / `governor.rs`.

---

## Effort summary

| Item | Effort | Risk | Priority |
|------|--------|------|----------|
| D1 (close_contract) | 0h | none | ✅ done |
| D2 (tokenizer) | 2-4h | low | Medium |
| D3 (static singletons) | 4-6h | medium | High (enables testing) |
| D4 (tool schemas) | 6-8h | low | Low |
| D5 (subprocess safety) | 4-6h | medium | High (security) |
| D6 (consolidate routing) | 3-5h | medium | Low |

**Recommended order:** D3 → D5 → D2 → D6 → D4

D3 first because it enables testing everything else. D5 for security. D2 is
small and isolated. D6 and D4 are nice-to-haves with lower impact.
