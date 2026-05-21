# AGENTS.md

Guidance for AI coding agents working on this repository.

## Project shape

- Rust workspace, single crate (`crates/itsy`), edition 2024,
  rust-version 1.85.
- Two binaries: `itsy` (the agent) and `itsy-init` (config wizard).
- One library: `itsy` (everything reusable; both bins consume it).

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

- No `unsafe` outside `config::load_dotenv` (single-threaded startup
  invariant — see the SAFETY comment there).
- Errors: `anyhow::Result` at module boundaries, `thiserror` for typed
  error enums that callers branch on (see `compiled::cognition::budget`).
- Concurrency: `tokio` for I/O, `parking_lot::Mutex` for short non-async
  critical sections, `Arc<Mutex<_>>` for shared state. Never hold a
  `parking_lot` guard across `.await` — take the value out, await, then
  put it back (the `mcp_bridge` and `tools_impl::mcp_client` write paths
  show the pattern).
- Logging: stderr only. The agent owns stdout for user-facing output.
- Doc comments (`//!` for modules, `///` for items) explain *why* and
  cross-link sibling modules with `[`backticks`]`.

## Layout cheatsheet

| Concern                       | Module                              |
|-------------------------------|-------------------------------------|
| Config / env / flags          | `config`                            |
| Tool schemas + routing        | `tools`, `compiled::tool_router`,   |
|                               | `compiled::two_stage_router`        |
| Tool dispatch                 | `executor`                          |
| Chat API                      | `model_client`,                     |
|                               | `compiled::providers::openai_compat`|
| Cloud fallback                | `escalation`                        |
| Project memory                | `memory`, `memory::evidence`        |
| Code-graph MCP                | `mcp_bridge`                        |
| Persistent shell, web, dedup  | `tools_impl::*`                     |
| Session state                 | `session::*`                        |
| Slash commands                | `commands`                          |
| Classic + fullscreen UI       | `tui`, `fullscreen`                 |
| Security primitives           | `security`                          |
| Programmatic embedding        | `api`                               |

## Adding a tool

1. Append the JSON schema to `TOOLS` (or `COMPOUND_TOOLS`) in
   `crates/itsy/src/tools.rs`.
2. Add a `match` arm in `crates/itsy/src/executor.rs::execute_tool`.
3. Mention the tool in the relevant category in
   `crates/itsy/src/compiled/tool_router.rs::get_tools_for_category`
   so the 2-stage router can route to it.

## Adding a config knob

1. Add the field to the appropriate sub-struct in `config::Config`.
2. Read the env var with `env_or(...)` or `env_str(...)` in
   `load_config`. The convention is `ITSY_FOO_BAR`.
3. Document it in `README.md` if it's user-facing.

## Don't

- Do not commit a `Cargo.lock` if you bumped a workspace dep without
  intent — `cargo update` is a deliberate operation.
- Do not introduce new top-level files; the repo root holds only
  `README.md`, `AGENTS.md`, `LICENSE`, `.gitignore`, `Cargo.toml`,
  `Cargo.lock`, and the `crates/` directory.
- Do not add `tokio::process` writes from inside a synchronous mutex
  guard — see the concurrency note above.
- Do not use `eprintln!` in hot paths; gate diagnostic output behind
  `ITSY_DEBUG` via `compiled::logger::debug`.

## Versioning

Single source of truth in `Cargo.toml` (`workspace.package.version`).
Bump when shipping, never in passing.
