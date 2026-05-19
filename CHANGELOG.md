# Changelog

## [0.4.14] - 2026-05-18

### Fixed
- **"Exit code undefined" display bug** — When `execSync` throws without a status code (e.g. EPERM, ENOENT), the error message now correctly shows "Timed out" instead of "Exit code undefined".

## [0.4.13] - 2026-05-18

### Fixed
- **Install no longer requires C++ build tools** — `budget-aware-mcp` (which needs `better-sqlite3` native compilation) moved to `optionalDependencies`. Install succeeds even without Python/gcc/make. SmallCode gracefully falls back to JSON-based memory when SQLite isn't available.
- **Playwright also made optional** — Web browsing (disabled by default anyway) won't block install on systems without Chromium deps.
- **Top-level require crash** — The `require('budget-aware-mcp')` was outside try/catch, crashing on startup if the module failed to install. Now wrapped with graceful fallback.

### Changed
- Updated README with accurate optional requirements for code graph features.

## [0.4.12] - 2026-05-18

### Fixed
- **Startup health check fails on authenticated endpoints** — `checkOllama` now sends `Authorization: Bearer` header when probing `/models`. Previously, remote servers requiring auth (oMLX, OpenRouter, etc.) would fail the startup check even with a valid API key configured.
- **Better error messages** — Startup no longer assumes "LM Studio" for all OpenAI-compatible endpoints. Shows specific hint on 401/403 to set `OPENAI_API_KEY`.

## [0.4.11] - 2026-05-18

### Fixed
- **Critical: API key not sent in requests** — `chatCompletion` now includes `Authorization: Bearer <key>` header when `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `DEEPSEEK_API_KEY` is set. Previously only local (no-auth) endpoints worked for the main agent loop.
- **OpenRouter support** — Added required `HTTP-Referer` and `X-Title` headers when `SMALLCODE_BASE_URL` points to `openrouter.ai`.
- **`/escalation` command crash** — `escalationEngine` was not in scope inside the command handler. Now passed as parameter to `createCommandHandler`.
- **`-v` flag collision** — `-v` was assigned to both `--version` and `--verbose`. Now `-v` is version, `-V` is verbose.
- **VERSION constant mismatch** — Was hardcoded as `0.1.0`, now reads `0.4.10` matching package.json.
- **Auto-compact destroying system messages** — Context compaction now preserves `role: 'system'` messages (skills, plugins) and only evicts user/assistant/tool messages.
- **ACP adapter version string** — Was stuck at `0.2.7`, now matches package version.

### Changed
- Removed dead `handleCommand` function from `bin/smallcode.js` (~110 lines of unreachable code).
- Pinned all dependency versions (removed caret ranges) per project conventions.
- Updated `.env.example` with OpenRouter configuration example.

## [0.2.0] - 2026-05-17

### Added
- **BoneScript Integration (Phase 1 + Phase 2 partial)**
  - `bone_compile` tool: Compile `.bone` files into complete Node.js/TypeScript backends
  - `bone_check` tool: Validate `.bone` files without generating code
  - `.bone` file validation in the improvement loop (auto-fix feedback)
  - Task classifier detects backend/API tasks and triggers BoneScript mode
  - System prompt dynamically injects BoneScript syntax guide when `taskType === 'backend'`
  - `bonescript-compiler` added as dependency (`file:../BoneScript/compiler`)
  - BoneScript quick reference module (`bin/bonescript_guide.js`)
  - Marrowscript source files for bone tools (`src/tools/builtin/bone_compile.ms`, `src/tools/builtin/bone_check.ms`)
  - Governor Marrowscript declaration updated with `backend` task type
  - Verifier updated to validate `.bone` files via `bone_check`

- **Model Escalation Engine**
  - When local model hard fails after decompose, escalate to a stronger model (Claude/OpenAI/DeepSeek)
  - Opt-in: requires API key via env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`) or `[escalation]` config
  - Supports Anthropic Messages API and OpenAI-compatible endpoints
  - Session-limited (default 5 escalations per session)
  - `/escalation` TUI command to view status
  - Marrowscript source: `src/governor/escalation.ms`
  - JS runtime: `bin/escalation.js`

- **Full-Screen TUI Engine**
  - Alternate screen buffer (terminal takeover, like OpenCode/vim)
  - Zero-dependency: raw ANSI escape sequences, no Ink/React/Bun needed
  - Panel layout: chat (scrollable) + optional tool panel (split view) + input + status bar
  - Raw mode input handling: arrow keys, history, PgUp/PgDn scroll, Ctrl+C exit
  - Dark/Light/Minimal themes with 24-bit RGB color support
  - Box drawing characters for borders and dividers
  - Streaming token display for real-time model output
  - Now the **default TUI** — use `--classic` flag for old readline mode
  - Marrowscript declaration: `src/tui/screen.ms`
  - JS runtime: `src/tui/fullscreen.js`

- **Plugin System**
  - Extend SmallCode with custom tools, commands, prompt injections, and hooks
  - Plugin locations: `.smallcode/plugins/` (project) and `~/.config/smallcode/plugins/` (global)
  - Each plugin is a directory with `plugin.json` manifest + JS handler files
  - Plugin tools are auto-injected into the model's tool list
  - Plugin prompts are injected into the system message based on task type
  - `/plugin list` command to show installed plugins
  - Runtime: `src/plugins/loader.js`

- **Skill System**
  - Reusable prompt templates that teach the model specific behaviors
  - Markdown files with YAML frontmatter (name, trigger, keywords)
  - Three trigger modes: `manual` (via /skill use), `auto` (always injected), `match` (keyword-activated)
  - Skills auto-activate when message matches keywords
  - `/skill list` — show all skills
  - `/skill add <name>` — create a new skill
  - `/skill use <name>` — activate for current conversation
  - `/skill remove <name>` — delete a skill
  - Skill locations: `.smallcode/skills/` (project) and `~/.config/smallcode/skills/` (global)
  - Runtime: `src/plugins/skills.js`

### Changed
- `bin/governor.js` — `classifyTask()` now detects backend/API creation tasks, scoped to Node.js/TypeScript only (respects Python/Go/Rust/etc)
- `bin/smallcode.js` — System prompt conditionally includes BoneScript guide; improvement loop now tracks decompose attempts and escalates on 2nd failure
- `bin/commands.js` — Added `/escalation` command
- `smallcode.toml` — Added `[escalation]` config section
- `src/tools/registry.ms` — Registered `bone_compile` and `bone_check` tools
- `src/governor/verifier.ms` — Added `.bone` extension to compile validation pipeline
- `src/governor/governor.marrow` — Added "backend" to task type constraint enum

## [0.1.0] - Initial Release

- Core agent loop with tool calling
- Improvement loop with auto-validation
- Governor with tool scoring and hard fail
- Compound tools for reduced tool call chains
- Memory integration (budget-aware-mcp SQLite+FTS5)
- Code graph MCP integration
- TUI with slash commands
- Model profiles for small LLMs
