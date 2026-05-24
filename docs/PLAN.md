# Plan: Remaining Work

## P0 — Extract handle_turn guards (ongoing)

**Done:**
- `TurnState` struct with all per-turn counters
- `AgentSession` moved to library (`runtime::agent_loop`)
- Foundation for importing handle_turn from library crate

**Remaining (est. 2-3 sessions):**

Extract each guard into a `TurnState` method. The pattern:

```rust
// TurnState method
pub fn check_quality_monitor(
    &mut self,
    tool_calls: &[Value],
    known: &[&str],
    history: &mut Vec<Value>,
) -> Option<GuardAction> { ... }
```

Guards to extract (~6):
1. **Quality monitor** — invalid tool names, hallucinated tools
2. **Contract gate** — refuse mutating tools before contract exists
3. **Idempotent write dedup** — repeat `mark_assertion` / `memory_remember`
4. **No-progress nudge** — read-only streak with no mutations
5. **Text-only streak** — consecutive text responses without tools
6. **Contract close-loop** — `close_contract` not called with assertions pending

---

## D2 — Tokenizer (optional)

Chars/4 heuristic is acceptable. Tokenizer dependency adds ~10MB binary weight.
Skip unless history compaction becomes a measurable problem.

---

## D3 — Complete call site migration for providers

The four providers (`read_tracker`, `file_state`, `knowledge_loader`,
`snapshot_manager`) are on the compartment structs and `ExecCtx`. But ~25 call
sites in `executor.rs` still use the old `get_read_tracker()` globals. Migrate
them to `ctx.read_tracker` etc.

---

## verify_code sandbox edge cases

The temp sandbox in `verify_code` is new. Monitor for:
- Temporary file leaks if the compiler crashes mid-flight
- Path traversal: `file_path = "../../etc/passwd"` + copy to sandbox
