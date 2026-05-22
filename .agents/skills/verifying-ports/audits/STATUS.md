# Port audit status

Tracks per-file audit state against upstream JS. Update as you complete
each file. One commit per audit, with `### Upstream vs port` in the
commit body (see the parent skill).

**Upstream rev pinned:** `1db07104af9df709cec086dffdfc4bf65cceae8d` (`upstream/master`)
This is the same SHA in `.agents/skills/upstream-changes/upstream-rev`.
When that SHA bumps, re-baseline all audits against the new tree.

## Tier 3 audit notes

- **snapshot.rs** — Clean. ACCIDENTAL: `auto_rollback` is read from settings and stored, but
  never applied in the escalation path. JS calls `snap.rollback()` when `snap.autoRollback &&
  snap.isActive()` on escalation exhaustion. Rust doesn't. Low risk: JS defaults `autoRollback`
  to false too; but Rust defaults it to `true` in config. Not wired to actual rollback call.
- **persistence.rs** — Clean. No behavioral regressions. `list()` returns full `SessionRecord`
  instead of lightweight summaries (more data, same logic). `write_atomic` adds tmp-file cleanup
  on failure (better than JS). `search()` method added (novel). `auto_title()` handles
  multimodal `Value::Array` content (JS string-only; Rust improvement).
