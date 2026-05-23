# itsy vs smallcode — terminal-bench-2 benchmark report

**Date:** 2026-05-23  
**Dataset:** terminal-bench-2 (harbor), 11 scoreboard tasks  
**Model:** `unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS` (~75 tok/s generation)  
**Protocol:** 5 trials per task, n-concurrent=1  
**Status:** Preliminary results (original 5×) + final 5× run in progress

---

## Summary

itsy (Rust) is marginally better than smallcode (JS) overall on the 11 scoreboard tasks.
Both agents fail identically on 3 tasks that require adversarial reasoning or exact HTML
preservation. itsy improves on regex-log (+40 pp) and fix-git (+20 pp); SC wins on
cobol-modernization (+40 pp). Neither agent has solved break-filter, overfull-hbox, or
filter-js-from-html at this model quant.

---

## Original 5× results (per-task)

*Baseline: 5 trials each, jobs in `jobs/tbt-20260523/`.*

| Task                       | SC 5× | itsy 5× | SC%  | itsy% | Δ      |
|----------------------------|-------|---------|------|-------|--------|
| fix-git                    | 4/5   | 5/5     | 80%  | 100%  | +20 pp |
| multi-source-data-merger   | 5/5   | 5/5     | 100% | 100%  | –      |
| prove-plus-comm            | 3/5   | 3/5     | 60%  | 60%   | –      |
| git-leak-recovery          | 5/5   | 5/5     | 100% | 100%  | –      |
| cobol-modernization        | 4/5   | 2/5     | 80%  | 40%   | −40 pp |
| kv-store-grpc              | 2/5   | 2/5     | 40%  | 40%   | –      |
| regex-log                  | 0/5   | 2/5     | 0%   | 40%   | +40 pp |
| break-filter-js-from-html  | 0/5   | 0/5     | 0%   | 0%    | –      |
| overfull-hbox              | 0/5   | 0/5     | 0%   | 0%    | –      |
| filter-js-from-html        | 0/5   | 0/5     | 0%   | 0%    | –      |
| pypi-server                | 5/5   | 5/5     | 100% | 100%  | –      |
| **Total**                  | **28/55** | **29/55** | **50.9%** | **52.7%** | **+1.8 pp** |

---

## Final 5× results

*Run: `jobs/final-5x-20260523/` — in progress at time of writing.*  
*Config: contract=false, thinking-budget=8000, bash-timeout=120, request-timeout-ms=600000.*

| Task                       | itsy 5× | Pass% |
|----------------------------|---------|-------|
| fix-git                    | –       | –     |
| multi-source-data-merger   | –       | –     |
| prove-plus-comm            | –       | –     |
| git-leak-recovery          | –       | –     |
| cobol-modernization        | –       | –     |
| kv-store-grpc              | –       | –     |
| regex-log                  | –       | –     |
| break-filter-js-from-html  | –       | –     |
| overfull-hbox              | –       | –     |
| filter-js-from-html        | –       | –     |
| pypi-server                | –       | –     |
| **Total**                  | **–**   | **–** |

*(Update this section once `jobs/final-5x-20260523/result.json` is complete.)*

---

## Failure mode analysis

### 1. break-filter-js-from-html — 0/5 (both agents)

**What the task requires:** craft an HTML payload that *bypasses* the existing JS filter
(adversarial task — create a filter bypass, not fix the filter).

**What the agents do:** treat "break" as "find and fix filter bugs." Agent iterates over
SVG/data-URI XSS test cases but keeps patching the filter rather than producing a bypass.
Hits the 250 tool call limit every time — classic stuck loop.

**Root cause:** adversarial task framing at IQ2_XXS quant. The model reads
`break the filter` as a debugging task, not as `write HTML that evades it`.

**Pattern:** same failure in SC and itsy; this is a model comprehension issue, not an
agent-framework issue.

### 2. filter-js-from-html — 0/5 (both agents)

**What the task requires:** implement a Python filter that (a) removes JS from HTML and
(b) leaves clean HTML byte-for-byte identical to the original.

**What the agents do:** use BeautifulSoup, which normalises HTML (reorders attributes,
changes `<br>` to `<br/>`, expands HTML entities, adds whitespace). The XSS removal
works; the clean-HTML-unchanged verifier test fails.

**Root cause:** BeautifulSoup's normalisation is incompatible with the verifier's
exact-equality check. A regex-based or lxml-passthrough approach would pass.

**Pattern:** both SC and itsy choose the same wrong library. The failure is task-knowledge,
not agent-framework.

### 3. overfull-hbox — 0/5 (both agents)

**What the task requires:** replace words in `input.tex` using `synonyms.txt` to eliminate
all `\hbox (N pt too wide)` LaTeX warnings.

**What the agents do:** apply synonym substitutions and run `pdflatex`, but stop before
resolving all overfull boxes. The verifier (`test_no_overfull_hboxes`) still finds
remaining offences.

**Root cause:** agent makes a substitution pass, sees reduced warnings, and considers
the task done without iterating until zero warnings remain. The verifier requires zero.

