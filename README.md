# itsy

AI coding agent optimized for small LLMs (8B–35B parameters).

Designed around models running on consumer hardware: budget-managed
context, forgiving multi-format tool-call parsing, TODO-decomposed
planning, search-and-replace patch edits, opt-in cloud escalation,
native code graph (tree-sitter + SQLite), and FTS5-backed project
memory. No external runtime dependencies — point it at LM Studio,
Ollama, vLLM, or any OpenAI-compatible endpoint.

## Build

```bash
cargo build --release
```

Single binary lands at `target/release/itsy`. Requires Rust 1.85+
(edition 2024).

## First launch

```bash
./target/release/itsy
```

If `~/.config/itsy/config.toml` doesn't exist, itsy runs an
interactive setup wizard that asks for your provider / endpoint /
model, detects the model's quantization tier and family from its
name, probes the endpoint for the actual context window, and writes
a sensible default config to disk. On subsequent launches the wizard
is skipped. Re-run it any time with `itsy --init`.

## Configure

Canonical config lives at `~/.config/itsy/config.toml`. It's a
versioned TOML file:

```toml
version = "1"

[model]
provider = "openai"
name = "your-model"
base_url = "http://localhost:1234/v1"
timeout = 300

[context]
detected_window = 32768
max_budget_pct = 70

[tools]
bash_timeout = 30
tool_routing = "direct"   # or "two_stage" for small-context models

[tui]
auto_approve = false
theme = "dark"

[git]
auto_commit = false
```

The `version` field is honored on load and the file is migrated
forward as the schema evolves. Don't remove it.

`ITSY_*` env vars override individual fields at runtime; CLI flags
override env vars. Cloud escalation keys come from the standard
provider env vars:

```
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
DEEPSEEK_API_KEY=sk-...
```

Project-local `.env` files are **not** read — runtime state and
config live under `~/.config/itsy/` exclusively.

## Run

```bash
./target/release/itsy                       # interactive fullscreen REPL
./target/release/itsy --classic             # classic line-based REPL
./target/release/itsy -p "fix the build"    # one-shot non-interactive
./target/release/itsy --print-system-prompt
./target/release/itsy --eval <suite>        # offline eval suite
./target/release/itsy --mcp                 # MCP-server mode
./target/release/itsy --init                # re-run the setup wizard
```

Slash commands: `/help`, `/model [name]`, `/endpoint [url]`,
`/stats`, `/tokens`, `/memory`, `/plan`, `/diff`, `/git`,
`/escalation`, `/sessions`, `/trace`, `/skills`, `/plugins`,
`/checkpoint`, `/rollback`, `/share`, `/clear`, `/quit`.

## State layout

All runtime state lives under `~/.config/itsy/`:

```
~/.config/itsy/
  config.toml              global config
  plugins/                 user plugins
  skills/                  user skills
  projects/<id>/           per-project state, keyed by cwd hash
    memory.db              SQLite + FTS5 project memory
    codegraph.db           SQLite + FTS5 symbol index (tree-sitter)
    sessions/              conversation persistence
    traces/                agent execution traces
    snapshots/             auto-rollback checkpoints
    tool_scores.json       governor's per-tool confidence
```

The project id is `<slug>-<hash10>` derived from the canonical
absolute path of your working directory — so each repo gets its own
isolated memory, code graph, and session history.

## License

MIT — see `LICENSE`.
