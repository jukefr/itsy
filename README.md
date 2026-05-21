# itsy

AI coding agent optimized for small LLMs (8B–35B parameters).

Designed around models running on consumer hardware: budget-managed
context, forgiving multi-format tool-call parsing, TODO-decomposed
planning, search-and-replace patch edits, and opt-in cloud escalation.
No network calls required at the model layer — point it at LM Studio,
Ollama, vLLM, or any OpenAI-compatible endpoint.

## Build

```bash
cargo build --release
```

Binaries land in `target/release/`:

- `itsy`      — the agent (REPL + non-interactive `-p "..."`)
- `itsy-init` — interactive `.env` setup wizard

Requires Rust 1.85+ (edition 2024).

## Configure

```bash
./target/release/itsy-init
# or write a .env yourself:
ITSY_MODEL=your-model-name
ITSY_BASE_URL=http://localhost:1234/v1
# Optional cloud escalation:
# OPENAI_API_KEY=sk-...
# ANTHROPIC_API_KEY=sk-ant-...
# DEEPSEEK_API_KEY=sk-...
```

Config search order: `.env` → `.itsy/.env` → `~/.config/itsy/.env` →
`~/.itsy/.env`. First file wins; existing env vars never overridden.
`itsy.toml` is also honored for backwards-compatible setups.

## Run

```bash
./target/release/itsy                       # interactive REPL
./target/release/itsy -p "fix the build"    # one-shot
./target/release/itsy --print-system-prompt
```

Slash commands: `/help`, `/model [name]`, `/endpoint [url]`, `/stats`,
`/tokens`, `/memory`, `/escalation`, `/clear`, `/quit`.

## Layout

```
Cargo.toml                  workspace root
crates/itsy/                single crate, lib + 2 bins
  src/
    bin/itsy.rs             agent entry point
    bin/itsy_init.rs        config wizard
    lib.rs                  module tree (see lib doc-comment)
    config.rs               env / toml / flag layering
    tools.rs                tool schemas + 2-stage routing
    executor.rs             tool dispatch
    model_client.rs         OpenAI-compatible chat client
    governor.rs             scoring, verification, classifier
    escalation.rs           cloud-model fallback
    memory.rs               typed project memory
    mcp_bridge.rs           code-graph MCP server lifecycle
    tui.rs                  classic line-based renderer
    fullscreen.rs           ratatui alternate-screen renderer
    commands.rs             slash commands
    security.rs             redaction, ANSI strip, path safety
    session/                persistence, undo, snapshots, ...
    tools_impl/             persistent shell, MCP client, web, ...
    model/                  profiles, routing, adaptive params
    compiled/               deterministic router + providers
    plugins/                plugin / skill loaders
    api.rs                  programmatic embedding API
    adapters/               ACP adapter
```

## License

MIT — see `LICENSE`.
