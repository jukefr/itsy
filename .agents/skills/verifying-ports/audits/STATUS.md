# Port audit status

Tracks per-file audit state against upstream JS. Update as you complete
each file. One commit per audit, with `### Upstream vs port` in the
commit body (see the parent skill).

**Upstream rev pinned:** `1db07104af9df709cec086dffdfc4bf65cceae8d` (`upstream/master`)
This is the same SHA in `.agents/skills/upstream-changes/upstream-rev`.
When that SHA bumps, re-baseline all audits against the new tree.

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
| `crates/itsy/src/model_client.rs` | `bin/model_client.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/bin/itsy.rs` | `bin/smallcode.js` | `NOT_AUDITED` | — |

## Tier 2 — model + governance

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/governor.rs` | `bin/governor.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/governor/early_stop.rs` | `src/governor/early_stop.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/escalation.rs` | `bin/escalation.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/trace_recorder.rs` | `bin/trace_recorder.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/token_monitor.rs` | `bin/token_monitor.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/adaptive_router.rs` | `src/model/adaptive_router.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/adaptive_temp.rs` | `src/model/adaptive_temp.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/chain.rs` | `src/model/chain.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/profiles.rs` | `src/model/profiles.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/reviewer.rs` | `src/model/reviewer.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/router.rs` | `src/model/router.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/model/thinking_budget.rs` | `src/model/thinking_budget.js` | `NOT_AUDITED` | — |

## Tier 3 — session + tools

| Rust | Upstream JS | State | Commit |
|---|---|---|---|
| `crates/itsy/src/session/bootstrap.rs` | `src/session/bootstrap.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/clarify.rs` | `src/session/clarify.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/file_state.rs` | `src/session/file_state.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/git_context.rs` | `src/session/git_context.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/images.rs` | `src/session/images.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/multi.rs` | `src/session/multi.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/persistence.rs` | `src/session/persistence.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/plan_tracker.rs` | `src/session/plan_tracker.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/references.rs` | `src/session/references.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/share.rs` | `src/session/share.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/snapshot.rs` | `src/session/snapshot.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/tokens.rs` | `src/session/tokens.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/session/undo.rs` | `src/session/undo.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/file_tree.rs` | `src/tools/file_tree.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/mcp_client.rs` | `src/tools/mcp_client.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/read_tracker.rs` | `src/tools/read_tracker.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/shell_session.rs` | `src/tools/shell_session.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/trust_decay.rs` | `src/tools/trust_decay.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/tools_impl/web_browse.rs` | `src/tools/builtin/web_browse.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/runtime/two_stage_router.rs` | `src/tools/two_stage_router.js` | `NOT_AUDITED` | — |
| `crates/itsy/src/memory.rs` | `bin/memory.js` | `NOT_AUDITED` | — |
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