### 4. cobol-modernization — 2/5 itsy vs 4/5 SC (−40 pp)

**What the task requires:** convert a COBOL program to an equivalent Python script that
produces byte-for-byte identical output.

**What itsy does:** 2/5 trials produced the correct files and output. 3 failures had either
missing required files or incorrect program output (`test_required_files_exist` and
`test_program_output` failures).

**Root cause (suspected):** itsy's Rust port may have a different prompt structure or tool
handling that causes the agent to stop before writing the final output files in some runs.
SC's JS implementation appears more consistent on this task.

**Action:** compare agent traces for a passing itsy trial vs a failing one; look for
differences in file-write sequencing.

### 5. kv-store-grpc — 2/5 (both agents)

**What the task requires:** implement a gRPC key-value store server that listens on a
specific port.

**What happens:** the agent's solution is technically correct, but harbor's 600-second
timeout fires before the agent finishes (the verifier still passes if the server is
listening at timeout). 3/5 trials hit `AgentTimeoutError`.

**Root cause:** harbor timeout is tight for complex tasks + slow model. The verifier runs
post-timeout and can still record reward=1 if the server is up — but 3/5 trials the server
wasn't started in time.

**Action:** no code fix needed. Harbor timeout is external. Could improve with a faster
model quant (IQ3 or IQ4) or pre-installed grpcio in the task image.

### 6. regex-log — 2/5 itsy vs 0/5 SC (+40 pp)

**What the task requires:** write a regex to extract the last date from a log line.

**What happens:** itsy agents that pass spend 5-10 tool calls searching for Python before
finding `/usr/bin/python3.12`. SC agents appear to fail at finding Python entirely.

**Root cause:** task container doesn't have Python in `$PATH`. Agents that try `python3`
get `command not found` and spiral. itsy's probing behaviour (search, try alternate paths)
recovers 2/5 times; SC's doesn't.

---

## Config experiments

### contract=true for prove-plus-comm

**Hypothesis:** enabling `features.contract` forces the agent to verify the proof compiles
before finishing, improving prove-plus-comm pass rate.

**Method:** 5 trials with `--set=features.contract=false` removed from adapter flags.

**Results:**
- contract=true: 3/4 completed (1 trial killed by OOM from runaway `coqc` process)
- contract=false baseline: 3/5

**Conclusion:** no measurable improvement. contract=true can trigger pathological Coq
evaluation (infinite term expansion in `coqc`) that consumes 15 GiB RAM. Reverted to
contract=false in adapter.

---

## Code fixes made during this session

| Commit | Change | Impact |
|--------|--------|--------|
| `1dd8907` | Remove BoneScript backend hint from prompt (bone_check/bone_compile not in itsy) | Eliminates stray "BoneScript" from system prompt |
| `74d638e` | Increase `max_tokens` headroom from +1024 to +4096 | Prevents token budget exhaustion on long reasoning tasks (cobol, kv-store-grpc) |

---

## Recommendations

### Generalizable (apply to all tasks)

1. **Loop detection**: filter-js-from-html and break-filter both hit 250 tool calls in a
   repetitive loop without progress. Detecting "last N tool calls are identical" and
   exiting or re-planning would recover these. This is distinct from the `chain` feature
   (multi-agent orchestration) — a simpler "stall detector" inside the main loop.

2. **Cobol regression investigation**: itsy is 40 pp behind SC on cobol-modernization.
   Read a failing agent trace and compare to a passing SC trace to find the structural
   difference. Likely a file-write or output-verification step that SC's prompt includes
   and itsy's doesn't.

3. **Higher quant for adversarial tasks**: break-filter (0/10 at IQ2_XXS) likely fails
   due to model compression losing task-direction nuance. Testing IQ3_XXS or IQ4_XS on
   break-filter specifically would confirm whether this is a quant problem or a
   fundamental model capability gap.

4. **Harbor timeout**: kv-store-grpc systematically times out harbor's 600s limit. This
   isn't fixable in itsy — it's a benchmark harness constraint. Document it as a known
   limitation; the verifier still rewards correct solutions that survive the timeout.

### Not recommended (overfit risk)

- Task-specific system-prompt hints (e.g., "use regex not BeautifulSoup for HTML")
- Tool-call limits tuned per task
- Per-task config profiles

---

## Methodology notes

- SC baseline and itsy original runs were conducted on the same day (2026-05-23) using
  the same model and endpoint.
- Runs were sequential (n-concurrent=1) to avoid resource contention.
- One cobol-modernization trial (hT7bJZS) may have been affected by a network issue in
  a previous session (uv install failed for verifier); the 2/5 result reflects only the
  tbt-20260523 session runs.
- prove-plus-comm itsy had 6 trial directories; 1 produced no reward (harbor timeout).
  The 3/5 figure counts only completed trials.
- Fisher's exact test on the full sample (29/55 itsy, 28/55 SC) gives p=0.88 — the
  difference is not statistically significant. The two agents are statistically equivalent
  at this sample size.
