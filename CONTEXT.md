# CONTEXT

Project vocabulary for architectural conversations. Terms here are anchors —
the words future architecture reviews, ADRs, and PR descriptions should reach
for first. When a term gets fuzzy, sharpen it here.

For *what to do* (workflows, commit conventions, tool-add ritual), see
`AGENTS.md`. CONTEXT.md is about *what things are called* and *what each name
means*.

---

## Layout: `runtime::*` is the spine

`runtime` is the deterministic core. Everything outside it (`tui`,
`fullscreen`, `session`, `executor`, `bin/itsy.rs`) sits at the edges and
talks to runtime through narrow seams.

The four runtime sub-spines have non-overlapping jobs. Naming a new module
under `runtime` is a choice between these four:

- **`runtime::providers`** — HTTP transport to LLM endpoints. Owns
  `(model, base_url, thinking_budget, max_tokens, headers, …)`. Nothing in
  providers knows what a "task type" or a "contract" is. The canonical
  one-shot is `runtime::providers::openai_compat::chat_oneshot`.
- **`runtime::cognition`** — pre-prompt routing. Task classification
  (`classify_task_type`), tier routing (`coding_router_route`), history
  compression. Decides *which* prompt to run and *which* model tier to send
  it to. Calls providers; never called by them.
- **`runtime::features`** — *prompt orchestration*. Each submodule is one
  orchestrated LLM workflow: a prompt + response parsing + a small policy
  around retries / second-opinion / fallbacks. Examples: `decompose`,
  `contract_review`, `verify_and_fix`, `commit_message`, `summarize`,
  `diagnose`, `clarify`, `repair`, `semantic_merge`. **Not** "feature
  flags". **Not** Cargo features. One LLM workflow per file.
- **`runtime::flows`** — control-flow primitives. Saga compensation,
  bounded retry loops, validation harnesses. Stateless logic about how to
  sequence work; doesn't know about prompts or HTTP.

If a candidate module doesn't fit any of these four, the seam is probably
wrong, not the module.

---

## Historical: the adapter belt

For a while `crates/itsy/src/*_adapter.rs` — `cognition_adapter`,
`features_adapter`, `loops_adapter`, and `adapters.rs` itself — were
the public-facing front for runtime. They were a **port mirror** (see
below) of the JS upstream's module shape, not a real architectural layer.

Most adapter functions were one-line forwarders to `runtime::*`. A few
were doing real work (`execute_flow`, `direct_chat`, multi-round
prompt logic for `decompose_task` / `negotiate_assertions`) but in the
wrong module.

The decision: **collapse the adapter belt into `runtime::*`.** When you
see "adapter belt" in a commit or review, this is what it means.

If a future port-mirror layer accretes for the same reason, name it
something other than "adapter" — the word has a specific architectural
meaning (an interface implementation at a seam, in `LANGUAGE.md` terms)
that the JS-port wrapper layer never satisfied.

---

## Port mirror

Pattern inherited from the JS upstream (`github.com/Doorman11991/smallcode`):
Rust modules mirror the JS module structure even where the JS structure was
incidental. Symptoms:

- Dead stubs that match a JS API: `is_compiled_cognition_available() ->
  true`, `get_compiled_provider() -> None`. These exist because the JS
  caller checked them; the Rust caller can't.
- Function names with `_compiled` suffix (`classify_task_compiled`,
  `validate_edit_compiled`) — distinguishes "compiled cognition" from
  "regex fallback" in JS, but the distinction collapses in Rust because
  the compiled layer is statically linked.
- Two implementations of the same algorithm where one is the original JS
  port and the other is the idiomatic-Rust rewrite that nobody deleted
  the first one for (`cognition_adapter::estimate_complexity` vs
  `model::router::estimate_complexity`).

When porting, prefer the JS-mirror structure as a *scaffold*; collapse it
once the Rust version is stable. The `.agents/skills/upstream-changes/`
flow tracks the per-commit decision of port-vs-skip; it does not require
the resulting Rust shape to match the JS shape forever.

---

## Tools (briefly)

- **Tool** — an LLM-callable action. Schema in `tools.rs::TOOLS`,
  dispatch in `executor.rs::execute_tool`, category in
  `runtime/tool_router.rs`. The split across three files is a known
  shallow seam; see architecture-review candidate 2 (ToolCatalog).
- **Compound tool** — an orchestrated multi-step tool (e.g.
  `read_and_patch`). Declared alongside primitive tools in
  `tools.rs::COMPOUND_TOOLS`.

---

## Session, contract, evidence, memory, knowledge

These have overlap that has not been fully sharpened (see
architecture-review candidate 5 — MemoryStore reseat). Working
definitions:

- **session** — one conversational run. Persisted under
  `paths::sessions_dir(cwd)`. Owns the message list and per-turn metadata.
- **contract** — the assertion ledger for a task. Lives on disk under
  the session. Defines what success looks like and tracks per-assertion
  pass/fail evidence.
- **evidence** — observations recorded during a turn (files read,
  commands run, errors seen). Some evidence promotes into `memory` at
  end-of-turn.
- **memory** — persistent recall across sessions. Stored in SQLite/FTS5
  (with a JSON fallback) under `paths::memory_db(cwd)`. The trait
  `MemoryStore` is the seam, though it currently lives in
  `memory/evidence.rs` instead of `memory.rs` — pending reseat.
- **knowledge** — *prompt-time* keyword-injected content. Not
  persistent; not recallable. Distinct from memory despite the adjacent
  naming.

---

## Rendering

- **classic mode** — line-based output via `tui.rs`. Pure functions
  returning ANSI-flavoured strings.
- **fullscreen mode** — ratatui alternate-screen via `fullscreen.rs`.
  Stateful.

The two modes share no interface today (see architecture-review
candidate 3 — `Renderer` trait). The agent loop in `bin/itsy.rs` is
coupled to the fullscreen path; classic is a parallel re-implementation.
