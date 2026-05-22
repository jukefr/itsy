---
name: benchmark-driven-development
description: Use when adding, tuning, or removing any agent-behaviour change in itsy — new tools, prompt edits, config flags, model-client tweaks, dedup heuristics, or anything else that could shift bench scores. The discipline is: measure first, change second, measure again, ship only if the numbers moved the right way.
---

# benchmark-driven-development

## Overview

**No agent-behaviour change ships without a measured before/after on terminal-bench-2.**

Vibes are not evidence. "It should help" is not a result. A feature is allowed to land only if a side-by-side bench run against the same baseline shows it moved the metric we care about — or, if it didn't, the feature is reverted or kept behind a flag that defaults off.

## When to use

- About to commit ANY change that could affect the agent's decision making: new tool, schema change, prompt edit, system-prompt section, plan-tracker tweak, dedup rule, max-tokens heuristic, model-client retry logic, new `[features]` flag, etc.
- A change "feels obviously right" — that's exactly when you've skipped measurement.
- Re-tuning an existing feature ("bump max_tool_calls_per_turn from 250 to 400")
- Comparing two implementations of the same thing.

**Don't use for:** pure bugfixes that match an existing test, dependency bumps, doc-only changes, internal refactors that don't change behaviour.

## The flow

1. **Pin a baseline commit.** `git rev-parse HEAD` before you touch anything. That's your reference.
2. **Run a baseline benchmark.** Use the [terminal-bench-2](../terminal-bench-2/SKILL.md) skill. For most changes, run the 11-task scoreboard at `--n-attempts 3 --n-concurrent 1`. Store the resulting `result.json` under `bench/baselines/<commit-sha>.json`.
3. **Implement the feature.** No rush — the baseline is now your ground truth.
4. **Run the same benchmark on the feature branch.** Same model, same tasks, same attempts, same concurrency. Store under `bench/baselines/<feature-branch-name>.json`.
5. **Compare.** Use `bench/baselines/diff.py` (create it if missing — see Implementation). Look at:
   - Mean reward delta (the headline)
   - Per-task pass-count diff (no task should regress)
   - Wall-clock delta (a 5% reward gain that doubles wall time is usually a bad trade)
   - Failure-mode distribution shift (did we trade `stuck-loop` for `verifier-correctness`?)
6. **Decide.** Pass → commit. Fail → revert or gate behind a default-off flag. Mixed → write up the trade-off in the commit body so future-you can re-evaluate.

## Quick reference

| Step | Where the artifact lives |
|---|---|
| Baseline commit SHA | commit message of the feature PR |
| Baseline result.json | `bench/baselines/<sha>.json` |
| Feature-branch result.json | `bench/baselines/<branch>.json` |
| Diff report | `bench/baselines/diff.py <baseline> <feature>` |
| Per-task verifier outputs | inherited from terminal-bench-2 layout |

## Implementation

For the diff tool — keep it simple, one Python file:

```python
# bench/baselines/diff.py — usage: ./diff.py baseline.json feature.json
import json, sys
from pathlib import Path

def load(p):
    d = json.load(open(p))
    s = d["stats"]
    eval_block = list(s["evals"].values())[0]
    return {
        "mean_reward": eval_block["metrics"][0]["mean"],
        "n_completed": s["n_completed_trials"],
        "n_errored":   s["n_errored_trials"],
        "label":       Path(p).stem,
    }

def per_task(p):
    out = {}
    for d in Path(p).parent.glob(f"{Path(p).parent.name}/*__*"):
        # Adjust to point at the actual job dir layout.
        ...
    return out

a = load(sys.argv[1])
b = load(sys.argv[2])
print(f"{'metric':<20} {a['label']:>14} {b['label']:>14}  delta")
print(f"{'mean_reward':<20} {a['mean_reward']:>14.3f} {b['mean_reward']:>14.3f}  {b['mean_reward']-a['mean_reward']:+.3f}")
print(f"{'n_errored':<20} {a['n_errored']:>14} {b['n_errored']:>14}  {b['n_errored']-a['n_errored']:+d}")
```

Wire it up the first time you need it. After that, every feature gets two `bench/baselines/*.json` files referenced in the commit message.

## What counts as "moved the right way"

| Metric | Good | Suspicious | Bad |
|---|---|---|---|
| Mean reward | +0.03 or more on the 11-task scoreboard | ±0.01 (noise) | regression |
| Per-task pass count | no task regresses | one task swings -1 | any task drops to 0/3 from ≥2/3 |
| Wall clock | within ±15% | +30–50% | +2× |
| Cost ($) | within ±10% | +20% | +50% |

A 0.01 delta on 33 trials is noise. Use `--n-attempts 5` if you need a tighter estimate.

## Common rationalizations

| Excuse | Reality |
|---|---|
| "It's just a prompt tweak, no need to benchmark" | Every prompt tweak in this session moved bench numbers ±10% in *both* directions. You can't predict the sign. Measure. |
| "I'll benchmark later when there's a slow moment" | Later = never. A baseline run is ~30 min. Do it before you start coding. |
| "It only affects the contract feature, not other tasks" | Cross-cutting changes (tool list, system prompt, plan tracker) touch every task. |
| "I'll just run one task to spot-check" | One run, one task, n=1 — that's the failure mode of this whole session. Run the full 11. |
| "The baseline from last week is fine" | Baselines drift with model swaps, llama-server upgrades, harbor bumps. Always re-baseline from the same commit. |
| "It's obviously an improvement — look at the agent log" | One agent log is a single sample. The model is high-variance at IQ2. Use the verifier-pass numbers. |
| "Measurement adds friction to iteration" | Iteration without measurement is sliding sideways. The friction is the point. |

## Red flags

You're about to ship a feature without benchmark evidence if:

- You're about to type `git commit` and there's no `bench/baselines/*.json` file paired with this branch.
- You haven't run terminal-bench-2 in the last hour but the diff is non-trivial.
- The PR description says "should improve X" without a number.
- You're tuning a knob (max_tokens, thinking_budget, dedup window) and have no baseline to compare against.
- The change is "to fix a single failure I saw" — without checking it doesn't break the other 10.

All of these mean: **stop, run the baseline, run the feature branch, then come back.**

## Real-world signal

This skill exists because of a session where the contract feature got built across ~10 commits and 4 rounds of prompt tuning. Each variant felt better than the last. None of them was measured against the pre-contract baseline. By the time the user asked "is this actually helping?", the answer was *"we don't know, we never compared."* That's the failure mode. This skill makes it cost more to skip the measurement than to do it.
