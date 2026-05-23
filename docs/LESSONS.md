# Lessons from External Research

Sources: Anthropic long-runs talk (mR-WAvEPRwE), Cline talk (yUmS-F9IX90), Factory Missions talk (ow1we5PzK-o), distributed systems talk (2czYyrTzILg), harness talks (C_GG5g38vLU, am_oeAoUhew), skills talks (CEvIs9y1uog, JT3OzDKrucU), fundamentals talk (v4F1gFy-hqg), Claude Code architecture study (sanbuphy/learn-coding-agent).

---

## The self-evaluation trap

Models are poor judges of their own work. The same sycophancy that causes a model to agree with a user applies to its own output — it will look at a half-implemented feature, decide it looks done, and move on. A button exists but the backend does not. A game renders sprites but pressing keys does nothing.

*"Tuning a standalone critic to be harsh is very tractable. Tuning a builder to be somewhat self-critical is not."* — Anthropic talk

**What this means for itsy:** The assertions contract (where the model self-reports pass/fail) has a ceiling. At IQ2_XXS the model's assertions are often as wrong as its implementation. The adversarial evaluator pattern — a separate session with its own context window that actually runs the output — is the architecture that addresses this. The model cannot be its own QA.

---

## Contracts are written before code

The Anthropic team's contract negotiation happens before a single line of code exists. Generator proposes what it will build and how the evaluator should verify it. They iterate on a markdown file until both agree. The evaluator then grades only against that negotiated contract, not the original spec.

Factory's validation contract is the same idea: written during planning, defines correctness independently of how the code will be structured, before any implementation. "Tests written after the fact confirm decisions rather than catching bugs."

**What this means for itsy:** The current contract.rs is a single-session completion guard written by the model after it decides to start. It is backward from the right design. The model should negotiate what "done" means with an evaluator before writing any code. This is the missing piece in the assertions contract design.

---

## Every addition risks making it worse

Frontier models perform better with less instruction, not more. The system prompt for GPT-5.3 is one-third the size of the one for GPT-5 because newer models get overwhelmed by dense instruction sets. Cline has been rewritten from scratch at least seven times partly to shed accumulated cruft.

*"Every single thing you add to an agent risks making it worse."* — Cline talk

**What this means for itsy:** Before adding any new system-prompt section, tool, or feature: benchmark before and after. A new section that "should help" has just as much chance of hurting as helping at IQ2_XXS. The build-measure-revert discipline is not optional.

---

## Serial execution beats naive parallelism for software tasks

Factory tried running agents in parallel and found it does not work well. Agents conflict, step on each other's changes, duplicate work, make inconsistent architectural decisions. The coordination overhead eats the speed gains while burning tokens. Missions runs features serially with only one worker active at a time, and parallelizes only read-only operations (codebase search, API research) within a feature.

*"This is serial execution with targeted internal parallelization. It seems slower on paper, but the error rate drops dramatically."*

**What this means for itsy:** For multi-session contracts (if we ever build them), don't parallelize workers. Parallelize reads within a single worker session.

---

## Harnesses change shape as models improve

Features that were essential for weaker models become unnecessary or even harmful for stronger ones. Context resets between sessions were dropped entirely when Opus 4.6 eliminated context anxiety. Sprint decomposition was critical for Opus 4.5 but unnecessary for 4.6 which can hold a two-hour continuous build coherently.

*"The lesson isn't necessarily our harness was wrong, but rather it was right for 4.5, the frontier moved, and we ran a simplified version."*

**What this means for itsy:** The harness complexity visible in itsy (loop detection, nudge, plan tracker, contract) was tuned for IQ2_XXS. Any of these could be harmful or redundant with a stronger model. Re-baseline after model swaps.

---

## Read the traces

There is no shortcut. The primary debugging loop for harness-building is reading what the agent actually did, finding where its judgment diverged from a human's, and tuning for that specific case. "The same muscle as reading a stack trace." Only by spending time reading traces line by line — "oh, I can see why it did that" — can you learn which scaffold elements to keep and which to delete.

The Claude Code architecture study confirms this: piping agent transcripts into files and having a separate agent grep through them is a legitimate technique for closing the harness-building loop.

**What this means for itsy:** The logs in `jobs/<job>/agent/chats/` are the ground truth. Intuitions about what is failing are cheaper than looking at ten actual failure traces, but they are also wrong more often.

---

## Structured handoffs as connective tissue

When a worker finishes in Factory's Missions system, it fills out a structured handoff detailing what was completed, what was left undone, which commands were run and their exit codes, what issues were discovered. This forces agents to write down what happened rather than relying on context that will be lost. Errors surface at milestone boundaries. The longest mission ran 16 days.

The key property: next worker inherits a clean codebase via Git, not accumulated context baggage.

