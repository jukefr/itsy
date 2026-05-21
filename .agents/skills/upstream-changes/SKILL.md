---
name: upstream-changes
description: Use when the user asks to sync, check, port, or integrate upstream changes from the smallcode JS repo into the itsy Rust port — surfaces what changed since the marker in `.agents/skills/upstream-changes/upstream-rev` and walks through each upstream commit asking the user per-change whether to port, skip, or defer.
---

# upstream-changes

itsy is a Rust port of `github.com/Doorman11991/smallcode` (JavaScript). The
upstream still ships new features and bug fixes. This skill walks a coding
agent through reviewing and selectively porting upstream commits.

## When to use

- "sync upstream", "check upstream", "port upstream", "pull upstream", "upstream version X dropped"
- A new upstream tag (`vX.Y.Z`) exists
- Any task that integrates external changes into itsy

## Pre-flight

- `upstream` git remote points at `https://github.com/Doorman11991/smallcode.git`.
  If missing: `git remote add upstream https://github.com/Doorman11991/smallcode.git`
- The marker file lives alongside this skill at
  `$MARKER` — one line, the SHA of the
  last upstream commit that has been integrated.
- Workspace is clean (`git status -sb` shows no changes).

If any pre-flight fails, stop and tell the user — do not guess.

## Workflow

### 1. Fetch and survey

```bash
MARKER=.agents/skills/upstream-changes/upstream-rev
git fetch upstream
LAST=$(cat "$MARKER")
git log --oneline "$LAST..upstream/master"
git diff --shortstat "$LAST..upstream/master"
```

If `LAST..upstream/master` is empty, nothing to port — say so and stop.

### 2. Present a breakdown, one commit at a time, oldest first

For each commit:

1. Show `git show --stat <sha>` (subject, body, files touched).
2. If non-trivial, show `git show <sha>` (full diff).
3. Map each touched JS path to the Rust module it corresponds to (table
   below). If a file has no Rust counterpart, flag it.
4. Ask the user **per commit** using `AskUserQuestion` (or the equivalent
   tool):

| Option  | Meaning |
|---------|---------|
| Port    | Translate this change into itsy now. |
| Skip    | Irrelevant or doesn't apply (Node-only, removed feature, etc.). Record reason in the bump commit. |
| Defer   | Relevant but not now — stop the loop, do NOT bump `$MARKER`. |

Do not batch the question. One ask per upstream commit.

### 3. Port a single commit

1. Apply the semantic equivalent in Rust. Port behavior, not syntax — Rust
   idioms diverge from JS idioms (no try/catch swallowing, explicit
   ownership, `Result<T, E>`).
2. Translate env var names: upstream uses `SMALLCODE_*`, itsy uses `ITSY_*`.
3. Run `cargo build --release`. If it fails, fix before continuing.
4. Stage the Rust changes, bump `$MARKER` to the just-ported SHA, and
   commit together:
   ```bash
   echo <sha> > "$MARKER"
   git add -A
   git commit -m "port: <upstream subject> (<short-sha>)"
   ```
5. Move to the next commit (back to step 2 of section 2).

For **Skip**, still bump `$MARKER` to that SHA in a commit:
```
chore(upstream): skip <short-sha> — <reason>
```
This advances the marker so the commit isn't re-presented next sync.

For **Defer**, leave `$MARKER` where it is and stop.

### 4. JS → Rust path map

