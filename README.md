# itsy

A coding agent for small local LLMs (8B–35B parameters). For when you've got a
model running locally and want it to actually edit files without it spinning
itself into a loop.

![](docs/itsy-tui.png)

<sub>Composed from actual run logs (a real `cobol-modernization` solve on Qwen3.6-35B IQ2_XXS); the source ANSI lives at [`docs/itsy-tui.ansi`](docs/itsy-tui.ansi) and is re-rendered via [`docs/regen-screenshot.sh`](docs/regen-screenshot.sh).</sub>

The core trade-off is tokens and speed for reliability. itsy is slower and
chattier than a thin wrapper. The payoff is that it actually finishes things.
Local models at 4-bit quant drift, repeat themselves, forget what file they
were editing — itsy burns the extra tokens on guardrails so that stays
contained. The tool-call parser accepts JSON, XML-ish, and code-fence wrappers.
Edits are search-and-replace patches, not full rewrites. Multi-step work
decomposes into a small TODO list so the model has somewhere to look when it
loses the thread. There's a SQLite + FTS5 code graph and per-project memory so
it doesn't reintroduce itself every turn.

Works well with Qwen3.5/3.6 and Gemma4 at IQ2–Q4. Talks to anything
OpenAI-compatible: LM Studio, Ollama, vLLM, llama-server.

## Install

You need Rust 1.85+ (edition 2024) and a running OpenAI-compatible
endpoint. Everything else is bundled.

```bash
git clone https://github.com/Doorman11991/itsy
cd itsy
cargo build --release
```

The binary lands at `target/release/itsy`. Copy it onto your `$PATH`
or invoke it from the repo. There's no `cargo install` recipe yet
because the build picks up native-feature flags from `Cargo.toml`
that `cargo install` doesn't propagate cleanly.

If you're running against bench images or older base distros that
ship a too-old glibc, build statically against musl:

```bash
rustup target add x86_64-unknown-linux-musl
apt-get install -y musl-tools
cargo build --release --target x86_64-unknown-linux-musl
```

## First run

```bash
itsy
```

On the very first launch (no `~/.config/itsy/config.toml`) the wizard
asks for provider, endpoint, model name, and the safeguards worth
turning on for your quant tier. It probes `/v1/models` for the real
context window so you don't have to guess. Re-run the wizard any time
with `itsy --init`.

After that, every launch drops straight into the REPL:

* `itsy`                — interactive fullscreen REPL (default)
* `itsy --classic`      — line-based REPL, useful over SSH or in
                          `docker exec` where the alternate screen
                          breaks
* `itsy -p "fix it"`    — one-shot, prints the answer and exits
* `itsy --mcp`          — talk to itsy from another tool over MCP

Slash commands inside the REPL: `/help`, `/model`, `/endpoint`,
`/memory`, `/plan`, `/diff`, `/git`, `/checkpoint`, `/rollback`,
`/sessions`, `/share`, `/quit`. Anything tied to a feature that's
disabled in your config is a no-op.

## Configure

Everything lives in `~/.config/itsy/config.toml`. The wizard writes a
sensible default; you usually only touch this file to swap models or
flip off a feature flag that's eating your context.

```toml
version = "2"

[model]
provider = "openai"
name = "Qwen3-Coder-30B-A3B-Instruct"
base_url = "http://localhost:1234/v1"
timeout = 300

[context]
detected_window = 32768
max_budget_pct = 70

[tools]
bash_timeout = 30
tool_routing = "direct"   # or "two_stage" for tiny-context models
shell_persist = true

[limits]
max_tool_calls = 50
max_tool_calls_per_turn = 250
max_output_tokens = 0     # 0 = auto (thinking_budget + 4k headroom)

[security]
allow_outside_paths = true   # /data, /tmp ok; sensitive paths still blocked

[features]
plan = true
snapshot = true
write_guard = true
clarifier = true
semantic_merge = true
error_diagnosis = true
context_retrieval = true

[tui]
auto_approve = false
classic = false
theme = "dark"
```

Every field can also be overridden by a CLI flag — see `itsy --help`.
For the long tail of less common knobs (dedup similarity, code-graph
DB path, snapshot dir, etc.) use `--set key=value`, e.g.
`--set dedup.similarity=0.85` or `--set code_graph.disable=true`.

itsy no longer reads `ITSY_*` env vars; everything is config + CLI.
The standard cloud-provider keys (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`) are still consulted when
cloud escalation is enabled.

The `version` field is honoured on load and older files get migrated
forward automatically. Don't remove it.

## Benchmark

Scored against [terminal-bench-2](https://github.com/harbor-framework/terminal-bench-2) — 11 tasks,
Qwen3.6-35B IQ2_XXS, n-concurrent=1. Compared against
[smallcode](https://github.com/smallcode-ai/smallcode) (JS agent, same model, same endpoint).

| Task | smallcode 5× | itsy (current) |
|---|---|---|
| fix-git | 80% | **100%** |
| multi-source-data-merger | 100% | **100%** |
| git-leak-recovery | 100% | **100%** |
| pypi-server | 100% | **100%** |
| kv-store-grpc | 40% | **100%** |
| prove-plus-comm | 60% | **100%** |
| cobol-modernization | 80% | 67% |
| regex-log | 0% | **67%** |
| overfull-hbox | 0% | **33%** |
| break-filter-js-from-html | 0% | 0% |
| filter-js-from-html | 0% | 0% |
| **Overall** | **50.9%** | **69.7%** |

Run pinned at `.claude/skills/benchmark-driven-development/baselines/scoreboard-3x-20260525-streaming/`
(3 attempts per task, Qwen3.6-35B IQ2_XXS, Gemma4 26B second-opinion enabled).

The two tasks both agents score 0% on (break-filter-js-from-html and
filter-js-from-html) are model-capability failures at IQ2 quant — neither
agent solves them regardless of framework.

## License

MIT — see `LICENSE`.
