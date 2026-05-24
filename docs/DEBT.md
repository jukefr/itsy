# Technical Debt

Structural issues identified during a codebase review of the agent loop,
orchestration, and supporting infrastructure. Prioritised by impact on
correctness and future maintainability.

---

## 1. `handle_turn` — 1500+ line monolith

### Path
`crates/itsy/src/bin/itsy.rs` — function `handle_turn` (lines ~650–2200)

### Problem
The entire agent loop (clarifier, reference resolution, contract reframing,
classification, routing, quality monitoring, gate checks, dedup, tool execution,
improvement loops, verification, badger, text-only streak detection, contract
close-loop, and auto-commit) lives in one nested `loop { match name { ... } }`
with interleaved `break`/`continue` paths.

No single path through the function can be reasoned about in isolation because
every guard (early-stop, quality-monitor, contract-loop, empty-retry, badger,
text-only-streak) can inject a `[SYSTEM]` user message and `continue`, making
it impossible to predict which fires first.

### Impact
- Modifications risk breaking subtle interactions between guards.
- Unit testing is infeasible.
- Cyclomatic complexity is extreme: the function contains ~20 `continue` points
  and ~10 `break` points, with conditionals that span hundreds of lines.

### Suggested approach
Extract each guard into its own module or function. The main loop body should
be a sequence of `if guard.tripped() { guard.inject(); continue; }` calls, not
200-line inline blocks.

---

## 2. Dual config state — `Config` vs `Settings`

### Path
`crates/itsy/src/config.rs` — struct `Config`
`crates/itsy/src/settings.rs` — struct `Settings`
`crates/itsy/src/bin/itsy.rs` — `main()` (lines ~3100–3200)

### Problem
Two config representations co-exist:

- `Config` — threaded through most `&Config` references; deserialised from
  `config.toml`, layered with CLI flags.
- `Settings` — global `OnceLock<RwLock<>>`, populated after `Config` is built.

The `main()` function pipelines CLI overrides into *both* structs with
duplicate assignments:

```rust
config.limits.max_tool_calls_per_turn = s.max_tool_calls_per_turn;
config.tools.bash_timeout = s.bash_timeout;
// ...
```

Code reading from `settings::get()` and code using `&Config` see different
values the moment a slash command calls `settings::update()` — `Config` does
not receive the mutation.

### Impact
Stale reads in modules that thread `&Config`. A user runs `/web on` at the
REPL; `executor.rs` (which receives `&Config`) still sees `web_browse: false`.

### Suggested approach
Pick one. The global `Settings` is already used by hot-path code in
`executor.rs`, `bin/itsy.rs`, and `session/contract.rs`. Eliminate `&Config`
references in favour of `settings::get()` and remove the dual-write path.

---

## 3. 17 `Arc<Mutex<T>>` fields with nested locking

### Path
`crates/itsy/src/bin/itsy.rs` — struct `AgentSession` (lines ~170–200)

### Problem
Every piece of session state lives behind `Arc<Mutex<_>>`:

```rust
struct AgentSession {
    config: Arc<Mutex<Config>>,
    history: Arc<Mutex<Vec<Value>>>,
    memory: Arc<Mutex<MemoryStore>>,
    tokens: Arc<Mutex<TokenTracker>>,
    token_monitor: Arc<Mutex<TokenMonitor>>,
    sessions: Arc<Mutex<SessionStore>>,
    scorer: Arc<Mutex<ToolScorer>>,
    verification: Arc<Mutex<VerificationHistory>>,
    early_stop: Arc<Mutex<EarlyStopDetector>>,
    trace: Arc<Mutex<TraceRecorder>>,
    skills: Arc<Mutex<SkillManager>>,
    plugins: Arc<Mutex<PluginLoader>>,
    mcp_bridge: Arc<McpBridge>,
    cwd: PathBuf,
    current_tool_category: Arc<Mutex<Option<String>>>,
    fullscreen: Arc<Mutex<Option<Arc<Fullscreen>>>>,
    tool_repeat_counts: Arc<Mutex<HashMap<String, u32>>>,
    mutated_paths: Arc<Mutex<HashSet<String>>>,
    bash_loop_keys: Arc<Mutex<HashSet<String>>>,
    readonly_turn_count: Arc<Mutex<u32>>,
    total_mutating_calls: Arc<Mutex<u32>>,
}
```

The code acquires several in sequence within `handle_turn`:

```rust
let cfg = session.config.lock();
let hist = session.history.lock();
// ...
```

`parking_lot::Mutex` is not fair and does not lock in order. The code relies on
explicit `drop()` calls before every `.await`, which is correct today but any
future refactor that inserts an `.await` before a `drop()` deadlocks at runtime
with no compile-time protection.

