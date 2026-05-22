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
- **trust_decay.rs** — UNVERIFIED: Rust adds time-based half-life exponential decay to
  `consecutive_fails` and a smoothed `trust` 0-1 score. JS has no time decay — just a raw
  counter. `filter_and_sort` behavior is equivalent. No bench evidence for the decay addition.
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
| `crates/itsy/src/executor.rs` | `bin/executor.js` | `PARTIAL` | — (port-incomplete; see notes) |
| `crates/itsy/src/tools.rs` | `bin/tools.js` | `AUDITED` | (no changes needed) |
| `crates/itsy/src/model_client.rs` | `bin/model_client.js` | `AUDITED` | (pending) |
| `crates/itsy/src/bin/itsy.rs` | `bin/smallcode.js` | `PARTIAL` | (pending) |

## Tier 2 — model + governance

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/governor.rs` | `bin/governor.js` | `AUDITED` | (no changes needed) |
| `crates/itsy/src/governor/early_stop.rs` | `src/governor/early_stop.js` | `PARTIAL` | sizes 137/143, structurally aligned |
| `crates/itsy/src/escalation.rs` | `bin/escalation.js` | `PARTIAL` | sizes 349/275 |
| `crates/itsy/src/trace_recorder.rs` | `bin/trace_recorder.js` | `AUDITED` | bloat is typed structs + 3 obs-only methods (INTENTIONAL) |
| `crates/itsy/src/token_monitor.rs` | `bin/token_monitor.js` | `PARTIAL` | sizes 125/84, structurally aligned |
| `crates/itsy/src/model/adaptive_router.rs` | `src/model/adaptive_router.js` | `UNVERIFIED` | 4.2x bloat (Wilson bounds, persistence); DEAD code — not called |
| `crates/itsy/src/model/adaptive_temp.rs` | `src/model/adaptive_temp.js` | `PARTIAL` | sizes 118/85, structurally aligned |
| `crates/itsy/src/model/chain.rs` | `src/model/chain.js` | `PARTIAL` | sizes 255/162, structurally aligned |
| `crates/itsy/src/model/profiles.rs` | `src/model/profiles.js` | `AUDITED` | bloat is disk-profile override (opt-in via file presence, no default behaviour change) |
| `crates/itsy/src/model/reviewer.rs` | `src/model/reviewer.js` | `AUDITED` | bloat is parse_reviewer_output helper + typed config struct, behaviour matches |
| `crates/itsy/src/model/router.rs` | `src/model/router.js` | `PARTIAL` | sizes 119/63, Rust adds Complexity enum |
| `crates/itsy/src/model/thinking_budget.rs` | `src/model/thinking_budget.js` | `PARTIAL` | sizes 260/207, Rust adds REASONING_MODEL_RE for Qwen3 detection (INTENTIONAL) |

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
| `crates/itsy/src/tools_impl/mcp_client.rs` | `src/tools/mcp_client.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/read_tracker.rs` | `src/tools/read_tracker.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/shell_session.rs` | `src/tools/shell_session.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/trust_decay.rs` | `src/tools/trust_decay.js` | `AUDITED` | — |
| `crates/itsy/src/tools_impl/web_browse.rs` | `src/tools/builtin/web_browse.js` | `AUDITED` | — |
| `crates/itsy/src/runtime/two_stage_router.rs` | `src/tools/two_stage_router.js` | `AUDITED` | — |
| `crates/itsy/src/memory.rs` | `bin/memory.js` | `AUDITED` | — |
| `crates/itsy/src/memory/evidence.rs` | `src/memory/evidence.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/knowledge.rs` | `src/knowledge/loader.js` | `NOT_AUDITED` | — |

## Tier 4 — auxiliary (UI, init, adapters)

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/tui.rs` | `bin/tui.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/fullscreen.rs` | `src/tui/fullscreen.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/commands.rs` | `bin/commands.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/config.rs` | `bin/config.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/init_wizard.rs` | `bin/init.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/features_adapter.rs` | `bin/features_adapter.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/cognition_adapter.rs` | `bin/cognition_adapter.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/loops_adapter.rs` | `bin/loops_adapter.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/mcp_bridge.rs` | `bin/mcp_bridge.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/eval_runner.rs` | `bin/eval_runner.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/lsp.rs` | `src/lsp/client.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/api.rs` | `src/api/index.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/adapters/acp.rs` | `src/adapters/acp.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/plugins/loader.rs` | `src/plugins/loader.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/plugins/skills.rs` | `src/plugins/skills.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/security.rs` | `src/security/sanitize.js` | `NOT_AUDITED` | — |

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
