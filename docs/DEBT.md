# Technical Debt — 2026-05-24

Audit findings, updated to reflect remediation progress.

---

## P0 — Monolithic `handle_turn` (1514 lines)

`src/bin/itsy.rs:217–1730`. Contains the entire agent loop: clarifier,
contract preamble, routing, tool dispatch, quality monitoring, loop detection,
improvement loops, early-stop nudge, badger guard, greeting guard, streaming,
contract close-the-loop, auto-commit. ~14 levels of nesting.

**Progress:** 21 constituent functions (~720 lines) extracted to library
modules under `model/prompts.rs`, `session/compaction.rs`,
`runtime/tool_guidance.rs`, `runtime/agent_loop.rs`. Routing logic
consolidated into `classify_and_filter` in `runtime/tool_router.rs`.
`TurnState` struct created for per-turn counters.

**Remaining:** The core loop body (`async fn handle_turn`) is still 1514 lines
with interleaved guards. Unit testing requires extracting each guard into its
own method on `TurnState`.

**Fix:** Extract ~6 named helpers from the loop body:
`maybe_run_clarifier`, `evaluate_one_tool_call`, `maybe_badger`,
`maybe_close_contract_loop`, `maybe_stream_final`.

---

## P1 — Test Coverage Gaps

200 unit tests pass across the library. No tests cover the binary or the
agent loop itself.

| Module | Coverage | What's missing |
|--------|----------|---------------|
| `bin/itsy.rs` | 0% | No tool-loop, routing, turn-handling, or evaluator tests |
| `executor.rs` | 26 tests | Pure functions + read_file only — missing write, patch, bash, search, etc. |
| `model_client.rs` | 13 tests | Think-tag stripping, XML tool-call parsing, JSON validation only — missing API retry, streaming, error recovery |
| `config.rs` | 0% | Schema migration chain, env-var layering, TOML parsing edge cases |

---

## P2 — Clone-Heavy History & Config Access

`session.mutable.lock().history.clone()` appears ~4 times; each clones the
full `Vec<Value>` on every tool-call iteration. `config.clone()` on
`session.shared.read()` is called ~9 times alongside these.

**Progress:** Reduced from ~20 history clones by compartment restructuring.
A single `session.mutable.lock()` replaces 17 per-field locks.

**Fix:** Token-count before cloning large histories. Use `Arc<Vec<Value>>` with
copy-on-write. Or restructure to take the lock in a tight scope.

---

## P2 — `expect()` on Lock Poisons

`adaptive_router.rs`, `settings.rs`, `session/contract.rs` use
`.expect("… poisoned")` on mutex/rwlock results. If a thread panics while
holding one of these locks, every subsequent access panics.

**Risk:** Low today (single-threaded async runtime), but the components
silently explode if the runtime model gains threads. Worth a `// SAFETY:`
comment explaining why it's safe *today*.

---

## P3 — Minor Code Smells

- `src/cognition_adapter.rs:33` — `trim_end_matches(|c| matches!(c, '.'|','|'!'|'?'))`
  where a char array `['.', ',', '!', '?']` is cleaner.
- Two `panic!()` in `#[cfg(test)]` blocks (`fullscreen.rs`, `file_state.rs`) —
  prefer `assert_eq!` / `assert!` for better failure messages.

---

## Resolved Items

| Issue | Resolution |
|-------|-----------|
| Lock held across `.await` (8 sites) | All fixed — clone under brief lock scope, drop guard, then async |
| `Regex::new().unwrap()` — ~70 sites | All cleaned — 0 remaining |
| `history.clone()` — ~20 sites | Reduced to 4 by compartment restructuring |
| `config.clone()` hot path | 9 remain (was higher count in audit) |
| Clippy warnings (71 total) | 0 across entire workspace |
| Dual Config/Settings state | Phase A complete — back-mirror deleted, all reads via `settings::get()` |
| 17 `Arc<Mutex<>>` on AgentSession | Phase C complete — 3 compartments (ro/shared/mutable) |
| Inline JSON tool schemas | D4 complete — `ToolDef` struct + validation tests |
| Subprocess safety | D5 complete — timeouts wired, sandbox, evaluator merged to exec_bash path |
| Route consolidation | D6 complete — `classify_and_filter` entry point in `tool_router.rs` |
| Tool schemas | D4 complete — `ToolDef` struct + validation tests |