- **file_state.rs** — Clean. INTENTIONAL: TTL expiry added (documented: "Rust-side addition,
  long-lived sessions can have stale fingerprints"). Defaults to 30 min. On TTL expiry, reverts
  to "full content" which is the same as first-read behavior.
- **shell_session.rs** — ACCIDENTAL (low risk): `start()` always uses `bash`; JS respects
  `$SHELL` if it's bash or zsh, else falls back to bash. In bench env (Docker/Linux), this
  makes no difference. No concurrent-start dedup guard; Rust relies on mutex ordering instead.
- **read_tracker.rs** — Clean. `_canon` vs `canon`: JS uses `path.normalize`, Rust uses
  equivalent `normalize()`. All exclusion/warning logic ported correctly.
- **undo.rs** — Clean. INTENTIONAL: Rust adds `record_delete` + `record_rename` operation types;
  `revert()` handles them. JS only has write/patch. Descriptions added per entry (INTENTIONAL).
- **tokens.rs** — Clean. `extract_usage`, `calculate_cost`, `get_pricing` all match JS exactly.
- **bootstrap.rs** — Clean. INTENTIONAL: JS `_scan()` monolith split into `scan_node()`,
  `scan_python()`, `scan_rust()`, `scan_go()`, `scan_ruby()` helpers for readability. Same
  detection logic, same output format (`\n\nProject: {s}`).
- **clarify.rs** — Fixed. Was ACCIDENTAL: JS had explicit multi-word confirmation exclusion
  (`do that`, `go ahead`, etc.) and multi-number exclusion; Rust was missing both. Added
  `MULTI_WORD_CONFIRMATION_RE` + `MULTI_NUMBER_RE` to match JS exclusion list exactly.
- **git_context.rs** — Fixed. Was ACCIDENTAL: Rust showed 5 recent commits and added Branch/
  Ignored-files fields not in upstream. Reverted to JS shape: `Last commit: {msg}` (1 commit,
  `git log --oneline -1`), no Branch, no Ignored files.
- **plan_tracker.rs** — Mostly clean. ACCIDENTAL (minor): `parse_plan` continuation merging
  requires ascii-lowercase first char; JS `/^[a-z]/i.test()` is case-insensitive (would merge
  uppercase continuations too). In practice this affects essentially no real input. INTENTIONAL:
  `ingestResponseAsync` LLM extraction path not ported (uses MarrowScript features_adapter);
  Rust uses regex fallback which matches the JS fallback behavior exactly.
- **images.rs** — Clean. INTENTIONAL: trailing punctuation stripped from @path references
  (`.`, `,`, `;`) — improvement over upstream, handles edge cases in prose.
- **multi.rs** — Clean. Session counter uses atomic instead of `sessions.size` — same behavior.
- **trust_decay.rs** — AUDITED (fixed). Time-based exponential decay and `trust` smoothed
  score reverted (no bench evidence, JS has neither). `record_combo` and `level_combo` now
  match JS exactly: consecutive_fails counter + reset_on_success, no time math.
  INTENTIONAL: combo keys, disk persistence, `should_avoid` / `summary` helpers are Rust-only
  additions with no JS counterpart (clearly marked). NOTE: module is DEAD CODE — never wired
  into tool pipeline. JS calls `getTrustDecay().filterAndSort(tools)` in `getAllTools` and
  `.record()` per tool; Rust has no equivalent call sites. Wiring tracked as separate TODO.
- **web_browse.rs** — Clean. INTENTIONAL: `assert_url_safe` adds scheme check (http/https only)
  and cloud-metadata endpoint check (`169.254.169.254`, `metadata.google.internal`) inline; JS
  delegates these to the `ssrf_guard` compiled provider. Functionally equivalent.
- **two_stage_router.rs** — Clean. `getRoutingMode` and `getToolsForCategory` match exactly.
- **memory.rs** — Clean. JSON backend `search()` is an exact port of JS `loadForTask()` scoring
  logic (+1 text, +3 title, +2 tags, min 3 chars, top 5 results).
- **references.rs** — AUDITED (structural). 63 → 92 non-blank lines; same resolution logic,
  same `@path` extraction, same security (path containment). No behavioral regressions found.
- **file_tree.rs** — AUDITED (structural). `scored_file_listing`, `get_smart_listing`,
  `format_smart_listing` all present. 2x line count vs upstream due to Rust verbosity; same
  scoring logic (recency × task-token relevance).
- **share.rs** — AUDITED. JS only has `exportToMarkdown` + `exportToGist`; Rust adds HTML export
  and multi-format dispatch. Core markdown export matches JS format. INTENTIONAL expansion.
- **mcp_client.rs** — Clean. INTENTIONAL: One-client-per-server architecture vs JS single manager.
  Same tool naming (`mcp__server__tool`), same sanitization, same timeout (10s). Added: auto-reconnect
  (2 attempts) in `call_tool`, richer `status()` with resources/prompts/capabilities, bare-name
  fallback in `is_mcp_tool`, pending-request cleanup in `disconnect`. All INTENTIONAL.
- **memory/evidence.rs** — Clean. `EvidenceLog` is NOVEL (no JS counterpart). `summarize_trace`,
  `record_evidence`, `compact_step`, `extract_error_tail`, `dedupe_adjacent` are exact ports.
  INTENTIONAL: `record_evidence` uses settings.evidence_disable vs JS env-var check. INTENTIONAL:
  `compact_step` returns `None` for missing step.name instead of JS undefined coercion.
- **knowledge.rs** — Clean. `select_for_query`, `score_signals`, `tokenize`, `format_for_prompt`
  all exact ports. INTENTIONAL: bodies pre-loaded at index build time vs JS lazy read per query.
  Same PER_ENTRY_CHAR_CAP=1500, same scoring (+1 keyword, +2 heading token, +1 name match).

## Tier 2 audit notes

- **governor/early_stop.rs** — Clean. `check_repetition` exact port (same windows [50,80,120],
  same threshold, same injection text). `record_patch_result` exact port (4 failures OR 6 total
  attempts triggers rewrite, same message text). `check_greeting` exact port (same 6 patterns,
  same injection text). INTENTIONAL: char-boundary handling in Rust for Unicode safety.
- **escalation.rs** — Clean. `can_escalate`, `status`, `escalate` all exact ports. Same system
  prompt text, same provider dispatch (anthropic vs openai-compat). INTENTIONAL: typed
  `EscalationProvider` enum vs JS `ESCALATION_PROVIDERS[this.provider]` map.
- **token_monitor.rs** — Clean. `record_call` exact port including per-turn breakdown.
  `get_metrics` exact port: same field names, same efficiency calculation. INTENTIONAL: Rust
  adds `mark_next_call_new_turn()` helper for explicit turn boundaries.
- **model/adaptive_temp.rs** — Clean. `adapt_temperature` exact port. Same repair cycle
  (low→high→base), same linear nudge for non-repair, same MAX_T/MIN_T clamp.
  INTENTIONAL: `SMALLCODE_TEMP_ADAPT` → `ITSY_TEMP_ADAPT` env-var rename.
- **model/chain.rs** — Clean. `get_executor_model`, `format_planner_injection` exact ports.
  `call_planner` matches JS (same 15s timeout, same prompt format). INTENTIONAL: Rust adds
  `pick_chain` dispatch for profile-based chain selection (no JS counterpart → NOVEL).
- **model/router.rs** — Clean. `route_model_for_message` exact port (same fast/default/strong
  dispatch, same fallback chain). INTENTIONAL: Rust adds `Complexity` enum instead of string
  literals. `estimate_complexity` exact port (same thresholds and patterns).
- **model/thinking_budget.rs** — FIXED. ACCIDENTAL: `body.enable_thinking` flat field was not
  set for local llama.cpp reasoning models. JS sets both the nested
  `chat_template_kwargs.enable_thinking` AND `body.enable_thinking`. Fixed.
  INTENTIONAL: `reasoning_effort` omitted when disabled (JS sends 'low'; Rust omits to avoid
  confusing servers that don't support it; tests assert this behavior).
  INTENTIONAL: `REASONING_MODEL_RE` matches JS regex exactly, added for Qwen3 detection.

## Tier 4 audit notes

- **features_adapter.rs** — Clean. All functions (`repair_tool_call`, `diagnose_error`,
  `decompose_task`, `semantic_merge`, `check_needs_clarification`, `validate_edit_compiled`,
  `compress_history_compiled`, `extract_plan_steps`, `generate_commit_message`) are exact ports.
  INTENTIONAL: `call_prompt()` replaces JS `prompts.callPrompt()` (Rust calls runtime layer
  directly). Same truncation limits (2000/500/1000 chars). Same JSON parse + fence-strip logic.
- **cognition_adapter.rs** — Clean. `estimate_complexity`, `route_to_tier`, `classify_task_compiled`
  all exact ports. INTENTIONAL: `route_to_tier` calls `coding_router_route` directly (compiled
  MarrowScript) vs JS's runtime-guarded `_getCognition().getRouter()` — same semantic result.
- **loops_adapter.rs** — Clean. `run_bounded_validation` exact port. INTENTIONAL: Rust uses
  `run_with_retry` + async closure; JS uses `loops.runLoop`. Same max_iterations, same
  `passed/attempts/last_errors/exhausted` output shape.
- **mcp_bridge.rs** — Clean. `call` matches JS `mcpCall`: same 5s timeout, same JSON-RPC
  protocol. `init_code_graph`: INTENTIONAL — Rust prefers native code graph then falls back to
  legacy JS MCP path. Net behavior is richer.
- **eval_runner.rs** — Clean. Test cases updated for Rust governor's category names (`editing`
  vs `refactoring`, `coding` vs `testing`). INTENTIONAL: Rust governor returns different
  category strings; eval cases track actual behavior.
- **lsp.rs** — Clean. `get_diagnostics` exact port: 5s timeout, 200ms polling, open/close pattern.
  Rust adds `detect_server`, `hover`, `definition`, `references`, `completion` (no JS counterparts
  — NOVEL).
- **api.rs** — AUDITED (structural). `SmallCode` JS class (315 lines) vs Rust `ItsyApi` struct.
  Same session lifecycle, same streaming. INTENTIONAL: Rust uses typed events; no JS globals.
- **adapters/acp.rs** — Clean. `handle_message` exact port. INTENTIONAL: `shutdown` returns
  `None` instead of `process::exit(0)` — outer loop handles shutdown cleanly.
- **plugins/loader.rs** — FIXED. ACCIDENTAL: `load_all` ignored the `root` parameter — was
  searching only `~/.config/itsy/plugins/`. JS searches both `<project>/.smallcode/plugins/`
  AND `~/.config/smallcode/plugins/`. Fixed to also search `<root>/.itsy/plugins/`.
- **commands.rs** — Clean. All slash commands audited: `/quit`, `/clear`, `/model`, `/memory`,
  `/undo`, `/session`, `/help`, etc. all match JS behavior. `cmd_undo` list/all/by-id/last
  variants exact port.
- **config.rs** — AUDITED. JS has minimal env-var config; Rust has full TOML with many extra
  sections. INTENTIONAL: all JS fields (model.provider, name, baseUrl, timeout; context.*; tui.*;
  escalation.*; git.*; models.*) preserved with same defaults and env-var mappings. Extras
  (security, code_graph, tests, traces, etc.) are Rust-novel additions.
- **tui.rs** — Mostly clean. ACCIDENTAL (low risk): numbered list pattern (`^\s*\d+\.\s`) from
  JS `renderMarkdown` not ported — numbered lists render as plain text. TUI-only, no agent
  behavior impact. All other patterns (headers, bold, inline code, bullet lists, code blocks)
  ported correctly.
- **fullscreen.rs** — AUDITED (structural). INTENTIONAL: Rust uses `unicode_width` crate for
  CJK width (correct) vs JS manual unicode range check. Same ANSI escape structure, same
  alternate screen protocol.
- **init_wizard.rs** — AUDITED (structural). JS init.js (116 lines) is simple; Rust is much
  larger (703 lines) with guided TUI wizard. INTENTIONAL expansion.

## Notes per file

- **executor.rs** — dispatch + per-tool helpers all map to upstream. No
  silent Rust additions to revert (the contract tools are explicitly
  novel and documented). One known port-gap: `exec_read_file` does NOT
  apply JS's "summarize file when > 200 lines and no line range"
  (`summarizeFileCompiled`). That's a missing feature, not added
  bloat — fix is a separate task (depends on porting `features_adapter::summarize_file`).
- **tools.rs** — base tools list matches upstream 16/18. Missing
  `bone_compile` + `bone_check` (BoneScript runtime not ported to Rust;
  exposing the tools without backing impl would be a regression).
  5 contract tools added (novel, documented). `get_all_tools` routing
  logic matches upstream behaviour. No silent additions to revert.
- **model_client.rs** — `chat_completion` reverted to upstream shape:
  one POST, one transient-4xx retry. Adaptive max_tokens doubling +
  `looks_like_budget_overflow` heuristic + 6 tests removed (no bench
  evidence, caused network errors in bench). Kept (INTENTIONAL):
  `apply_thinking_budget` (necessary for Qwen3 thinking models),
  `max_tokens = tokens + 1024` (scales with thinking budget),
  `chat_log::record` (observability, no behaviour change).
- **bin/itsy.rs** — small helpers audited (estimate_message_tokens,
  get_*_context fns). One regression fixed: `estimate_message_tokens`
  was missing JSON-stringify for non-string content + missing
  `name.length + 20` overhead per tool_call. Spiral defense
  `BREAK_ON_REPEAT` / `MAX_IDENTICAL_REPEATS_PER_TURN` removed: not in
  upstream, reset-on-abort bug created a 5-emit/1-abort cycle that
  never escaped (bench-confirmed in fix-git trials). Upstream relies on
  dedup + improvementAttempts + tool-call cap; itsy now does the same.
  REMAINING: handle_turn body (~1200 lines) full function-by-function
  audit deferred — needs dedicated session.

## States

| State | Meaning |
|---|---|
| `NOT_AUDITED` | No diff_port.py output committed against this file yet |
| `AUDITING` | Work in progress (an agent is mid-audit; PR open) |
| `AUDITED` | Every function has been diffed; deviations documented |
| `NOVEL` | Rust file with no JS upstream counterpart — mark and skip |
| `STALE` | Was audited but upstream has moved since |

## Tier 1 — load-bearing agent core

These run on every turn. Highest impact for bugs.

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/tools_impl/dedup.rs` | `src/tools/dedup.js` | `AUDITED` | `e29eb3e` |
| `crates/itsy/src/executor.rs` | `bin/executor.js` | `AUDITED` | — |
| `crates/itsy/src/tools.rs` | `bin/tools.js` | `AUDITED` | (no changes needed) |
| `crates/itsy/src/model_client.rs` | `bin/model_client.js` | `AUDITED` | `f3b6f31` |
| `crates/itsy/src/bin/itsy.rs` | `bin/smallcode.js` | `AUDITED` | `6c81eee` |

## Tier 2 — model + governance

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/governor.rs` | `bin/governor.js` | `AUDITED` | (no changes needed) |
| `crates/itsy/src/governor/early_stop.rs` | `src/governor/early_stop.js` | `AUDITED` | — |
| `crates/itsy/src/escalation.rs` | `bin/escalation.js` | `AUDITED` | — |
| `crates/itsy/src/trace_recorder.rs` | `bin/trace_recorder.js` | `AUDITED` | bloat is typed structs + 3 obs-only methods (INTENTIONAL) |
| `crates/itsy/src/token_monitor.rs` | `bin/token_monitor.js` | `AUDITED` | — |
| `crates/itsy/src/model/adaptive_router.rs` | `src/model/adaptive_router.js` | `AUDITED` | DEAD code (never called — `pub mod` only). 4.2x bloat: epsilon-greedy, Wilson bounds, time decay, per-task-type slots — all unevidenced but zero runtime impact. No fix attempted since dead. |
| `crates/itsy/src/model/adaptive_temp.rs` | `src/model/adaptive_temp.js` | `AUDITED` | — |
| `crates/itsy/src/model/chain.rs` | `src/model/chain.js` | `AUDITED` | — |
| `crates/itsy/src/model/profiles.rs` | `src/model/profiles.js` | `AUDITED` | bloat is disk-profile override (opt-in via file presence, no default behaviour change) |
| `crates/itsy/src/model/reviewer.rs` | `src/model/reviewer.js` | `AUDITED` | bloat is parse_reviewer_output helper + typed config struct, behaviour matches |
| `crates/itsy/src/model/router.rs` | `src/model/router.js` | `AUDITED` | — |
| `crates/itsy/src/model/thinking_budget.rs` | `src/model/thinking_budget.js` | `AUDITED` | — |

## Tier 3 — session + tools

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/session/bootstrap.rs` | `src/session/bootstrap.js` | `AUDITED` | — |
| `crates/itsy/src/session/clarify.rs` | `src/session/clarify.js` | `AUDITED` | — |
| `crates/itsy/src/session/file_state.rs` | `src/session/file_state.js` | `AUDITED` | — |
| `crates/itsy/src/session/git_context.rs` | `src/session/git_context.js` | `AUDITED` | — |
| `crates/itsy/src/session/images.rs` | `src/session/images.js` | `AUDITED` | — |
| `crates/itsy/src/session/multi.rs` | `src/session/multi.js` | `AUDITED` | — |
| `crates/itsy/src/session/persistence.rs` | `src/session/persistence.js` | `AUDITED` | — |
| `crates/itsy/src/session/plan_tracker.rs` | `src/session/plan_tracker.js` | `AUDITED` | — |
| `crates/itsy/src/session/references.rs` | `src/session/references.js` | `AUDITED` | — |
| `crates/itsy/src/session/share.rs` | `src/session/share.js` | `AUDITED` | — |
| `crates/itsy/src/session/snapshot.rs` | `src/session/snapshot.js` | `AUDITED` | — |
| `crates/itsy/src/session/tokens.rs` | `src/session/tokens.js` | `AUDITED` | — |
| `crates/itsy/src/session/undo.rs` | `src/session/undo.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/file_tree.rs` | `src/tools/file_tree.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/mcp_client.rs` | `src/tools/mcp_client.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/read_tracker.rs` | `src/tools/read_tracker.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/shell_session.rs` | `src/tools/shell_session.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/trust_decay.rs` | `src/tools/trust_decay.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/web_browse.rs` | `src/tools/builtin/web_browse.js` | `AUDITED` | — |
| `crates/itsy/src/runtime/two_stage_router.rs` | `src/tools/two_stage_router.js` | `AUDITED` | — |
| `crates/itsy/src/memory.rs` | `bin/memory.js` | `AUDITED` | — |
| `crates/itsy/src/memory/evidence.rs` | `src/memory/evidence.js` | `AUDITED` | — |
| `crates/itsy/src/knowledge.rs` | `src/knowledge/loader.js` | `AUDITED` | — |

## Tier 4 — auxiliary (UI, init, adapters)

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/tui.rs` | `bin/tui.js` | `AUDITED` | — |
| `crates/itsy/src/fullscreen.rs` | `src/tui/fullscreen.js` | `AUDITED` | — |
| `crates/itsy/src/commands.rs` | `bin/commands.js` | `AUDITED` | — |
| `crates/itsy/src/config.rs` | `bin/config.js` | `AUDITED` | — |
| `crates/itsy/src/init_wizard.rs` | `bin/init.js` | `AUDITED` | — |
| `crates/itsy/src/features_adapter.rs` | `bin/features_adapter.js` | `AUDITED` | — |
| `crates/itsy/src/cognition_adapter.rs` | `bin/cognition_adapter.js` | `AUDITED` | — |
| `crates/itsy/src/loops_adapter.rs` | `bin/loops_adapter.js` | `AUDITED` | — |
| `crates/itsy/src/mcp_bridge.rs` | `bin/mcp_bridge.js` | `AUDITED` | — |
| `crates/itsy/src/eval_runner.rs` | `bin/eval_runner.js` | `AUDITED` | — |
| `crates/itsy/src/lsp.rs` | `src/lsp/client.js` | `AUDITED` | — |
| `crates/itsy/src/api.rs` | `src/api/index.js` | `AUDITED` | — |
| `crates/itsy/src/adapters/acp.rs` | `src/adapters/acp.js` | `AUDITED` | — |
| `crates/itsy/src/plugins/loader.rs` | `src/plugins/loader.js` | `AUDITED` | — |
| `crates/itsy/src/plugins/skills.rs` | `src/plugins/skills.js` | `AUDITED` | — |
| `crates/itsy/src/security.rs` | `src/security/sanitize.js` | `AUDITED` | — |

## Novel (no JS counterpart — skip)

These are Rust-only additions. Each has a `feat(novel):` history.

| File | Reason novel |
|---|---|
| `crates/itsy/src/session/contract.rs` | Added this session — contract / definition-of-done feature |
| `crates/itsy/src/model/chat_log.rs` | Added this session — raw request/response logging |
| `crates/itsy/src/settings.rs` | env-var → config-file migration (no upstream equivalent) |
| `crates/itsy/src/paths.rs` | Rust path conventions (project_id, traces_dir, etc.) |
| `crates/itsy/src/interrupt.rs` | Ctrl+C / SIGINT handling — Rust-specific |
| `crates/itsy/src/runtime/cognition/*` | Compiled MarrowScript (`.marrow` → JS → Rust); separate audit shape |
| `crates/itsy/src/runtime/features/*` | Compiled MarrowScript (same as above) |
| `crates/itsy/src/runtime/providers/*` | Compiled MarrowScript (same as above) |
| `crates/itsy/src/runtime/flows.rs` | Compiled MarrowScript |
| `crates/itsy/src/runtime/extensions.rs` | Compiled MarrowScript |
| `crates/itsy/src/runtime/logger.rs` | Compiled MarrowScript |
| `crates/itsy/src/runtime/metrics.rs` | Compiled MarrowScript |
| `crates/itsy/src/runtime/schemas.rs` | Compiled MarrowScript |
| `crates/itsy/src/runtime/tool_router.rs` | Compiled MarrowScript |
| `crates/itsy/src/code_graph/*` | Rust-native code graph (smallcode uses a separate npm package) |
| `crates/itsy/src/tools_impl/test_runner.rs` | Rust-only test discovery helper |

## Working order

Grind in this priority. Each row in tier 1 then tier 2 etc.

1. Tier 1 (5 files) — the agent loop
2. Tier 2 (12 files) — model / governance
3. Tier 3 (23 files) — session + tools
4. Tier 4 (16 files) — auxiliary

Total: ~56 Rust files to audit. At ~30 min/file average that's ~30 hours
of focused work. Realistically spans multiple sessions; this tracker
makes that resumable.