**What this means for itsy:** The single-session model sidesteps this entirely. If we ever extend to multi-session, file-based state (not context-window state) is the right medium for handoffs.

---

## Truthful failure over false success

Harness engineering rule: add a verify step before reporting success. A model that cannot reach a goal should stop claiming success. "You cannot fix what you cannot measure, and truthful failure is the foundation of reliable systems." The demo showed an agent claiming it had upvoted a post when it had not — because nothing verified the outcome.

**What this means for itsy:** The contract assertions are only meaningful if they are verified against external reality, not the model's self-report. A model running `./solution` and seeing output that looks right is not the same as a verifier checking behavioral correctness.

---

## The rate of feedback is the speed limit

Tight feedback loops (types, tests, lint, real runtime inspection) are the speed limit for agent velocity. Models often "outrun their headlights" by making sweeping changes before checking types or tests. TDD enforces small deliberate steps because it requires confirmation before proceeding.

**What this means for itsy:** For tasks with a compiler or test suite, itsy should be running checks frequently and stopping on failure rather than continuing to layer changes on a broken base.

---

## Context anxiety is a harness failure

When agents notice they are near the end of their context window they rush to finish, producing hasty or incomplete work. This is "context sense anxiety." It is a harness failure — the agent should never be placed in a position where it senses imminent context death and panics.

The Claude Code solution: server-side compaction that runs transparently, keeping the agent from ever seeing a near-full window.

**What this means for itsy:** The 750s bench timeout is a wall-clock issue, not a context issue, but the same principle applies. If the model reaches the timeout in a "panicked finishing" state the outputs are worse. The no-progress nudge helps by preventing long analysis loops, but context management itself is a possible lever.

---

## 12 harness mechanisms (Claude Code architecture study)

Claude Code layers production features progressively on the basic agent loop. In order:

1. The loop — while(true), call API, execute tools, append results
2. Tool dispatch — each tool registers into a dispatch map; loop stays identical
3. Planning — list steps first (TodoWrite), doubles completion rate
4. Sub-agents — fresh messages[] per child, keeps main conversation clean
5. Knowledge on demand — inject via tool_result, not system prompt (lazy loading)
6. Context compression — autoCompact (summarize) + snipCompact (trim) + contextCollapse
7. Persistent tasks — file-based task graph with status tracking
8. Background tasks — daemon threads, inject notifications on completion
9. Agent teams — persistent teammates with async mailboxes
10. Team protocols — single request-response pattern for all negotiation
11. Autonomous agents — idle cycle + auto-claim, no lead assignment needed
12. Worktree isolation — each task in its own directory, bounded by ID

**What this means for itsy:** Itsy implements roughly 1-4. Mechanisms 5-12 are the gap between a single-session coding tool and a production multi-day agent. The contracts spec in `/docs/contracts.md` is a design for mechanisms 7-12.

---

## Skills as composable expertise

The Anthropic / Supabase skills design: a skill is a folder of files. Progressive disclosure — only metadata loads until the skill is invoked. Critical guidance goes in skill.md, not separate files, because "if something can be skipped it will be skipped." Don't duplicate docs — point to authoritative source. Be opinionated: prescribe defaults that reduce mistakes.

*"The bottom line is not the context, it's the guidance."*

**What this means for itsy:** The system prompt's role is guidance density, not information coverage. One opinionated sentence about how to approach a task type is worth more than three paragraphs of context the model will skim.

---

## Distributed systems thinking for multi-agent

Multi-agent systems are distributed systems and fail for the same reasons. Shared mutable state causes race conditions. Cache invalidation is as hard for agents as for databases. The right patterns: immutable state snapshots with versioning (append-only log, version numbers), data contracts at agent boundaries (validate on receipt), circuit breakers to prevent cascading failures, orchestration over choreography for debuggability and auditability.

*"The problem was we built a distributed system without distributed system thinking. And that's what kills multi-agent projects, not bad AI, but bad architecture."*

**What this means for itsy:** The contracts spec uses files for shared state (state.json, progress_log.jsonl, handoffs/) specifically to avoid in-memory shared state between sessions. This is correct.

---

## Silent reasoning trace degradation

If reasoning traces (cached intermediate outputs from thinking-enabled models) are sent back in a format that differs even slightly from what the API expects, the model still responds but at degraded performance — with no error or warning. Developers using abstraction layers may be silently losing gains.

**Status for itsy:** Investigated and tested live. The IQ2_XXS model at 10.0.2.2:8000 handles `reasoning_content` in incoming history messages correctly in the simple 2-turn case. The Jinja2 template on the llama-server side picks it up. No degradation detected. The concern remains hypothetical for the 15-20 turn accumulated case, but is not confirmed as a problem.
