# Technical Debt

Audit run 2026-05-24. Items below are raw findings;
crossed-out entries were fixed in the 2026-05-24 remediation pass.

---

## P0 — Lock Held Across `.await` (Deadlock Risk) — **FIXED**

~~8 sites in 3 clusters.~~ All resolved. `parking_lot::Mutex` and
`parking_lot::RwLock` guards were held across `.await` points. Fix pattern:
clone the needed data under a brief lock scope, drop the guard, then proceed
with the async call.

| File | Fix |
|---|---|
| `src/bin/itsy.rs:914-918` | Extract config, read_tracker, file_state, snapshot_manager into locals before the `let result = { … }` block |
| `src/bin/itsy.rs:1644` | `std::mem::take` the `early_stop` out of the lock before `stream_final_response().await` |
| `src/executor.rs:383,412` | Extract `error_diagnosis` bool into local before `.await` |
| `src/lsp.rs:251` | Take `ChildStdin` out of lock with `.take()`, write/flush, put back |
| `src/tools_impl/web_browse.rs:183` | Clone `searx_url` into local before `.await` |

---

## P0 — Monolithic `handle_turn` (1496 lines)

`src/bin/itsy.rs:217–1712`. Contains the entire agent loop: clarifier,
contract preamble, routing, tool dispatch, quality monitoring, loop detection,
improvement loops, early-stop nudge, badger guard, greeting guard, streaming,
contract close-the-loop, auto-commit. 14 levels of nesting.

**Risk:** One missing `break` or mis-ordered guard and the model either spins
or escapes. Impossible to unit-test as a single function. New features require
reading 1500 lines of interleaved logic to find where to wire in.

**Fix:** Extract into ~6 named helpers:
- `maybe_run_clarifier` / `maybe_wrap_contract_preamble`
- `evaluate_one_tool_call` (the inner single-tool-execution body)
- `maybe_badger` / `maybe_close_contract_loop` / `maybe_stream_final`

---

## P1 — Test Coverage Gaps

187 tests pass (up from 157 before the remediation pass). 30 new executor
tests and 13 new model_client tests were added. The following still have
**zero tests**:

| Module | What's missing |
|---|---|---|
| `bin/itsy.rs` | The entire binary — no tool-loop, routing, turn-handling, or evaluator tests |
| `executor.rs` | All 18+ built-in tool implementations |
| `model_client.rs` | API retry, streaming, tool-call parsing, error recovery |
| `config.rs` | Schema migration chain, env-var layering, TOML parsing edge cases |
**New tests added:** `executor.rs` (30 tests: pure functions + read_file integration),
`model_client.rs` (13 tests: think-tag stripping, XML tool-call parsing,
JSON validation). The 11 modules listed above remain untested.

---

## P1 — `unwrap()` on Static Regex Literals

~70 `Regex::new(...).unwrap()` call sites across the codebase on
compile-time-constant string literals. These can never fail under normal
operation, but if someone edits the literal and introduces an invalid regex,
it **panics at module load time**. Distributed across `cognition_adapter.rs`,
`executor.rs`, `governor.rs`, `knowledge.rs`, `security.rs`, `tui.rs`,
`plugins/`, `runtime/cognition/`, `runtime/features/`, `session/`,
`tools_impl/`.

**Fix:** Replace `.unwrap()` with `.expect("valid regex literal")` so the
failure mode is documented. Low urgency — all are static literals that would
be caught by a build-time test.

---

## P2 — Clone-Heavy History Access Pattern

`session.mutable.lock().history.clone()` appears ~20 times across
`bin/itsy.rs` and the clarifier path. Every tool-call iteration clones the
full `Vec<Value>` — potentially millions of tokens of serialised JSON.

The clarifier alone clones history **3–4 times** sequentially:
1. Lock → clone to check `assistant_asked_question`
2. Lock → clone to push clarifier instruction
3. Lock → clone to splice instruction back out
4. Lock → clone for `snapshot` (alongside `config.clone()`)

`config.clone()` on `session.shared.read()` is called alongside many of
these — a second large allocation per iteration.

**Fix:** Token-count before cloning and skip when the history exceeds a
threshold (the model probably won't read it all anyway). Or use
`Arc<Vec<Value>>` with copy-on-write so unmodified clones share
allocation. Or restructure to take the lock only in a tight scope that
reads the needed field directly and drops the guard before async work.

---

## P2 — `expect()` on Lock Poisons

`adaptive_router.rs`, `settings.rs`, `session/contract.rs` all use
`.expect("… poisoned")` on mutex/rwlock lock results. If a thread panics
while holding one of these locks, every subsequent access panics.

**Risk:** Low today (single-threaded async runtime), but the component
will silently explode if the runtime model gains threads. At minimum
worth a `// SAFETY:` comment explaining why it's safe *today*.

---

## P3 — Clippy Warnings — **CLEANED** (0 remaining in lib)

65 library warnings and 6 binary warnings were cleaned. The auto-fixable ones
were resolved with `cargo clippy --fix --lib -p itsy` (~23 fixes). Manual
fixes applied:

| Fix | File(s) |
|---|---|
| `needless_range_loop` → `for step in &mut steps` | `runtime/flows.rs`, `loops_adapter.rs` |
| `question_mark` (`let Some = x.take() else`) | `mcp_bridge.rs`, `tools_impl/mcp_client.rs` |
| `new_without_default` (auto-fixed) | `session/file_state.rs`, `tools_impl/read_tracker.rs` |
| `unnecessary_to_owned`, `useless_format` | `tui.rs` |
| Dead `ToolScore::new()` removed | `tools_impl/trust_decay.rs` |

Remaining non-mechanical items (low priority):
- `type_complexity` in `web_browse.rs:372` — `[(&str, fn(&str) -> Option<String>); 5]`
- `field_reassign_with_default` in `fullscreen.rs:320` and `init_wizard.rs`
- `await_holding_lock` in `features_adapter.rs:82` — false positive (has `drop(guard)`)

---

## P3 — Minor Code Smells

- `src/cognition_adapter.rs:33` — `trim_end_matches(|c| matches!(c, '.'\|','\|'!'\|'?'))`
  where a char array `['.', ',', '!', '?']` is cleaner and faster.
- Two `panic!()` in `#[cfg(test)]` blocks (`fullscreen.rs`, `file_state.rs`) —
  consider `assert_eq!` / `assert!` for better failure messages.