| Upstream JS path                                  | itsy Rust module                                  |
|---------------------------------------------------|---------------------------------------------------|
| `bin/smallcode.js`                                | `crates/itsy/src/bin/itsy.rs`                     |
| `bin/init.js`                                     | `crates/itsy/src/bin/itsy_init.rs`                |
| `bin/config.js`                                   | `crates/itsy/src/config.rs`                       |
| `bin/tools.js`                                    | `crates/itsy/src/tools.rs`                        |
| `bin/executor.js`                                 | `crates/itsy/src/executor.rs`                     |
| `bin/model_client.js`                             | `crates/itsy/src/model_client.rs`                 |
| `bin/governor.js`                                 | `crates/itsy/src/governor.rs`                     |
| `bin/escalation.js`                               | `crates/itsy/src/escalation.rs`                   |
| `bin/memory.js`                                   | `crates/itsy/src/memory.rs`                       |
| `bin/mcp_bridge.js`                               | `crates/itsy/src/mcp_bridge.rs`                   |
| `bin/tui.js`                                      | `crates/itsy/src/tui.rs`                          |
| `bin/commands.js`                                 | `crates/itsy/src/commands.rs`                     |
| `src/security/sanitize.js`                        | `crates/itsy/src/security.rs`                     |
| `src/session/*.js`                                | `crates/itsy/src/session/*.rs`                    |
| `src/tools/*.js`                                  | `crates/itsy/src/tools_impl/*.rs`                 |
| `src/tools/builtin/web_browse.js`                 | `crates/itsy/src/tools_impl/web_browse.rs`        |
| `src/model/*.js`                                  | `crates/itsy/src/model/*.rs`                      |
| `src/compiled/cognition/*.js`                     | `crates/itsy/src/compiled/cognition/*.rs`         |
| `src/compiled/features/*.js`                      | `crates/itsy/src/compiled/features/*.rs`          |
| `src/compiled/providers/*.{js,ts}`                | `crates/itsy/src/compiled/providers/*.rs`         |
| `src/compiled/{tool_router,schemas,flows,extensions,logger,metrics}.{js,ts}` | `crates/itsy/src/compiled/*.rs`                   |
| `src/governor/early_stop.js`                      | `crates/itsy/src/governor/early_stop.rs`          |
| `src/memory/evidence.js`                          | `crates/itsy/src/memory/evidence.rs`              |
| `src/knowledge/loader.js`                         | `crates/itsy/src/knowledge.rs`                    |
| `src/plugins/{loader,skills}.js`                  | `crates/itsy/src/plugins/{loader,skills}.rs`      |
| `src/lsp/client.js`                               | `crates/itsy/src/lsp.rs`                          |
| `src/api/index.js`                                | `crates/itsy/src/api.rs`                          |
| `src/adapters/acp.js`                             | `crates/itsy/src/adapters/acp.rs`                 |
| `src/tui/fullscreen.js`                           | `crates/itsy/src/fullscreen.rs`                   |
| `src/tools/two_stage_router.js`                   | `crates/itsy/src/compiled/two_stage_router.rs`    |
| `package.json` / `smallcode.toml`                 | `Cargo.toml` + `crates/itsy/Cargo.toml`           |

When upstream adds a brand-new JS file with no Rust counterpart, ask the
user whether to create a new Rust module or skip.

### 5. Things that are NOT ports (skip with a reason)

- Node build/install machinery: `build.js`, `install.{sh,ps1}`, `package.json` deps, `package-lock.json`, `.npmignore`, `.github/workflows/*.yml`
- MarrowScript source under `marrow/*.ms` — itsy uses the Rust hand-port of the compiled output
- npm release tooling, `CHANGELOG.md` bumps, version bumps that only touch JS
- Node-specific perf hacks (e.g. `execSync` quirks, `child_process` workarounds that have no Rust analog)
- `knowledge/` prose, `bench/`, `extensions/`, `profiles/` — pruned from itsy by design
- Pure i18n additions (e.g. `README_zh-CN.md`)

### 6. Things that DEFINITELY are ports

- Logic changes in `bin/*.js` or `src/{model,session,tools,security,governor,memory,plugins,adapters}/*.js`
- New tool schemas in `bin/tools.js` (also wire into `compiled/tool_router.rs`'s category lists if relevant)
- New executor branches in `bin/executor.js` (add to the `match` arm in `executor.rs::execute_tool`)
- New env-var knobs in `bin/config.js` (translate `SMALLCODE_X` → `ITSY_X`)
- Provider / SSRF / prompt changes under `src/compiled/`
- Security fixes — flag explicitly to the user even if you're sure

## Quick reference

| Step                | Command |
|---------------------|---------|
| Fetch upstream      | `git fetch upstream` |
| List new commits    | `git log --oneline $(cat "$MARKER")..upstream/master` |
| Show one commit     | `git show --stat <sha>` then `git show <sha>` |
| Bump marker         | `echo <sha> > "$MARKER"` |
| Verify build        | `cargo build --release` |
| Commit a port       | `git commit -am "port: <subject> (<short-sha>)"` |
| Commit a skip       | `git commit -am "chore(upstream): skip <short-sha> — <reason>"` |

## Common mistakes

- **Batching ports.** Each upstream commit gets its own port-commit. Granular history makes rollbacks trivial.
- **Forgetting `$MARKER`.** A port without bumping the marker means the next sync re-surfaces the same diff.
- **Bumping the marker first.** Bump only after the port lands and `cargo build` is green.
- **Line-by-line transliteration.** Port behavior, not syntax. JS try/catch ≠ Rust `Result`.
- **Forgetting the env-var rename.** Upstream `SMALLCODE_FOO` always becomes `ITSY_FOO` in itsy.
- **Touching deleted upstream paths.** If upstream modifies `marrow/`, `bench/`, `knowledge/`, etc., skip them — we pruned those by design.
- **Skipping the user check.** Even small commits get the Port / Skip / Defer prompt.

## Red flags — STOP

- "I'll port all N commits at once"
- "This is too small to ask about, I'll just port it"
- "I'll bump `$MARKER` first and port later"
- "`$MARKER` is missing or stale, I'll guess the baseline"
- "Tests / build are red, but I'll fix it in the next commit"

All of these mean: stop, surface the situation to the user, and follow the
workflow.
