# Failure Mode Analysis

Based on 72 trials across the terminal-bench-2 scoreboard (11 tasks × ~3 attempts each, IQ2_XXS quantization, ~2 bits/weight).

## Top-line breakdown

| Failure mode | Share | Count (of 72) |
|---|---|---|
| Wrong answer | 69% | ~50 trials |
| Timeout (750s) | 23% | ~17 trials |
| Loop / stuck | 9% | ~6 trials |

The loop and timeout fixes (cross-turn loop detection, no-progress nudge) address at most the 32% tail. The dominant failure is correctness.

---

## Wrong answer (69%)

The model produces output, exits cleanly, and the bench verifier rejects it.

Two distinct sub-modes:

### 1. Stops too early

Model decides it is done before it actually is. Marks itself complete, exits. No timeout, no loop — just premature confidence.

**What helps:** the assertions completion guard (`contract.rs`). Forces the model to enumerate what "done" means up front, and refuses to let it close the contract while any assertion is still `pending`. Addresses this sub-mode directly.

### 2. Implements the wrong thing

Model writes code, runs its own checks, everything passes locally, but the bench verifier still rejects. The implementation is structurally plausible but semantically wrong — wrong algorithm, wrong file edited, wrong interpretation of the task spec.

**What helps:** nothing in the current tooling. This is a capability floor.

At IQ2_XXS the model loses so much reasoning resolution that it routinely:
- Misreads which part of the codebase to change
- Writes code that compiles and produces output but fails the behavioral contract
- Marks its own assertions `passed` based on superficial checks that don't match the verifier

No orchestration wrapper, multi-agent runner, or loop guard fixes this. The assertions the model writes are often just as wrong as the implementation.

---

## Timeout (23%)

Session hits the 750s wall clock limit. Usually caused by one of:
- Analysis paralysis — model reads files in a loop without writing anything (→ nudge fix)
- Infinite compile-check-patch cycle on a genuinely hard task
- Orphaned tool calls that never resolve

**What helps:** the no-progress nudge (fires after 3–6 consecutive read-only batches). Partially addressed.

---

## Loop / stuck (9%)

Model calls the same tool with the same arguments repeatedly, either across turns or within a single turn. Usually a read_file loop on the same path, or a bash loop running the same failing command.

**What helps:** the cross-turn loop detection (`tool_repeat_counts` in `AgentSession`). Addressed.

---

## What would actually move the 69%

The wrong-answer problem is a capability issue at the model tier, not a tooling issue. Things that would measurably help:

| Approach | Mechanism | Expected lift |
|---|---|---|
| Larger / less quantized model | More reasoning resolution → fewer misreads | High — but changes the bench target |
| Better task-specific prompting | Front-load the most common misread patterns per task type | Moderate — diminishing returns quickly |
| Assertions guard (already implemented) | Prevents premature exit sub-mode | Low-moderate — only hits the "stops too early" slice |
| Multi-agent contracts (orchestrator + workers) | Wrong tool — requires a capable orchestrator, adds overhead, same broken core model | None for IQ2_XXS single-session tasks |

The honest conclusion: at IQ2_XXS, roughly 35–40% of the wrong-answer failures are recoverable with better tooling (completion guards, nudges, loop detection). The remaining ~30% of total trials require a better model.
