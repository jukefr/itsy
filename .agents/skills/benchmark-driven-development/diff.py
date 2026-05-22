#!/usr/bin/env python3
"""Compare two terminal-bench-2 job outputs side by side.

Usage:
    diff.py <baseline-job-dir> <feature-job-dir>

A job dir is whatever `harbor run --jobs-dir <X> --job-name <Y>` produces,
i.e. `<X>/<Y>/` containing `result.json` plus one subdir per trial. The
script reads:

    result.json            for mean reward + trial counts
    <task>__<id>/verifier/reward.txt   for per-task pass counts
    <task>__<id>/trial.log             for wall-clock (best-effort)

Output: a tabular diff showing reward delta, per-task pass-count delta,
trials erred, and wall-clock delta. Exit code is 0 on improvement,
1 on regression, 2 on insufficient data.

Keep this script narrow — it lives next to its skill on purpose.
Don't grow it into a general-purpose CLI.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from collections import defaultdict


def load_job(job_dir: Path) -> dict:
    """Read everything we need out of a single job dir."""
    result_path = job_dir / "result.json"
    if not result_path.exists():
        sys.exit(f"error: {result_path} does not exist")
    raw = json.loads(result_path.read_text())
    stats = raw.get("stats", {})
    evals = stats.get("evals", {})
    if not evals:
        sys.exit(f"error: {result_path} has no evals block")
    eval_block = next(iter(evals.values()))
    mean = eval_block.get("metrics", [{}])[0].get("mean")

    per_task: dict[str, dict] = defaultdict(
        lambda: {"passes": 0, "attempts": 0, "rewards": []}
    )
    total_wall = 0.0
    n_wall = 0
    for trial in job_dir.glob("*__*"):
        if not trial.is_dir():
            continue
        m = re.match(r"^(.+)__[A-Za-z0-9]+$", trial.name)
        if not m:
            continue
        task = m.group(1)
        reward_file = trial / "verifier" / "reward.txt"
        if not reward_file.exists():
            continue
        try:
            r = float(reward_file.read_text().strip())
        except ValueError:
            continue
        per_task[task]["attempts"] += 1
        per_task[task]["rewards"].append(r)
        if r == 1.0:
            per_task[task]["passes"] += 1

        # Wall-clock from trial.log if available.
        log = trial / "trial.log"
        if log.exists():
            for line in log.read_text(errors="ignore").splitlines():
                m2 = re.search(r"elapsed[:= ]+(\d+(?:\.\d+)?)\s*s", line)
                if m2:
                    total_wall += float(m2.group(1))
                    n_wall += 1
                    break

    return {
        "label": job_dir.name,
        "path": str(job_dir),
        "mean_reward": mean,
        "n_total": raw.get("n_total_trials"),
        "n_completed": stats.get("n_completed_trials"),
        "n_errored": stats.get("n_errored_trials"),
        "per_task": dict(per_task),
        "wall_s_per_trial": (total_wall / n_wall) if n_wall else None,
        "n_with_wallclock": n_wall,
    }


def fmt(x, w=8, fmt_spec=".3f"):
    if x is None:
        return f"{'—':>{w}}"
    if isinstance(x, float):
        return f"{x:>{w}{fmt_spec}}"
    return f"{x:>{w}}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("baseline", type=Path, help="baseline job dir")
    ap.add_argument("feature", type=Path, help="feature job dir")
    ap.add_argument("--threshold", type=float, default=0.02,
                    help="reward delta required to count as improvement (default 0.02)")
    args = ap.parse_args()

    a = load_job(args.baseline)
    b = load_job(args.feature)

    print()
    print(f"{'metric':<22} {a['label'][:18]:>18} {b['label'][:18]:>18}   delta")
    print("─" * 76)
    delta_reward = (b["mean_reward"] or 0) - (a["mean_reward"] or 0)
    print(f"{'mean_reward':<22} {fmt(a['mean_reward'])         :>18} {fmt(b['mean_reward'])         :>18}   {delta_reward:+.3f}")
    print(f"{'completed':<22} {fmt(a['n_completed'])          :>18} {fmt(b['n_completed'])          :>18}   {(b['n_completed'] or 0)-(a['n_completed'] or 0):+d}")
    print(f"{'errored':<22} {fmt(a['n_errored'])             :>18} {fmt(b['n_errored'])             :>18}   {(b['n_errored'] or 0)-(a['n_errored'] or 0):+d}")
    if a["wall_s_per_trial"] is not None or b["wall_s_per_trial"] is not None:
        print(f"{'avg wall s / trial':<22} {fmt(a['wall_s_per_trial']):>18} {fmt(b['wall_s_per_trial']):>18}   ", end="")
        if a["wall_s_per_trial"] and b["wall_s_per_trial"]:
            print(f"{(b['wall_s_per_trial']-a['wall_s_per_trial'])/a['wall_s_per_trial']*100:+.1f}%")
        else:
            print("(missing)")

    # Per-task pass diff
    tasks = sorted(set(a["per_task"]) | set(b["per_task"]))
    if tasks:
        print()
        print(f"{'task':<32} {'baseline':>10} {'feature':>10}   delta")
        print("─" * 76)
        regressed = []
        improved = []
        for t in tasks:
            ap_ = a["per_task"].get(t, {"passes": 0, "attempts": 0})
            bp_ = b["per_task"].get(t, {"passes": 0, "attempts": 0})
            ap_s = f"{ap_['passes']}/{ap_['attempts']}"
            bp_s = f"{bp_['passes']}/{bp_['attempts']}"
            diff = bp_["passes"] - ap_["passes"]
            mark = "↑" if diff > 0 else ("↓" if diff < 0 else " ")
            print(f"{t:<32} {ap_s:>10} {bp_s:>10}   {mark} {diff:+d}")
            if diff < 0:
                regressed.append(t)
            elif diff > 0:
                improved.append(t)

        print()
        if regressed:
            print(f"REGRESSIONS: {', '.join(regressed)}")
        if improved:
            print(f"IMPROVEMENTS: {', '.join(improved)}")

    print()
    print("─" * 76)
    if delta_reward >= args.threshold:
        print(f"VERDICT: improvement ({delta_reward:+.3f} ≥ {args.threshold})")
        return 0
    if delta_reward <= -args.threshold:
        print(f"VERDICT: regression ({delta_reward:+.3f} ≤ -{args.threshold}) — revert or gate")
        return 1
    print(f"VERDICT: noise ({delta_reward:+.3f} within ±{args.threshold}) — need more attempts")
    return 2


if __name__ == "__main__":
    sys.exit(main())
