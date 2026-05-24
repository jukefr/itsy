---
name: terminal-bench-2
description: Use when the user asks to run, profile, or debug itsy against the harbor terminal-bench-2 dataset (e.g. "run the benchmark", "rerun terminal-bench", "test fix-git", "score N iterations against the bench"). Walks through choosing the model, picking a task subset or difficulty tier, setting attempts per task, and launching the harbor harness — points the run at the prebuilt musl binary and the in-tree adapter.
---

# terminal-bench-2

Drives the [harbor terminal-bench-2](https://github.com/harbor-framework/terminal-bench-2)
dataset against the itsy agent. The dataset is 89 docker-image-backed tasks
(software-engineering, security, ML, build/compile, etc.) graded by a per-task
pytest verifier.

This skill captures everything you need to launch a run, what knobs you're
likely to tweak, and how to read the results.

## When to use

- The user says "run the benchmark", "rerun terminal-bench", "score the
  bench", "test on terminal-bench-2", or names a specific task
  (`fix-git`, `cobol-modernization`, etc.).
- You're investigating a regression and want a controlled signal across
  several iterations of one task.
- You want to populate `FAILURES.md` and a progress monitor for a long run.

Skip this skill if the user just wants `cargo build` / `cargo test`. Skip
also if the harbor daemon isn't reachable (see Prerequisites).

## Quickstart (for the impatient)

```bash
cd /workspace/itsy
cargo build --release --target x86_64-unknown-linux-musl
ITSY_BINARY=$PWD/target/x86_64-unknown-linux-musl/release/itsy \
PYTHONPATH=$PWD/.agents/skills/terminal-bench-2 \
uv run --with harbor harbor run \
  --dataset terminal-bench@2.0 \
  --agent-import-path itsy_agent:ItsyAgent \
  --include-task-name fix-git \
  --model unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS \
  --n-attempts 3 \
  --n-concurrent 1 \
  --jobs-dir $PWD/jobs \
  --job-name fix-git-3x \
  2>&1 &

# Attach the live dashboard immediately after:
cargo run --bin itsy-bench -- watch $PWD/jobs/fix-git-3x
```

The five things you'll change between runs:

| Knob | CLI flag |
|---|---|
| Model | `--model` |
| Endpoint | `--ae ITSY_BASE_URL=...` *or* set it in the agent adapter |
| Task subset | `--include-task-name <name>` (repeatable, supports globs) |
| Difficulty / tag | `--include-task-name <glob>` matched against task names |
| Attempts per task | `--n-attempts N` |
| Concurrent trials | `--n-concurrent N` (keep at 1 if `llama-server --parallel 1`) |
| Output dir | `--jobs-dir` + `--job-name` |

## Memory safety — MANDATORY after every task

**After each task completes, before starting the next one:**

```bash
free -h                                          # available should be >8 GiB
docker ps --format '{{.Names}}\t{{.Status}}'    # kill anything stale (up >30 min that isn't the current trial)
```

Kill any container that has been running for more than 30 minutes and is NOT the current active trial:

```bash
docker ps --format '{{.Names}}\t{{.RunningFor}}' | awk '$2+0 > 30 {print $1}' | xargs -r docker rm -f
```

If available RAM drops below 4 GiB, **stop the job immediately**, identify and kill the memory hog, confirm the system recovers before continuing. Do NOT keep launching trials into a swapping system — it hard-freezes the host.

**Root cause of the 2026-05-23 incident:** Three harbor jobs run in background (fix-git-itsy-5x, break-filter-itsy-5x, cobol-itsy-5x-v2) left their Docker containers running after completing because the harbor process was killed by context compression. Each container held ~10 GiB. Combined with the running job and llama-server, the system OOM'd and required a hard reboot.

## Prerequisites

Verify these before launching. The skill should NOT proceed silently if
anything's missing — surface the gap and either install/fix it or ask the
user how to proceed.

**Always run `itsy-bench-net` first.** The bring-up script lives at
`.agents/skills/terminal-bench-2/itsy-bench-net.sh` (and is mirrored to
`/usr/local/bin/itsy-bench-net`). It restarts the rootless docker daemon
with `DOCKERD_ROOTLESS_ROOTLESSKIT_DISABLE_HOST_LOOPBACK=false` and
starts a `socat 8000 → 10.0.2.2:8000` hop so harbor task containers can
reach the host's `llama-server`. **Run it before every harbor invocation
in this skill.** Idempotent — if everything's already healthy it exits
in <1s.

```bash
itsy-bench-net          # idempotent; exits 0 when everything's healthy
```

| Need | Check | Fix |
|---|---|---|
| Rust toolchain + musl target | `rustup target list --installed \| grep musl` | `rustup target add x86_64-unknown-linux-musl && apt-get install -y musl-tools` |
| Built musl binary | `ls target/x86_64-unknown-linux-musl/release/itsy` | `cargo build --release --target x86_64-unknown-linux-musl` |
| `uv` | `which uv` | `curl -LsSf https://astral.sh/uv/install.sh \| sh` |
| Docker daemon reachable | `docker info` | `itsy-bench-net` |
| Docker compose plugin | `docker compose version` | drop binary at `~/.docker/cli-plugins/docker-compose` |
| Child container → LLM | `docker run --rm alpine wget -qO- --timeout=5 http://10.0.2.2:8000/v1/models \| head -c 30` | `itsy-bench-net` |
| llama-server itself | `curl -s http://10.0.2.2:8000/v1/models` from this container | start `llama-server` or point at the right endpoint |

The musl binary matters — bench task images often ship with glibc 2.36ish,
older than the host, so a glibc-linked binary will fail with
`GLIBC_2.39 not found`.

## Configuration walkthrough

Before launching, work through these questions with the user. **Do not
guess defaults** — confirm each one. Quote their answers back so the run
command is auditable.

### 1. Model + endpoint

Ask: *"Which model and endpoint should the run use?"*

The model name has to match what the OpenAI-compatible server reports in
`/v1/models`. The endpoint defaults to whatever's in `config.toml`; for a
benchmark you almost always want to be explicit.

Common locally-loaded models on this box (check `curl $BASE_URL/models`):
- `unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS` (Qwen3.6 35B MoE @ IQ2 — the
  default the existing scoreboard uses)
- `bartowski/FINAL-Bench_Darwin-36B-Opus-GGUF:IQ2_XXS` (Darwin 36B)
- `unsloth/Qwen3.6-35B-A3B-GGUF:IQ3_XXS` / `IQ4_XS`
- `unsloth/Qwen3.5-9B-GGUF:Q4_K_XL`
- `unsloth/gemma-4-26B-A4B-it-GGUF:IQ2_M`

From inside the harbor trial container, the host's loopback is reachable
at `10.0.2.2` (rootless-docker convention) — that's the default the
adapter uses. If the user has bridged differently, ask for the URL.

### 2. Task subset

Ask: *"Run the full 89-task dataset, the 11 scoreboard tasks, a
difficulty tier, or specific tasks?"*

Options the user is likely to pick:

| Choice | Flags |
|---|---|
| Full dataset | omit `--include-task-name` |
| Scoreboard 11 | `-i fix-git -i multi-source-data-merger -i prove-plus-comm -i git-leak-recovery -i cobol-modernization -i kv-store-grpc -i regex-log -i break-filter-js-from-html -i overfull-hbox -i filter-js-from-html -i pypi-server` |
| Single task | `-i <task-name>` |
| Glob | `-i "fix-*"` (quote it) |

Difficulty tiers aren't a first-class harbor concept — they live in each
task's `task.toml` under `[metadata].difficulty`. To filter by tier:

```bash
grep -l 'difficulty = "easy"' ~/.cache/harbor/tasks/*/*/task.toml \
  | sed 's|.*/\([^/]*\)/task.toml|\1|' \
  | sed 's/^/--include-task-name /' \
  | tr '\n' ' '
```

Use that output as harbor args. Available tiers: `easy`, `medium`, `hard`.
**Confirm the matched task count with the user before launching** — `hard`
alone is ~40 tasks at ~3-8 minutes each per attempt.

### 3. Attempts per task

Ask: *"How many attempts per task?"*

| Attempts | When to use |
|---|---|
| 1 | Quick triage — does this task work at all? |
| 3 | Default for a real run (matches scoreboard convention) |
| 5 | High-variance task you want a tighter estimate on |
| 10+ | Statistical signal — only when the run is short OR the task is fast |

Flag: `--n-attempts N`. Mean reward and `pass@k` come out automatically.

### 4. Concurrency

Ask: *"What's the llama-server `--parallel` setting?"*

- `parallel=1` (default here) → `--n-concurrent 1`. More concurrency just
  queues at the server and inflates total wallclock with no throughput gain.
- `parallel ≥ 2` → match it with `--n-concurrent`. Each concurrent trial
  spins up its own docker container, so check available RAM/CPU.

### 5. Output destination

Ask: *"Where should the results go?"*

```
jobs-dir/
  job-name/
    config.json
    result.json
    job.log
    <task>__<trial-id>/
      agent/itsy.txt
      verifier/{reward.txt, test-stdout.txt, ctrf.json}
      result.json
```

Suggested naming: `<task-or-subset>-<attempts>x-<dateYYYYMMDD>` (e.g.
`fix-git-3x-20260522`, `scoreboard-3x-20260522`).

### 6. Optional — debug knobs

Things the user might ask for that map to itsy CLI flags or `--set`:

| Want | Adapter line to add |
|---|---|
| Different tool-call ceiling | `"--max-tool-calls-per-turn=400"` |
| Allow internet | `"--set=security.allow_public_endpoints=true"` |
| Disable code graph | `"--set=code_graph.disable=true"` |
| Force a model profile | `"--profile=qwen3-iq2"` |
| Tighter thinking budget | `"--thinking-budget=2000"` |

Open `itsy_agent.py` and append to the `flags` list. Commit the change so
it's recorded in the run's git state.

## Launching

Always launch in the background and immediately give the user the TUI watch command.

```bash
ITSY_BINARY=/workspace/itsy/target/x86_64-unknown-linux-musl/release/itsy \
PYTHONPATH=/workspace/itsy/.agents/skills/terminal-bench-2 \
uv run --with harbor harbor run \
  --dataset terminal-bench@2.0 \
  --agent-import-path itsy_agent:ItsyAgent \
  --model <model> \
  --n-attempts <N> --n-concurrent <C> \
  --jobs-dir /workspace/itsy/jobs \
  --job-name <job-name> \
  -i <task1> -i <task2> ... \
  2>&1 &
```

**Immediately after launching**, give the user this command to attach the live dashboard:

```bash
cargo run --bin itsy-bench -- watch /workspace/itsy/jobs/<job-name>
```

The TUI shows live agent logs, per-task pass/fail, reward as it lands, and
token spend. It reads directly from the jobs directory — no side-effects,
safe to run at any time.

The `PYTHONPATH` line is mandatory because the skill directory contains a
hyphen, which isn't a valid Python module name segment — pointing
`PYTHONPATH` at the skill dir makes `itsy_agent` importable as a flat
module.

For long runs arm a Monitor on `<jobs-dir>/<job-name>/result.json` so you
get notified when the run completes.

## Tracking failures

The previous run shipped a `.tracker.py` script at
`/workspace/itsy/jobs/full-grid-run1/.tracker.py` that classifies each
failed trial and appends to `FAILURES.md`. Copy it (or re-run it) on a
fresh `jobs-dir` to keep the same workflow:

```bash
cp /workspace/itsy/jobs/full-grid-run1/.tracker.py <jobs-dir>/<job-name>/.tracker.py
echo "# Failure tracker" > <jobs-dir>/<job-name>/FAILURES.md
# then run inside a Monitor:
for d in <jobs-dir>/<job-name>/*__*/; do
  python3 <jobs-dir>/<job-name>/.tracker.py "$d" <jobs-dir>/<job-name>/FAILURES.md
done
```

Patterns the tracker recognises (from worst to least common):
`stuck-loop`, `no-output`, `verifier-correctness`, `panic`,
`empty-response`, `agent-timeout`, `tool-limit`, `unknown`.

## Reading results

```bash
python3 - <<'PY'
import json
d = json.load(open('<jobs-dir>/<job-name>/result.json'))
s = d['stats']
print(f"Completed: {s['n_completed_trials']}/{d['n_total_trials']}")
print(f"Errored:   {s['n_errored_trials']}")
for eval_name, ev in s['evals'].items():
    print(f"{eval_name}: mean={ev['metrics'][0]['mean']:.3f}")
PY
```

Per-task pass/fail and reward distribution:

```bash
for d in <jobs-dir>/<job-name>/*__*/; do
  t=$(basename "$d" | sed 's/__[A-Za-z0-9]*$//')
  r=$(cat "$d/verifier/reward.txt" 2>/dev/null | tr -d '[:space:]')
  printf '%-32s %s\n' "$t" "${r:-?}"
done | sort
```

## Common gotchas

| Symptom | Cause | Fix |
|---|---|---|
| `GLIBC_2.XX not found` | Built with glibc; bench images are older | Use the musl target |
| `unknown flag: --project-name` | Docker compose plugin missing | Install at `~/.docker/cli-plugins/docker-compose` |
| `'AgentContext' object has no attribute 'metrics'` | Old harbor adapter | Already fixed — adapter uses `.metadata` |
| `path resolves outside project root` | Stale itsy without security fix | Pull the binary built from current main |
| `Reached tool call limit` at 32 calls | Pre-fix binary | Rebuild — limit is 250 now |
| Empty model responses | `max_output_tokens` < `thinking_budget` | Adapter already handles this via `--thinking-budget` plumbing into `max_output_tokens` auto |
| All trials error with `RuntimeError: Docker compose command failed` | docker compose plugin not installed | Install per "Prerequisites" |
| 10.0.2.2 not reachable from child container | Rootless docker daemon started with `DISABLE_HOST_LOOPBACK=true`, or no socat hop in this container | Run `itsy-bench-net` — it restarts the daemon with the right env var and brings up socat. Idempotent. |

## Stopping a run cleanly

If you launched in background and the user wants to bail:

```bash
# stop the harbor process
TaskStop <task-id>          # or: ps aux | grep harbor | awk '{print $2}' | xargs -r kill
# tear down any compose stacks that didn't get cleaned up
docker ps --format '{{.Names}}' | xargs -r -I{} docker rm -f {}
```

`result.json` will still show one trial as `running` because nothing wrote
a terminal state — that's expected; the per-trial directory's
`reward.txt` is authoritative.
