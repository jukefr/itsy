# little-coder full audit

Source: https://github.com/itayinbarr/little-coder  
Reviewed: 2026-05-24 — full source read of all 23 extensions + 30 skill files +
benchmark write-ups.

little-coder is pi (minimal agent substrate) plus 23 TypeScript extensions that
handle everything from tool-use guidance injection to browser automation to
evidence-aware compaction. Their leaderboard result on TB 2.0 is 24.6% ± 3.2 at
5 attempts × 89 tasks with Qwen3.6-35B-A3B IQ2_XXS — roughly the same model and
bench we use.

---

## What we already implemented (from the earlier analysis)

### 1. Recency-weighted tool guidance (adopted, simpler version)

**What they have (`skill-inject`):** A 3-priority selection system that injects
per-tool markdown "skill cards" into the system prompt on every turn:

1. **Error recovery**: if the last tool call failed, force-include that tool's
   card first.
2. **Recency**: include cards for tools used in the last 2–4 turns.
3. **Intent prediction**: scan user message for keywords ("fix" → Edit, "run" →
   Bash, etc.) and include predicted tools' cards.

Each card is a full markdown doc with exact syntax, examples, and recovery steps
(e.g. `edit.md`: "String not found → Read the file, fix oldText, retry. Do NOT
fall back to Write."). 13 cards total across Read, Write, Edit, Bash, Glob, Grep,
WebFetch, ShellSession, BrowserNavigate, etc. Budget-aware (max ~300 tokens);
selections are cached by frozenset.

**What we have (`recency_tool_hint`):** A much simpler version: if the _same_
tool fails ≥2 times in a row, inject a one-line hint string. No proactive
injection, no error-recovery priority on first failure, no intent prediction.

**Gap:** Their system is proactive; ours is only reactive after two failures.
Their recovery rate shows 57% of Polyglot exercises where the model would have
tried to Write over an existing file were caught before the second failure.

**Adopting the full system?** The tool cards themselves (the markdown files) are
generic enough to be legitimate. The selection algorithm is general-purpose.
Worth a port — but it requires adding the markdown files and wiring the
before-API-call injection, which is non-trivial.

### 3. Thinking budget abort+retry (adopted)

**What they have (`thinking-budget`):** Monitors token count in real-time during
streaming via `message_update` events. When thinking tokens cross the budget
mid-stream, it synchronously (before calling `ctx.abort()`): captures current
thinking level, sets it to "off", sends a "commit to implementation now" follow-up
via `sendUserMessage`, surfaces a harness-intervention line, then calls
`ctx.abort()`. Thinking remains forced-off across all restart turns until the
next genuine user input, at which point the prior level is restored.

The detailed test suite in `budget.test.ts` pins a subtle bug they hit twice:
when `ctx.abort()` triggers a session replacement, any deferred (setTimeout,
turn_end) handler runs against a stale `pi` reference and throws silently —
recovery never fires. Their fix: do everything synchronously in `message_update`
before the abort.

**What we have:** We detect `finish_reason == "length"` after the full response
is received and retry with `apply_thinking_budget(..., disable=true)`. Simpler
and correct for llama-server (which doesn't expose mid-stream abort), but we
don't send a "stop deliberating" nudge.

---

## What they have that we don't (and should evaluate)

### A. Context-aware read trimming (`read-guard`)

**What they do:** On every `tool_result` for a Read, check whether the result
size would push the current context past the window. If so, replace the result
with the first 30 lines plus an explicit directive: "this file is too large —
search with grep/find then read only the relevant range, don't re-read it in full."

Uses `ctx.getContextUsage()` to get live token count. Falls back to "trim if the
file alone exceeds 50% of the window" when current usage is unknown.

**What we have:** `cap_tool_result()` with a hard 4000-char limit regardless of
context state. Never tells the model why, and doesn't offer a grep/targeted-read
strategy.

**Why it matters:** On long tasks (kv-store-grpc, cobol-modernization) the model
reads large files early in the turn budget and fills its context. Our dumb cap
returns a truncated file silently, which causes the model to work from incomplete
information without realizing it. Their approach preserves structure (first 30
lines = imports + function signatures) and redirects to a cheaper strategy.

**Verdict:** Adoptable. The mechanism is general — no task-specific knowledge.

---

### B. Quality monitor (`quality-monitor`)

**What they do:** After each turn, inspect the assistant message for four failure
modes:

1. **Empty response** (no text, no tool calls)
2. **Empty tool name** (model emitted a tool call with `name: ""`)
3. **Hallucinated tool** (tool name not in the known registry)
4. **Repeated tool call** (exact same name+args as previous turn)

On failure, inject a targeted correction via `sendUserMessage` with
`deliverAs: "steer"` so it lands on the model immediately, not at the next user
prompt. Cap at 2 consecutive corrections to avoid a correction spiral. Back off
with a harness-intervention warning after the cap.

Key design point: skip quality checks for turns that were aborted
(`stopReason == "aborted"`) to avoid spuriously firing "empty response" on a
thinking-budget abort.

**What we have:** `per_turn_repeats` tracks some loop detection, `early_stop`
has its own repetition check, and we have the no-progress nudge from `EarlyStopDetector`.
But we don't check for empty responses, hallucinated tool names, or inject
immediate corrections.

**Verdict:** Their data is compelling — 28 tasks on TB triggered at least one
quality-monitor correction, meaning the failure modes are real. The correction
cap (≤2) is important to prevent a correction spiral. Adoptable.

---

### C. Turn cap at the task level (`turn-cap` + `finalize-warn`)

**What they do:** `turn-cap` counts total turns fired during a task (not calls
per turn). When `turnsThisRun > capForRun`, abort immediately. Benchmark
overrides set `max_turns: 40` for terminal_bench. `finalize-warn` fires 5 turns
before the cap and sends "you have 5 turns left, produce your final answer now,
end with `Answer: <value>`" — prevents the model running out of turns mid-thought
with no final line.

Their TB analysis: at cap=25, ~20/80 tasks hit the cap and all failed. At cap=40:
8/80 hit cap, 7 fail, 1 passes. 88% of cap-hits still fail — those are
genuinely lost tasks — but bumping to 40 freed ~12 tasks that needed 26–39 turns.

**What we have:** `max_tool_calls_per_turn` limits per-LLM-call tool batches, but
there's no global per-task turn limit. The `--agent-timeout-multiplier` covers
the harbor wall-clock timeout but not an agent-internal turn budget.

**Verdict:** We don't need `finalize-warn` (our tasks don't require a final
`Answer: <value>` line). A turn cap is potentially useful for preventing runaway
tasks, but harbor's own timeout already handles the extreme case. Low priority.

---

### D. Live context window probe (`llama-cpp-provider`)

**What they do:** At startup, hit the llama-server `/props` endpoint (which is at
the root, not under `/v1`) and read `default_generation_settings.n_ctx`. If the
probe succeeds, register all models with the live `contextWindow` rather than the
static value from `models.json`. Falls back silently on any failure.

This matters because a user might start `llama-server -c 131072` but the static
config says 32768. Without the probe, read-guard and context budget decisions
use the wrong window.

**What we have:** itsy uses whatever context window is in `config.toml`, which
the user sets manually.

**Verdict:** Nice quality-of-life feature. Low priority for bench work (we control
the launch), higher priority for interactive use.

---

### E. BrowserExtract retention pruning (`browser-extract-retention`)

**What they do:** When browsing multiple pages (GAIA research tasks), each
BrowserExtract call returns 2KB chunks that accumulate in context. After the
model distills the relevant facts via EvidenceAdd, the raw chunks are redundant.
This extension replaces all but the 2 most recent BrowserExtract tool-results
with compact placeholders (URL, original size, list of Evidence IDs that cited
that URL).

**What we have:** Irrelevant — we don't use Browser tools in terminal-bench.

**Verdict:** Not applicable to our benchmark tasks.

---

### F. Evidence store (`evidence` + `evidence-compact`)

**What they do:** `EvidenceAdd/Get/List` tools give the model a per-session
scratchpad for citable facts, intended for GAIA research tasks where the model
needs to cite before answering. `evidence-compact` preserves the store across
pi's context compaction by injecting a bridge message after compact events.

**What we have:** Nothing equivalent, but it's GAIA-specific.

**Verdict:** Not applicable to our benchmark tasks.

---

### G. Bash whitelist (`permission-gate`)

**What they do:** Block any Bash command not matching a prefix whitelist (ls, cat,
head, tail, git log/status/diff, find, grep, cp, mv, mkdir, touch, etc.) in
`auto` mode. Customizable via `LITTLE_CODER_BASH_ALLOW` env. `rm` and `sudo`
deliberately excluded from defaults. Bench runner sets
`LITTLE_CODER_PERMISSION_MODE=accept-all` to skip the gate entirely.

**What we have:** itsy has `security.allow_public_endpoints` and other flags but
no bash whitelist at the tool level.

**Verdict:** Relevant for interactive use (prevents model running `rm -rf`
accidentally), less relevant for bench tasks where we want full shell access.
Low priority for bench.

---

### H. Per-benchmark overrides in profiles (`benchmark-profiles`)

**What they do:** `.pi/settings.json` has per-model profiles that can include
`benchmark_overrides`:

```json
"llamacpp/qwen3.6-35b-a3b": {
  "thinking_budget": 4096,
  "benchmark_overrides": {
    "terminal_bench": {
      "thinking_budget": 3000,
      "temperature": 0.2,
      "max_turns": 40
    },
    "gaia": {
      "thinking_budget": 2000,
      "temperature": 0.4,
      "max_turns": 40,
      "context_limit": 65536
    }
  }
}
```

Set `LITTLE_CODER_BENCHMARK=terminal_bench` and all overrides apply
automatically.

**What we have:** itsy's config.toml has `[features]` flags and the
`--thinking-budget` CLI flag, but no structured benchmark-override mechanism.

**Verdict:** Their TB override uses `thinking_budget: 3000`, lower than the
default 4096. Their data says all 11 tasks that hit the 3000 budget failed, but
they note this could be selection bias (hard tasks think more). The
`temperature: 0.2` setting is meaningful — their llama-server default is ~0.8
which adds variance; 0.2 reduces it.

**Why we're not adopting this:** Temperature tuning for a specific benchmark is
the same class of problem as knowledge injection — it's fitting a hyperparameter
to the answer key, not improving the agent. A temperature that reduces variance
on TB 2.0 may increase it on a different task distribution. The right fix for
high-variance outputs is a stronger model or better agent logic, not a
benchmark-specific sampling temperature. Not adopting.

---

## What we have that they don't

- **Contract system**: model proposes assertions, marks pass/fail, adversarial
  evaluator verifies independently. No equivalent in little-coder.
- **Code graph tools** (`graph_search`, `explain_symbol`): codebase indexing.
  Not in LC.
- **`read_and_patch`**: atomic read+patch tool that avoids stale-content mismatches.
  LC relies on their Edit skill card to guide the model to Read first.
- **Thinking budget applied at the HTTP request level**: itsy sets
  `chat_template_kwargs.enable_thinking`, `thinking_budget_tokens`, and
  `reasoning_effort` on the outgoing request. LC manages the budget via
  mid-stream abort and disabling thinking at the pi level.
- **musl binary**: itsy compiles to a static musl binary for bench compatibility.
  LC depends on the host Node.js installation.

---

## Summary by priority

| Feature | Priority | Complexity | Notes |
|---|---|---|---|
| Full tool skill card system (adopt LC's markdown + 3-priority selection) | **High** | Medium | Their data shows 57% of Write spirals caught pre-failure. Proven mechanism. |
| Context-aware read trimming (`read-guard`) | **High** | Low | Our dumb 4000-char cap is worse for long tasks. No task-specific knowledge. |
| Quality monitor (empty/hallucinated/loop detection with steer injection) | **Medium** | Low | 28/80 TB tasks hit it; itsy sees the same failure modes |
| Temperature control per bench run | **Rejected** | — | Benchmark-specific hyperparameter fitting. Same class of problem as knowledge injection — not adopting. |
| Turn cap at task level | **Low** | Low | Harbor timeout covers the extreme case already |
| Live context window probe | **Low** | Low | Only matters if users start llama-server with non-default `-c` |
| Bash whitelist | **Low** | Low | Relevant for interactive use only |
| Evidence store / BrowserExtract retention | **N/A** | — | GAIA-only features |

---

## What we explicitly rejected

### 2. Knowledge injection (`knowledge-inject`)

little-coder loads 13 algorithm cheat sheets (binary search, dynamic programming,
BFS/DFS, two pointers, etc.) and injects whichever ones score ≥2.0 against the
user's message keywords.

**Why we didn't take this:** Not scalable and just a cheat for passing benchmarks.
The cheat sheets overfit to the exact tasks in the benchmark catalog. Any
improvement they produce is the answer key doing the work, not the agent. Users
working on arbitrary codebases don't have matching cheat sheets. The right fix
for an agent that fails an algorithmic task is a stronger model or better tooling,
not a hard-coded answer key.

### H. Temperature control per bench run (`benchmark-profiles`)

little-coder sets `temperature: 0.2` for terminal-bench runs vs. a default of ~0.8.

**Why we didn't take this:** Same class of problem as knowledge injection —
fitting a hyperparameter to the benchmark's task distribution rather than improving
the agent. A lower temperature reduces variance on TB 2.0 specifically, but there's
no principled reason it would generalise to other task distributions or real use.
The right fix for high-variance model outputs is a better model or better agent
logic, not a sampling temperature tuned to the answer key.
