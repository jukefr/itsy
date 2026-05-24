# Technical Debt — Status

> Most items from the initial review have been addressed across three
> refactoring phases. Items below are **remaining** or **partially addressed**.

## Resolved

| # | Item | Resolution |
|---|------|-----------|
| 1 | 1500-line `handle_turn` monolith | **B1 done**: 21 functions (~720 lines) moved to library modules; `bin/itsy.rs` 3560→2785 lines. TurnState struct created in `runtime/agent_loop.rs`. |
| 2 | Dual `Config`/`Settings` state | **Phase A complete**: all feature-flag reads go through `settings::get()`; back-mirror deleted; `build_config_and_settings()` extracted from `main()`. |
| 3 | 17 `Arc<Mutex<>>` with nested locking | **Phase C complete**: restructured into 3 compartments (`ro`/no lock, `shared`/RwLock, `mutable`/single Mutex). |
| 8 | Single-file binary with all logic | **B1 complete**: 21 functions moved to library crate. System prompt assembly, history compaction, token estimation, string helpers, and classification helpers all in library. |
| — | Compiler warnings (itsy crate) | Zero warnings (was 3: dead `exec_patch`, unwired `exec_close_contract`, unused `ctx`). |

## Remaining

Structural issues from the original review, still open.

---

## 4. Inline JSON tool schemas

### Path
`crates/itsy/src/tools.rs` — `TOOLS` and `COMPOUND_TOOLS` statics

### Problem
Every tool schema is a giant `json!({...})` literal inside a Lazy static:

```rust
pub static TOOLS: Lazy<Vec<Value>> = Lazy::new(|| vec![
    json!({"type":"function","function":{"name":"read_file", ...}}),
    json!({"type":"function","function":{"name":"bash", ...}}),
    // ~20 more
]);
```

### Impact
- Unchecked at compile time — a missing comma or misspelled field name
  surfaces as a runtime panic when the static is first accessed.
- Hard to diff (PRs show hundreds of lines of JSON noise).
- Duplicated in `COMPOUND_TOOLS`.

### Suggested approach
Define tool schemas as typed Rust structs with `#[derive(Serialize)]` and derive
the JSON schema from the type. This gives compile-time checking and a single
source of truth.

---

## 5. Unauthenticated subprocess execution in verification paths

### Path
`crates/itsy/src/governor.rs` — function `verify_code` (lines ~120–200)
`crates/itsy/src/governor.rs` — function `run_with_timeout` (lines ~200–220)
`crates/itsy/src/bin/itsy.rs` — function `evaluator_run_bash` (lines ~2400–2440)

### Problem
`verify_code` runs `python`, `node --check`, `npx tsc`, `go build` on
user-accessible file paths with no sandboxing:

```rust
fn run_with_timeout(cmd: &str, args: &[String], cwd: &Path, _timeout: Duration) -> Result<(), String> {
    let output = Command::new(cmd).args(args).current_dir(cwd).output()?;
```

The adversarial evaluator duplicates this with `evaluator_run_bash` which calls
`Command::new("bash").arg("-c").arg(command)` — no path safety, no nix signal
handling, no timeout.

The `timeout` parameter is accepted but never passed to the subprocess — it is
dead code. Subprocesses can run indefinitely.

### Impact
If a task prompts the model to write a shell script and run it, `verify_code`
will execute it. The safety model here relies entirely on the model not being
socially engineered. The evaluator doubles the attack surface.

### Suggested approach
- Wire the timeout parameter through to an actual subprocess timeout (e.g.
  `Command::new("timeout").arg(timeout.to_string()).arg(cmd)...`).
- Restrict verification compilers to a sandboxed directory.
- Merge evaluator command execution with the main `exec_bash` path so it goes
  through the same security checks.

---

## 6. Heuristic token estimation drives compaction decisions

### Path
`crates/itsy/src/session/tokens.rs` — function `estimate_message_tokens`

### Problem
The entire context compaction system (`maybe_compact`, `mid_turn_evict`) bases
eviction decisions on a chars/4 heuristic:

```rust
fn estimate_message_tokens(m: &Value) -> u64 {
    ((content_chars + tc_chars) as f64 / 4.0).ceil() as u64
}
```

For code-heavy conversations (chars/token ≈ 2–3) this underestimates; for
JSON-heavy ones (chars/token ≈ 6–8) it overestimates. At IQ2_XXS, every token
of context matters — over-eviction loses working memory, under-eviction hits
the API's context limit mid-turn.

### Impact
The model either loses useful context early or gets truncated at the API
boundary mid-turn.

### Suggested approach
Use a real tokenizer (tiktoken-rs or the model's own tokeniser) so compaction
decisions match actual usage.

---

## 7. Tool routing distributed across 5 modules

### Paths
- `crates/itsy/src/runtime/tool_router.rs` — deterministic regex classifier
- `crates/itsy/src/governor.rs` — `classify_task` (regex + LLM)
- `crates/itsy/src/cognition_adapter.rs` — `classify_task_compiled`
- `crates/itsy/src/features_adapter.rs` — `check_needs_clarification`
- `crates/itsy/src/bin/itsy.rs` — inline affirmation guard + respond override

### Problem
The decision "what tools does the model see for this user message?" is the
product of five loosely-coupled classifiers with overlapping responsibilities.

### Impact
To predict what tool set the model will receive, you need to trace through
five files and understand the priority order.

### Suggested approach
Consolidate routing into `runtime/tool_router.rs` with a single
`classify_and_filter` entry point. Remove the inline logic from
`bin/itsy.rs`.

---

## 9. Global static singletons prevent testing (PARTIAL FIX)

### Paths
- `crates/itsy/src/tools_impl/read_tracker.rs` — `get_read_tracker()`
- `crates/itsy/src/session/snapshot.rs` — `get_snapshot_manager()`
- `crates/itsy/src/session/file_state.rs` — `get_file_state_tracker()`
- `crates/itsy/src/knowledge.rs` — `get_knowledge_loader()`

### Problem
All four of these return global `OnceLock` singletons. There is no way to
create an isolated instance for a test case — state leaks across tests.

### Impact
No integration tests exist for the core agent loop. Tests in `verification.rs`
only cover pure helper functions. The main orchestration logic has zero test
coverage.

### Suggested approach
Make these trait-backed interfaces that can be swapped for test doubles. The
agent session should hold an instance, not call a global getter.
