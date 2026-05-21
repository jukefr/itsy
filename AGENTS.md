# AGENTS.md

Guidance for AI coding agents working on this repository.

## Project shape

- Rust workspace, single crate (`crates/itsy`), edition 2024,
  rust-version 1.85.
- One binary: `itsy` (the agent + the init wizard, merged). First
  launch with no config auto-runs the wizard; re-run any time with
  `itsy --init`.
- One library: `itsy` (everything reusable; the binary consumes it).

## Build, test, run

```bash
cargo build              # debug
cargo build --release    # release (target/release/itsy)
cargo check              # fastest feedback loop
cargo test               # unit + integration tests
cargo clippy --all-targets
```

When iterating on a single module, prefer `cargo check -p itsy` over
full builds.

## Conventions

- No `unsafe` outside `config::load_dotenv` and `paths` env-var setup
  (single-threaded startup invariant — see the SAFETY comments).
- Errors: `anyhow::Result` at module boundaries, `thiserror` for typed
  error enums callers branch on (see `runtime::cognition::budget`).
- Concurrency: `tokio` for I/O, `parking_lot::Mutex` for short non-async
  critical sections, `Arc<Mutex<_>>` for shared state. Never hold a
  `parking_lot` guard across `.await` — take the value out, await, then
  put it back (`mcp_bridge` and `tools_impl::mcp_client` show the pattern).
- Logging: stderr only. The agent owns stdout for user-facing output.
- Doc comments (`//!` for modules, `///` for items) explain *why* and
  cross-link sibling modules with `[`backticks`]`.

## Path conventions

All runtime state lives under `~/.config/itsy/` (overridable via
`ITSY_HOME` or `XDG_CONFIG_HOME`). Nothing is written into the
project working tree.

Never hardcode `.itsy/` paths anywhere. Route through
[`crate::paths`]:

| Want                     | Use                                       |
|--------------------------|-------------------------------------------|
| config file              | `paths::config_file()`                    |
| user plugins / skills    | `paths::plugins_dir()` / `skills_dir()`   |
| per-project root         | `paths::project_dir(cwd)`                 |
| project memory DB        | `paths::memory_db(cwd)`                   |
| code-graph DB            | `paths::codegraph_db(cwd)`                |
| sessions / traces / snaps| `paths::sessions_dir(cwd)` etc.           |
| tool scores              | `paths::tool_scores(cwd)`                 |

## Adding a tool

1. Append the JSON schema to `TOOLS` (or `COMPOUND_TOOLS`) in
   `tools.rs`.
2. Add a `match` arm in `executor.rs::execute_tool`.
3. List the tool in the appropriate category in
   `runtime::tool_router::get_tools_for_category` so the 2-stage
   router can pick it up on small-context models.

## Adding a config knob

1. Add the field to the appropriate sub-struct in `config::Config`.
2. Add the same field with `#[serde(default)]` to the corresponding
   `Option<…>` in `ConfigFile` so the TOML loader picks it up.
3. Read the env var with `env_or(...)` / `env_str(...)` in
   `load_config`. The convention is `ITSY_FOO_BAR`.
4. Document it in `README.md` if it's user-facing.

If your change is a breaking schema bump:

1. Increment `config::CURRENT_CONFIG_VERSION`.
2. Add a `Migration` fn to `config::MIGRATIONS` keyed by the *old*
   version that mutates the raw `toml::Value` in place.
3. The wizard at first launch and `itsy --init` will pick it up
   automatically.

## Integrating upstream changes

itsy started life as a 1:1 port of `github.com/Doorman11991/smallcode`.
When syncing new upstream work, follow the skill at
`.agents/skills/upstream-changes/SKILL.md` — it tracks the last-ported
SHA in its own `upstream-rev` file and covers the fetch / per-commit
review / port-or-skip workflow plus a JS → Rust path map.

## Repo root

Try to keep the root lean. Current contents:

- `README.md`, `AGENTS.md`, `LICENSE`
- `.gitignore`
- `Cargo.toml`, `Cargo.lock`
- `crates/` — the workspace
- `.agents/` — agent skills (see above)

Add new top-level files only when there's a real reason — and when
you do, add a line here so future agents know it's load-bearing.

## Heads-up

- `cargo update` is a deliberate operation. Don't bump `Cargo.lock`
  in passing.
- No `tokio::process` writes from inside a synchronous mutex guard
  — see the concurrency note above.
- No `eprintln!` in hot paths; gate diagnostic output behind
  `ITSY_DEBUG` via `runtime::logger::debug`.
- No project-local `.env` files — config lives at
  `~/.config/itsy/config.toml`; one-off env overrides go in
  `~/.config/itsy/.env`.

## Versioning

Two version axes:

- Crate version — `Cargo.toml` (`workspace.package.version`). Bump
  when shipping a release.
- Config schema version — `config::CURRENT_CONFIG_VERSION`. Bump
  when the on-disk `config.toml` shape changes, and add a migration.
