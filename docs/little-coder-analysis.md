# little-coder analysis

Source: https://github.com/itayinbarr/little-coder

Reviewed 2026-05-24 against itsy's current feature set.

## What little-coder has

### 1. Recency-weighted tool guidance (adopted)

When the same tool fails twice in a row, inject a short actionable hint before
the next model call. The hint is generic — it says things like "try
`read_and_patch` instead" for repeated `patch` failures — not task-specific.

**Why it helps:** At IQ2 quantisation the model does not reliably change
approach after a single failure. A second identical error with a targeted
hint shifts it out of the loop without spending extra turns.

**Implemented in:** `crates/itsy/src/bin/itsy.rs` — `recency_tool_hint()`
plus the `recent_tool_failures` counter in the per-turn state block.

---

### 2. Knowledge injection (not adopted)

little-coder maintains a library of task-specific cheat sheets (e.g. "for
overfull-hbox tasks: shorten lines using `\linebreak`, never delete words").
These are injected into the system prompt at runtime based on task name or
keyword matching.

**Why we didn't take this:** It is not scalable and it's just a cheat for
passing benchmarks. Cheat sheets overfit to the exact benchmark tasks and
tell you nothing about how the agent performs in the wild. Any improvement
you see is the cheat sheet doing the work, not the agent. You can't ship
task-specific hints to users who are working on arbitrary codebases.

The right fix for an agent that fails a task is better tooling, better
prompting strategy, or a stronger model — not a hard-coded answer key.

---

### 3. Thinking budget abort+retry (adopted)

When a reasoning model returns `finish_reason=length`, it was cut off
mid-thinking. The model emits no tool calls and the turn is wasted. little-coder
retries the same request with thinking disabled so the model skips the thinking
phase and produces an answer in the remaining token budget.

**Why it helps:** At tight `max_tokens` settings (or when the task is long
and the budget is nearly full), the model occasionally overshoots its
thinking allocation. One retry is cheap. Without it the turn silently
produces nothing and the next turn re-issues the same thinking budget,
potentially looping.

**Implemented in:** `crates/itsy/src/model_client.rs` — after `chat_log::record`,
detect `finish_reason == "length"` with `tokens > 0` and `attempt == 1`,
then call `apply_thinking_budget(..., disable=true)` and continue the loop.
