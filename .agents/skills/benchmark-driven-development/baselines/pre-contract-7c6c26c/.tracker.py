#!/usr/bin/env python3
"""Classify a single failed trial and append a markdown entry to FAILURES.md.

Called with the trial directory path. Idempotent: if the trial already has
an entry (matched by trial_id), it's skipped.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")


def strip(s: str) -> str:
    return ANSI.sub("", s)


def classify(agent_log: str, verifier_stdout: str, exception: str) -> str:
    log = strip(agent_log)
    last = log[-3000:]
    if "panicked at" in log or "thread 'main'" in log and "panicked" in log:
        return "panic"
    if "Reached tool call limit" in log:
        return "tool-limit"
    if "Model returned empty responses" in log:
        return "empty-response"
    if "AgentTimeoutError" in exception:
        return "agent-timeout"
    if "Stuck calling" in last or "Aborting this turn" in last:
        return "stuck-loop"
    # missing-output if the verifier says "does not exist"
    if "does not exist" in verifier_stdout or "No such file" in verifier_stdout:
        return "no-output"
    if "FAILED" in verifier_stdout or "AssertionError" in verifier_stdout:
        return "verifier-correctness"
    return "unknown"


def tool_call_count(agent_log: str) -> int:
    return len(re.findall(r"⚙", agent_log))


def trial_id_from_dir(d: Path) -> str:
    return d.name


def task_name_from_dir(d: Path) -> str:
    return re.sub(r"__[A-Za-z0-9]+$", "", d.name)


def first_verifier_failure(verifier_stdout: str) -> str:
    # Pull lines after the first FAILED / AssertionError marker.
    lines = []
    capture = 0
    for line in verifier_stdout.splitlines():
        if capture > 0:
            lines.append(line)
            capture -= 1
        elif re.search(r"FAILED|AssertionError|Traceback", line):
            lines.append(line)
            capture = 4
        if len(lines) >= 12:
            break
    return "\n".join(lines).strip()


def last_meaningful_lines(agent_log: str, n: int = 8) -> str:
    out = []
    for line in reversed(strip(agent_log).splitlines()):
        s = line.strip()
        if not s:
            continue
        out.append(line.rstrip())
        if len(out) >= n:
            break
    return "\n".join(reversed(out))


def main():
    if len(sys.argv) != 3:
        print("usage: tracker.py <trial-dir> <FAILURES.md>", file=sys.stderr)
        sys.exit(2)
    trial = Path(sys.argv[1])
    failures_md = Path(sys.argv[2])
    if not trial.is_dir():
        sys.exit(0)
    reward_path = trial / "verifier" / "reward.txt"
    if not reward_path.exists():
        sys.exit(0)
    reward = reward_path.read_text().strip()
    if reward == "1":
        sys.exit(0)  # success — nothing to log
    trial_id = trial_id_from_dir(trial)
    # Skip if already logged.
    existing = failures_md.read_text() if failures_md.exists() else ""
    if f"### `{trial_id}`" in existing:
        sys.exit(0)
    agent_log = ""
    al = trial / "agent" / "itsy.txt"
    if al.exists():
        agent_log = al.read_text(errors="replace")
    verifier_stdout = ""
    vs = trial / "verifier" / "test-stdout.txt"
    if vs.exists():
        verifier_stdout = vs.read_text(errors="replace")
    exception = ""
    exf = trial / "exception.txt"
    if exf.exists():
        exception = exf.read_text(errors="replace")[:1000]
    pattern = classify(agent_log, verifier_stdout, exception)
    n_tools = tool_call_count(agent_log)
    task = task_name_from_dir(trial)
    block = []
    block.append(f"\n### `{trial_id}`")
    block.append(f"- **task**: {task}")
    block.append(f"- **reward**: {reward}")
    block.append(f"- **pattern**: `{pattern}`")
    block.append(f"- **tool_calls**: {n_tools}")
    if exception:
        head = exception.splitlines()[0] if exception.strip() else ""
        block.append(f"- **exception**: `{head[:200]}`")
    vf = first_verifier_failure(verifier_stdout)
    if vf:
        block.append("- **verifier failure**:")
        block.append("```")
        block.append(vf[:800])
        block.append("```")
    tail = last_meaningful_lines(agent_log)
    if tail:
        block.append("- **agent log tail**:")
        block.append("```")
        block.append(tail[-1200:])
        block.append("```")
    with failures_md.open("a") as f:
        f.write("\n".join(block) + "\n")
    print(f"[failure-logged] {trial_id} → {pattern}")


if __name__ == "__main__":
    main()
