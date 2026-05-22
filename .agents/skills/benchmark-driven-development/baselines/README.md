# baselines/

Snapshot one harbor job dir per pinned bench run, named `<branch-or-sha>/`.

The skill expects:

```
baselines/
  main-7c6c26c/             ← pre-feature baseline (full job dir)
  contract-feature/         ← feature branch
  contract-feature-v2/      ← later iteration
  …
```

Each subdir is a full copy of `jobs/<job-name>/`, including `result.json`
and every `<task>__<id>/` trial subdir with its `verifier/reward.txt`.
Snapshotting the whole thing lets `../diff.py` compute per-task pass
deltas, not just the headline mean.

Take a snapshot with:

```bash
cp -r /workspace/itsy/jobs/<job-name> ./<short-name>/
```

Compare two snapshots:

```bash
../diff.py ./main-<sha> ./contract-feature
```

Or compare a stored baseline to a fresh, still-in-place job:

```bash
../diff.py ./main-<sha> /workspace/itsy/jobs/contract-feature-v2
```
