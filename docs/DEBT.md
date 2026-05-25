# Technical Debt — 2026-05-24

Audit findings, updated to reflect remediation progress.

---

## P0 Monolithic handle_turn 1514 lines

src/bin/itsy.rs:174-1280 (1106 lines, down from 1514). Still contains
the main chat loop, improvement loops, streaming, badger guard, and
greeting guard.

**Extracted as TurnState methods in runtime/agent_loop.rs:**
- check_quality_monitor — validates tool names
- check_contract_gate — blocks mutating calls before contract exists
- check_idempotent_write — dedup memory_remember / mark_assertion
- check_no_progress — nudge after N read-only batches
- check_text_only_streak — force tool use after N text responses
- check_contract_close_loop — refuse end turn with unclosed assertions
- check_loop_detection — block identical tool calls after 3 repeats


---

## P1 — Test Coverage Gaps

**Current state (2026-05-25): 916 tests, 69.30% line / 73.11% function /
69.37% region coverage.** Up from 200 tests / ~34% lines at the start of
this audit pass.

### Now well-covered (≥70%)

| Module | Cov | Tests added |
|--------|-----|------------|
| `runtime/two_stage_router.rs` | 100% | 6 |
| `runtime/cognition/router.rs` | ~94% | 9 |
| `runtime/cognition/cache.rs` | ~95% | 8 |
| `runtime/providers/ssrf_guard.rs` | ~94% | 11 |
| `runtime/agent_helpers.rs` | ~94% | 3 (lifted from bin/itsy.rs) |
| `runtime/evaluator.rs` | ~96% | 16 (lifted from bin/itsy.rs; **caught a real timeout bug** — `read_to_end` blocked the polling loop, fixed with reader threads) |
| `runtime/mcp_server.rs` | ~95% | 15 (lifted from bin/itsy.rs MCP handler) |
| `runtime/flows.rs` | 100% | 6 |
| `runtime/features/multi_file_edit.rs` | ~95% | 6 |
| `runtime/features/policy.rs` | 100% | 3 |
| `loops_adapter.rs` | ~98% | 6 |
| `cognition_adapter.rs` | ~95% | 11 |
| `model/adaptive_temp.rs` | ~96% | 10 |
| `model/router.rs` | ~90% | 9 |
| `model/profiles.rs` | ~75% | 7 |
| `tools.rs` | ~90% | 7 |
| `verification.rs` | 91% | 10 |
| `adapters/acp.rs` | ~91% | 12 |
| `session/undo.rs` | ~91% | 11 |
| `model_client.rs` | ~85% | 22 (+ wiremock HTTP mock layer added as dev-dep) |
| `runtime/providers/openai_compat.rs` | new | 10 (wiremock-driven) |
| `tools_impl/web_browse.rs` | ~70% | 22 (extraction + wiremock fetch) |
| `commands.rs` | ~48% | 31 slash-command dispatch tests |
| `token_monitor.rs` | new | 10 |
| `settings.rs` | new | 17 (apply_set_override parsing, alias paths) |
| `tui.rs` | new | 24 (markdown render, tool helpers, diff render) |
| `fullscreen.rs` | ~41% | 28 (theme, char-boundary, stream, diff truncation) |
| `runtime/agent_loop.rs` | (mostly TurnState guards) | 12 |
| `runtime/logger.rs` | new | 7 |
| `mcp_bridge.rs` | new | 6 |
| `eval_runner.rs` | new | 9 |
| `init_wizard.rs` | ~46% | 18 (model-hint detection, quant tier, budget pct) |
| `plugins/loader.rs` | new | 14 |
| `session/bootstrap.rs` | new | 12 (framework detection) |
| `session/share.rs` | new | 12 (export markdown/json/html + perms) |
| `session/persistence.rs` | new | 10 (short_id, valid_id, base36, auto_title) |
| `session/multi.rs` | ~67% | 16 |
| `session/compaction.rs` | ~80% | 5 (system msg preserved, eviction stubs) |
| `session/references.rs` | new | 15 (`@file` parsing, line ranges) |
| `session/contract.rs` | ~74% | +6 (mark_assertion / mark_feature / close gates) |
| `session/git_context.rs` | ~70% | +5 |
| `governor.rs` | ~58% | 14 (decompose strategy, classifier branches) |
| `governor/early_stop.rs` | ~85% | 10 |
| `tools_impl/dedup.rs` | ~90% | 11 (idempotent key order-invariance) |
| `tools_impl/read_tracker.rs` | ~80% | 8 (read-before-write guard) |
| `tools_impl/test_runner.rs` | ~60% | 17 (jest, vitest, mocha, rspec, go output parsers) |
| `runtime/tool_guidance.rs` | new | 8 |
| `runtime/cognition/budget.rs` | ~90% | 6 |
| `runtime/cognition/repair.rs` | ~85% | 12 |
| `runtime/cognition/traces.rs` | ~70% | 5 |
| `runtime/cognition/loops.rs` | ~80% | 6 |
| `runtime/cognition/prompts.rs` | ~25% | 9 (template render `{{var}}`) |
| `runtime/features/prompts.rs` | ~60% | 14 (each known prompt, TTL ordering) |
| `runtime/features/checkpoints.rs` | ~80% | 9 (decision parsing, await/submit) |
| `runtime/features/context_retriever.rs` | ~95% | 19 (token-budget picker, keyword extract) |
| `runtime/extensions.rs` | 100% | 6 |
| `memory.rs` | ~77% | +3 (Arc<Mutex> sharing — bug #2 pin) |

### Regression tests pinning recently-fixed bugs

1. **MemoryStore sharing** — `memory.rs::shared_arc_writes_visible_to_clone`
   pins that the same `Arc<Mutex<MemoryStore>>` is shared across tool calls
   (was a fresh empty store per call before the fix).
2. **Bash loop counter reset** — `runtime::agent_loop::reset_bash_loop_actually_clears_counts`
   pins that the real `tool_repeat_counts` HashMap is mutated, not a discarded clone.
3. **Affirmation category preservation** — `runtime::tool_router::affirmation_inherits_prior_category`
   pins that "yes" after a "coding" turn stays in coding tools.
4. **Confidence in routing trace** — `runtime::tool_router::confidence_is_non_zero_for_clear_intents`
   pins that classifier confidence is plumbed into `record_classification`.
5. **propose_contract injection** — `tools.rs::two_stage_filtered_set_keeps_propose_contract_after_injection`
   pins that the GPU-idle stall bug stays fixed.
6. **Timeout in `run_bash_with_timeout`** — `runtime::evaluator::bash_timeout_kills_long_running_command`
   pins the reader-thread fix for the buggy inline `read_to_end` polling loop.
7. **verification removed `/tests`** — `verification.rs::discover_does_not_reach_outside_cwd`
   pins that the speculative external path is gone.

### Still missing — what to write next

| Module | Cov | Missed | Why not covered |
|--------|-----|-------:|----------------|
| `bin/itsy.rs` | 0% | ~2200 | Main binary glue — REPL loop, signal handling, fullscreen REPL bootstrap, eval driver. Lifted helpers (`recency_tool_hint`, `BashOutput`/`run_bash_with_timeout`/`evaluator_run_bash`/`evaluator_read_file`, MCP JSON-RPC builders, destructive-cmd guard, unique-patch) now live in `runtime::` and are covered. What's still hard: `handle_turn` itself (the main agent loop with LLM calls + per-turn state machine), `run_repl`/`run_fullscreen_repl` (interactive stdin), `run_evaluator_phase` (LLM-driven), `try_auto_commit` (live git). Needs either further `runtime::` lifts or an integration harness with a fake llama-server. |
| `executor.rs` | ~49% | ~780 | Many tool functions tested at boundaries (rejection paths, missing path, oversized content). Still untested: success path of `exec_bash`, `exec_search`'s rg invocation, `exec_list_projects`, `exec_graph_search`, `exec_explain_symbol`, `exec_propose_contract` / `exec_mark_assertion` / `exec_close_contract` (need full contract test fixtures), `exec_web_search` / `exec_web_fetch` (HTTP). Most need either a real bash/rg child or wiremock for the HTTP-side tools. |
| `commands.rs` | ~48% | ~555 | Dispatch + obvious helpers covered. Still untested: `cmd_git` (subprocess), `cmd_share` to gist (network), `cmd_undo all` with real undo stack, `cmd_lsp` start/stop (needs language server), `cmd_eval` (drives EvalRunner end-to-end), `cmd_trace replay <id>` (needs disk fixtures), `cmd_skill add/remove` (mutates skill manager state). All testable but need richer ctx fixtures or HTTP mocks. |
| `fullscreen.rs` | ~41% | ~640 | Public API + char-boundary helpers + diff truncation covered. Still untested: the ratatui render functions (`render_chat_lines`, `render_welcome`, `render_status_bar`, `render_input`). Testing these means asserting against `ratatui::buffer::Buffer` after a `Frame` render — feasible via `ratatui::backend::TestBackend`, but each test is a snapshot-style assertion (cell-by-cell), which is high-effort, low-signal except for visual regressions. |
| `config.rs` | ~53% | ~330 | Migration chain, normalize_base_url, auth headers, env_str, resolve_api_key — all covered. Still untested: `load_config` (reads disk + env layering), `check_endpoint` (HTTPS probe), `ConfigFile::save_to_path` (atomic write + permissions). Wiremock could cover `check_endpoint`. |
| `init_wizard.rs` | ~46% | ~310 | `detect_model_hints`, `quant_tier_for`, `budget_pct_for`, `routing_for`, `family_default_window`, `provider_for` covered. Still untested: `run` (the wizard itself — `read_line`-blocking stdin), `synchronous_probe` (HTTP), `print_detected` (stdout writer), `write_default` (config file writer). The interactive `run` is the bulk; testing it would need a mockable input/output trait pair. |
| `runtime/agent_loop.rs` | ~17% | ~440 | `TurnState` guard methods covered. **`handle_turn` (the main async function ~350 lines) is uncovered** — it owns the per-turn chat-completion loop, retry, tool dispatch, history mutation. Testing it means mocking `chat_completion` against wiremock AND building a fully-populated `AgentSession`, which is feasible but a significant fixture investment. |
| `model_client.rs` | ~85% | ~120 | `chat_completion` and `stream_final_response` covered via wiremock. Untested: `build_system_prompt` paths (some branches), the vision/image extraction path inside `chat_completion`. |
| `lsp.rs` | ~28% | ~350 | Detection + header parsing covered. Untested: the actual LSP RPC loop (`open_document`, `request`, `read_loop`). Requires an LSP server stub. |
| `tools_impl/test_runner.rs` | ~60% | ~365 | All parsers covered. Untested: actual `run` and `run_detected` paths that spawn subprocesses (cargo/pytest/jest). Integration-testable in CI but heavy locally. |
| `tools_impl/web_browse.rs` | ~70% | ~180 | Extraction + fetch wiremock-tested. Untested: `web_search` against DDG / Brave / SearxNG (each needs its own wiremock fixture for the response shape). |
| `tools_impl/mcp_client.rs` | 0% | 494 | JSON-RPC over stdio with a child process. Hard to test without spinning up a real MCP server child or replacing `Command` with a trait — non-trivial refactor. |
| `tools_impl/shell_session.rs` | 0% | 430 | Long-running bash session multiplexer. Requires real bash + a sentinel-based protocol. Best tested via integration tests that spawn `bash` in a tempdir. |
| `features_adapter.rs` | ~30% | ~430 | Pure helpers (`strip_fences`, `truncate`, `merge_assertions`, `parse_assertion_array`) covered. Untested: every `*_compiled` function calls a live LLM. Would need wiremock + a fake response shape for each prompt. |
| `runtime/providers/openai_compat.rs` | ~94% | ~15 | Almost fully covered via wiremock. Only the streaming SSE branch + transport-error mapping left. |
| `runtime/features/verify_and_fix.rs` | ~58% | ~140 | Some logic covered. The improvement-loop orchestration that calls `validate_edit_compiled` is LLM-bound. |
| `model/prompts.rs` | ~43% | ~145 | Memory/skill/plugin context helpers covered. Untested: `build_system_prompt` branches for different task types + reasoning models. |
| `eval_runner.rs` | ~50% | ~130 | Pure paths covered. The actual chat_fn invocations are mocked at the test boundary; the LLM integration is untested by design. |
| `model/profiles.rs` | ~75% | ~75 | Built-in profile match + disk override + new_strips_trailing_slash all covered. Still missing: the on-disk TOML loader paths (file-system-heavy). |

### What 70%+ would buy beyond this

The remaining ~30% of LOC is overwhelmingly:
- Live HTTP/stdio I/O (LSP, MCP client, shell session, chat_log writer)
- TUI rendering (ratatui Frame → Buffer snapshot tests are low-signal)
- One-shot bootstrap (init_wizard interactive flow, save_to_path atomic writes)
- LLM-driven feature adapters (`*_compiled` functions in `features_adapter.rs`)
- Main binary REPL + signal/SIGINT plumbing

Pushing past 70% on these adds line coverage but minimal regression-catching
power — terminal-bench-2 exercises them end-to-end and is the appropriate
test layer.

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