### Impact
Latent deadlock on the next significant refactor. Testing for deadlocks is
hard; the first sign is a hung agent in production.

### Suggested approach
- Merge session state into a single struct behind one mutex.
- Or migrate fields that are read-mostly (config, tokens, skills, plugins) to
  `RwLock`.
- Or restructure so that `.await` boundaries never cross lock acquisitions.

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

These are:
- Unchecked at compile time — a missing comma or misspelled field name
  surfaces as a runtime panic when the static is first accessed.
- Hard to diff (PRs show hundreds of lines of JSON noise).
- Duplicated in `COMPOUND_TOOLS`.

### Impact
Maintainability hazard. Adding a new tool requires editing two list literals
with no structural check that they stay in sync.

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
`crates/itsy/src/bin/itsy.rs` — function `estimate_message_tokens` (lines ~240–270)

### Problem
The entire context compaction system (`maybe_compact`, `mid_turn_evict`) bases
eviction decisions on a chars/4 heuristic:

```rust
fn estimate_message_tokens(m: &Value) -> u64 {
    let content_chars = ...;
    ((content_chars + tc_chars) as f64 / 4.0).ceil() as u64
}
```

For code-heavy conversations (chars/token ≈ 2–3) this underestimates; for
JSON-heavy ones (chars/token ≈ 6–8) it overestimates. At IQ2_XXS, every token
of context matters — over-eviction loses working memory, under-eviction hits
the API's context limit.

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
The affirmation guard in `bin/itsy.rs` can silently override the two-stage
router result:

```rust
if is_affirmation(&user_msg) && current.as_deref().is_some() && current.as_deref() != Some("respond") {
    current.clone()
}
```

### Impact
To predict what tool set the model will receive, you need to trace through
five files and understand the priority order. Edge cases (e.g. an affirmation
after a `respond`-classified turn) are non-obvious.

### Suggested approach
Consolidate routing into `runtime/tool_router.rs` with a single
`classify_and_filter` entry point. Remove the inline logic from
`bin/itsy.rs`.

---

## 8. Single-file binary with ALL logic

### Path
`crates/itsy/src/bin/itsy.rs` — 3550 lines

### Problem
The single `bin/itsy.rs` contains:
- CLI parsing
- Agent loop (handle_turn)
- System prompt assembly (6+ distinct functions)
- History compaction (2 functions)
- Tool skill card selection
- Empty-response retry logic
- Adversarial evaluator (5 functions)
- MCP server (5 functions)
- REPL loops (2 functions)
- Eval runner dispatcher
- Boot/init logic
- Main()

None of these are in the library crate.

### Impact
- No isolation: every function here is coupled to every other via shared
  `AgentSession` state.
- Testing requires launching the full binary.
- Library crate (`lib.rs`) is missing core orchestration logic that ought to be
  reusable.

### Suggested approach
Move agent orchestration into the library crate under a module like
`runtime::agent_loop`. Leave CLI parsing, REPL loop, and main() in the binary.

---

## 9. Global static singletons prevent testing

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

---

## 10. Per-call string allocations in result processing

### Path
`crates/itsy/src/security.rs` — functions `strip_ansi`, `redact_string`, `sanitize_tool_output`

### Problem
Every tool result goes through `sanitize_tool_output` in `exec_read_file`,
which chains three allocations:

```rust
pub fn sanitize_tool_output(input: &str) -> String {
    let stripped = strip_ansi(input);  // alloc #1
    let redacted = redact_string(&stripped);  // alloc #2
    redacted.replace("\r\n", "\n")  // alloc #3
}
```

In the agent loop, every tool result is also run through `cap_tool_result`,
which allocates *another* `String`. For a turn with 50 tool calls, that's
~200 intermediate allocations just for output processing.

### Impact
Moderate GC pressure. Not a bottleneck for correctness, but the project ethos
is "no unnecessary copies."

### Suggested approach
- Skip ANSI stripping for known-safe outputs (e.g. `write_file` returns
  structured JSON, not raw terminal output).
- Consider a `Cow<str>` path that returns the input when no transformation
  is needed.

---

## Low-priority observations

- The `timeout` parameter in `run_with_timeout` is accepted but never applied
  to the subprocess — it is dead code.
- `HardFailAction::Retry { escalate }` is destructured with `let _ = (attempt, escalate);`
  — the field is never used.
- Several `eprintln!` calls with raw `\x1b[` color codes co-exist with the
  `tui::*` formatting helpers; some paths use one, some the other.
- The `contract.rs` `set_active_id` writes to a shared `.active` marker file
  with no locking — two sessions racing on the same project dir could clobber
  each other's contract.
- The feature-adapter LLM calls (`call_prompt`) swallow errors silently — any
  failure returns `None`/`false`, masking potential issues with the compiled
  feature prompts.
