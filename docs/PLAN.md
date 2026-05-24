# Plan: Fix Blocking Debt

Three interrelated blockers:

| # | Issue | Dependencies |
|---|-------|-------------|
| A | Dual Config/Settings state | None — can be done independently |
| B | 1500-line `handle_turn` monolith | Depends on A (config threading) |
| C | 17 `Arc<Mutex<>>` with nested locking | Depends on B (loop extract creates injector point) |

Execute A first, then B, then C. Dependencies are advisory — B and C have
non-overlapping files so can be done in parallel after A lands.

### Design principle (from Claude Code study)

Claude Code's main loop (`query.ts`) is also large at 785KB — size alone is
not the goal. The critical property is **separation of concerns**: `query.ts`
imports from `services/tools/`, `services/api/`, `tasks/` — it does not contain
tool implementations, CLI parsing, UI rendering, or config management inline.

Itsy's `handle_turn` fails on every axis: it defines system-prompt assembly,
handles CLI parsing edge cases, formats UI output, manages config state, and
runs tool execution — all with inline conditionals. The fix is not to shrink
the function; it's to ensure that every concern lives in exactly one module,
and `handle_turn` only orchestrates the transition between them.

The guard/middleware pattern (Claude Code's `canUseTool()` pipeline) is the
right model for itsy's guards. Each guard should be a function that receives
the current state and returns a decision, not an inline conditional that also
builds the error message and pushes to history.


---

## Phase A — Eliminate dual config state

### Goal

All runtime code reads from `settings::get()`. No module receives `&Config`
for anything that is also in `Settings`. The back-mirror in `main()` (lines
3460–3475) is deleted.

### Steps

**A1. Add missing fields to `Settings`**

`ContextConfig` fields are consumed via `&Config` but absent from `Settings`
(the only fields Settings lacks that &Config readers use):

```
settings.detected_window  ← config.context.detected_window
settings.max_budget_pct   ← config.context.max_budget_pct
```

Add them to `Settings` struct, `defaults()`, and `from_full_config()`.

(Everything else `&Config` consumers read is already mirrored in Settings —
features flags, model name/url, limits, etc.)

**A2. Migrate `model_client.rs` away from `&Config`**

`ChatContext` holds `&Config`. In `chat_completion`:

| Field | New source |
|-------|-----------|
| `config.model.name` | `settings::get().model_name` |
| `config.model.base_url` | `settings::get().base_url` |
| `config.model.timeout` | `settings::get().request_timeout_ms` → convert to Duration |
| `config.features.temp_adapt` | `settings::get().temp_adapt` |
| `config.features.thinking_budget` | used through `thinking_budget::thinking_budget` which already reads from settings |

Delete `config` field from `ChatContext`. Thread only what isn't in Settings
through explicit parameters (model name, base URL, timeout).

**A3. Migrate `executor.rs` away from `&Config`**

`ExecCtx` holds `&Config`. Two reads:

| Location | Field | New source |
|----------|-------|-----------|
| `exec_patch` | `config.features.semantic_merge` | `settings::get().semantic_merge` |
| `exec_bash` | `config.features.error_diagnosis` | `settings::get().error_diagnosis` |
| `exec_bash` | `config.context.detected_window` | `settings::get().detected_window` (added in A1) |
| `exec_propose_contract` | `config` | threaded to `contract::set_active_id` |
| `exec_mark_assertion` | `config` | threaded to contract load |
| `exec_close_contract` | `config` | threaded to contract load |

Contract executors need `Config` for `config.model.name` + second-opinion
model routing. Move those into explicit parameters.

Delete `config` field from `ExecCtx`.

**A4. Migrate `bin/itsy.rs` helpers away from `&Config`**

Functions that take `&Config`:

| Function | Uses | New source |
|----------|------|-----------|
| `maybe_compact` | `context.detected_window`, `context.max_budget_pct` | `settings::get()` |
| `mid_turn_evict` | `context.detected_window` | `settings::get()` |
| `get_knowledge_context` | `context.detected_window` | `settings::get()` |
| `build_contract_proposal_prompt` | `model.name` | `settings::get().model_name` |
| `build_full_system_prompt` | `features.*`, `context.*` | `settings::get()` |

Also: all `session.config.lock()` reads within `handle_turn` (e.g. checking
`features.clarifier`, `features.snapshot`, `features.validate_edits`,
`features.context_retrieval`, `git.auto_commit`, `context.detected_window`).

Replace each with `settings::get().<param>`.

**A5. Delete back-mirror in `main()`**

After A2–A4 land, remove lines 3460–3475 (the "Mirror CLI overrides back into
config" block) and the corresponding field assignments.

**A6. Extract config-building from `main()`**

Pull the CLI-flag → settings pipeline (lines 3388–3477) into a function:

```rust
fn build_settings(cli: &Cli, config: Config) -> Settings { ... }
```

Keeps `main()` focused on startup orchestration.

### Artifacts changed

`crates/itsy/src/settings.rs` — add fields
`crates/itsy/src/model_client.rs` — delete `config` from `ChatContext`
`crates/itsy/src/executor.rs` — delete `config` from `ExecCtx`; update consumers
`crates/itsy/src/bin/itsy.rs` — replace all `session.config.lock()` reads with
  `settings::get()`; delete back-mirror; extract `build_settings`
`crates/itsy/src/tui.rs` — `render_status`/`render_welcome` take `&Config` for
  model name + cwd → switch to `settings::get()`
`crates/itsy/src/tools.rs` — `get_all_tools` takes `&Config` for web_browse flag
  → switch to `settings::get()`
`crates/itsy/src/model/adaptive_router.rs` — `select_model` takes `&Config` →
  model name/url from `settings::get()`
`crates/itsy/src/model/router.rs` — `route_model`/`route_model_for_message` take
  `&Config` → model tiers from `settings::get()`

---

## Phase B — Extract `handle_turn`

### Goal

`bin/itsy.rs` `handle_turn` shrinks from ~1500 lines to <200. The monolithic
loop body is replaced with a sequence of well-named modules:

```
clarifier.precheck(&user_msg) → Option<response>    // short msg → ask back
turn_state = TurnState::new(user_msg, session)
turn_state.resolve_refs(&cwd)
turn_state.classify()
turn_state.build_prompt(&session)

loop {
    turn_state.chat(&session) → TurnEvent
    match turn_event {
        TurnEvent::ToolCalls(calls) => turn_state.execute_batch(calls, &session),
        TurnEvent::TextResponse(content) => turn_state.handle_text(content, &session),
    }
    if turn_state.should_stop() { break }
}

turn_state.finish(&session)
```

### Steps

**B1. Move helpers into library crate**

Move these from `bin/itsy.rs` into `crates/itsy/src/` (they're already clean
functions with no bin-only deps):

- `estimate_message_tokens` → `session/tokens.rs`
- `estimate_history_tokens` → `session/tokens.rs`
- `cap_tool_result` → `executor.rs`
- `first_n_lines` → `executor.rs`
- `truncate_short` → utility location
- `maybe_compact` → `session/compaction.rs`
- `mid_turn_evict` → `session/compaction.rs`
- `is_affirmation`, `looks_like_path`, `looks_like_option_ref` → `runtime/tool_router.rs`
- `tool_skill_card`, `select_tool_skill_cards` → `runtime/tool_guidance.rs`
- `get_memory_context` → `memory/mod.rs`
- `get_skill_context` → `plugins/skills.rs`
- `get_plugin_prompts` → `plugins/loader.rs`
- `get_knowledge_context` → `knowledge.rs`
- `build_full_system_prompt` → `model/prompts.rs`
- `build_contract_proposal_prompt` → `session/contract/prompts.rs`
- `build_contract_active_prompt` → `session/contract/prompts.rs`

**B2. Extract `TurnState`**

Create `runtime/agent_loop.rs` with a `TurnState` struct that encapsulates
all per-turn mutable state:

```rust
pub struct TurnState<'a> {
    user_msg: String,
    augmented: String,
    task_type: &'static str,
    current_category: Option<String>,
    tool_calls_this_turn: u32,
    force_disable_thinking: bool,
    edited_files: Vec<String>,
    had_mutating_call: bool,
    recent_tool_failures: HashMap<String, u32>,
    per_turn_repeats: HashMap<String, u32>,
    improvement_attempts: HashMap<String, u32>,
    last_prompt_tokens: u64,
    consecutive_blocks: u32,
}
```

Move all per-turn counters out of `handle_turn` locals into `TurnState`.
The session-scoped counters (`tool_repeat_counts`, `readonly_turn_count`,
etc.) stay on `AgentSession`.

**B3. Extract guards into methods**

Each guard becomes a method on `TurnState` that returns `Option<GuardAction>`:

```rust
impl TurnState {
    fn check_clarifier(&mut self, session: &AgentSession) -> Option<GuardAction>;
    fn check_quality_monitor(&mut self, tool_calls: &[Value], known: &[&str]) -> Option<GuardAction>;
    fn check_contract_gate(&mut self, name: &str, task_is_action: bool) -> Option<GuardAction>;
    fn check_loop_detection(&mut self, name: &str, args: &Value, session: &AgentSession) -> Option<GuardAction>;
    fn check_early_stop(&mut self, name: &str, args: &Value, session: &AgentSession) -> Option<GuardAction>;
    fn check_idempotent_write(&mut self, name: &str, args: &Value) -> Option<GuardAction>;
    fn check_no_progress(&mut self, batch_status: &BatchStatus) -> Option<GuardAction>;
    fn check_text_only_streak(&mut self, session: &AgentSession) -> Option<GuardAction>;
    fn check_contract_close_loop(&mut self, session: &AgentSession) -> Option<GuardAction>;
}
```

`GuardAction` is an enum:

```rust
enum GuardAction {
    /// Inject a system message and continue the inner loop.
    Inject { message: String },
    /// Break out of the inner tool-batch loop.
    Break,
    /// Break out of the outer turn loop.
    Stop,
}
```

**B4. Rewrite `handle_turn` as a state machine**

The main loop becomes a sequence of steps, each calling `check_*` and
short-circuiting if any guard fires:

```rust
async fn handle_turn(prompt_in: &str, session: &AgentSession) {
    let mut state = TurnState::new(prompt_in, session);

    // Pre-chat phase: clarifier, references, contract reframe.
    if let Some(action) = state.run_pre_chat(session).await { return; }

    // Main loop.
    loop {
        if state.should_stop() { break; }

        // Build request, call API.
        let response = state.chat(session).await;
        let Some(turn_event) = response else { break; };

        match turn_event {
            TurnEvent::ToolCalls(calls) => {
                state.execute_batch(calls, session).await;
            }
            TurnEvent::TextResponse(content) => {
                state.handle_text_response(content, session).await;
            }
        }
    }

    // Post-turn: contract display, auto-commit.
    state.finish(session).await;
}
```

### Artifacts created

`crates/itsy/src/runtime/agent_loop/mod.rs` — `handle_turn`, `TurnState`
`crates/itsy/src/runtime/agent_loop/guards.rs` — guard methods
`crates/itsy/src/runtime/agent_loop/execution.rs` — `execute_batch`
`crates/itsy/src/runtime/agent_loop/response.rs` — `handle_text_response`
`crates/itsy/src/session/compaction.rs` — `maybe_compact`, `mid_turn_evict`
`crates/itsy/src/runtime/tool_guidance.rs` — skill card selection
`crates/itsy/src/session/tokens.rs` — token estimation
`crates/itsy/src/model/prompts.rs` — system prompt assembly

### Artifacts removed from `bin/itsy.rs`

~30 functions and ~1200 lines move into the library crate.

---

## Phase C — Reduce mutex nesting

### Goal

No more than 2 `Arc<Mutex<>>` held at the same stack level. No parking_lot
guard held across an `.await`.

### Steps

**C1. Merge per-turn stop / counter state into `TurnState`**

Already done by B2. Counters that were `Arc<Mutex<u32>>` on `AgentSession`
(`readonly_turn_count`, `total_mutating_calls`) remain there since they
cross turns — the rest become plain fields on `TurnState`.

**C2. Split `AgentSession` into slow-path and hot-path structs**

```rust
/// Immutable after startup. No locks needed.
struct AgentSessionReadOnly {
    flags: Flags,
    cwd: PathBuf,
    mcp_bridge: Arc<McpBridge>,
}

/// Read-mostly. RwLock.
struct AgentSessionShared {
    config: Config,          // written by slash commands; RwLock
    memory: MemoryStore,
    skills: SkillManager,
    plugins: PluginLoader,
    tokens: TokenTracker,
    token_monitor: TokenMonitor,
    sessions: SessionStore,
}

/// Written every turn. Single Mutex.
struct AgentSessionMutable {
    history: Vec<Value>,
    scorer: ToolScorer,
    verification: VerificationHistory,
    early_stop: EarlyStopDetector,
    trace: TraceRecorder,
    current_tool_category: Option<String>,
    fullscreen: Option<Arc<Fullscreen>>,
    tool_repeat_counts: HashMap<String, u32>,
    mutated_paths: HashSet<String>,
    bash_loop_keys: HashSet<String>,
    readonly_turn_count: u32,
    total_mutating_calls: u32,
}
```

`AgentSession` wraps all three behind a single top-level struct:

```rust
struct AgentSession {
    ro: AgentSessionReadOnly,         // no lock
    shared: RwLock<AgentSessionShared>,
    mutable: Mutex<AgentSessionMutable>,
}
```

**C3. Eliminate per-field locks entirely**

After C2, the pattern goes from:

```rust
session.history.lock().push(msg);
session.scorer.lock().record_success(...);
```

to:

```rust
let mut m = session.mutable.lock();
m.history.push(msg);
m.scorer.record_success(...);
```

One lock acquisition per critical section. No nesting.

**C4. Audit `.await` boundaries**

For every `.await` in `handle_turn` (and the extracted modules), verify the
`mutable` guard is dropped before the `.await`. If the guard must persist
across the `.await` (e.g. push then immediately query), take the value out:

```rust
let val = session.mutable.lock().some_field.clone();
do_async(val).await;
```

### Artifacts changed

`crates/itsy/src/bin/itsy.rs` — `AgentSession` restructuring
`crates/itsy/src/runtime/agent_loop/*.rs` — all guard/turn-state methods
`crates/itsy/src/executor.rs` — `ExecCtx` no longer needs `MemoryStore` etc.
  as separate Arcs; they're accessed through session

---

## Verification

Each phase:

1. `cargo check` passes
2. `cargo clippy --all-targets` passes
3. `cargo test` passes (existing tests + any new unit tests for extracted
   functions)
4. Spot-check: launch itsy, send one read task, one write task, confirm
   tool calls work the same

Phase A cannot change behaviour — it's a pure mechanical migration.
Phase B must preserve all guard semantics — review each extracted guard
for correctness.
Phase C must not introduce deadlocks — review all lock acquisition order.
