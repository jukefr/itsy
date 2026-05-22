//! Centralised runtime settings.
//!
//! Every knob that used to be an `ITSY_*` env var now lives here. The
//! struct is built once at startup by merging:
//!
//!   CLI flags  (highest precedence — set by `clap` in `bin/itsy.rs`)
//! > config.toml (versioned, persisted under `~/.config/itsy/`)
//! > built-in defaults
//!
//! After [`init`] is called, the merged view is stored in a `OnceLock`
//! and every other module reads from [`get`] — there is no longer any
//! direct `std::env::var("ITSY_X")` lookup at runtime.
//!
//! The one exception is `ITSY_HOME` (see [`crate::paths`]) — it has to
//! resolve *before* we can read the config file, so the chicken-and-egg
//! forces us to keep it as an env var. Everything else is config + CLI.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::config::Config;

/// Global runtime settings. Populated once at startup; subsequent reads
/// are lock-free.
static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Merged settings derived from CLI + config.toml + defaults.
///
/// Add new fields here when you need a runtime knob. Do *not* read
/// from `std::env` directly — wire it through CLI / config + this
/// struct instead.
#[derive(Debug, Clone)]
pub struct Settings {
    // ── core ─────────────────────────────────────────────────────
    /// Verbose tool output (was `ITSY_VERBOSE`).
    pub verbose: bool,
    /// Picks a built-in model profile by name (was `ITSY_PROFILE`).
    pub profile: Option<String>,
    /// Print SIGTSTP debug info (was `ITSY_DEBUG_SIGTSTP`). Dev-only.
    pub debug_sigtstp: bool,

    // ── tool budgets ─────────────────────────────────────────────
    /// Hard cap on tool calls per `run()` (was `ITSY_MAX_TOOL_CALLS`).
    pub max_tool_calls: u32,
    /// Hard cap on tool calls per single turn (was
    /// `ITSY_MAX_TOOL_CALLS_PER_TURN`).
    pub max_tool_calls_per_turn: u32,
    /// Per-request chat-completion timeout in ms (was `ITSY_TIMEOUT_MS`).
    pub request_timeout_ms: u64,
    /// Bash command timeout in seconds (mirrors `config.tools.bash_timeout`).
    pub bash_timeout: u32,
    /// Persistent shell (was `ITSY_SHELL_PERSIST`).
    pub shell_persist: bool,
    /// Constrain shell cwd to project root (was `ITSY_SHELL_CONTAIN`).
    pub shell_contain: bool,
    /// Tool routing mode: "direct" / "two_stage" / "auto" (was `ITSY_TOOL_ROUTING`).
    pub tool_routing: String,
    /// Reasoning-format toolkit (was `ITSY_RTK`). Set false to disable.
    pub rtk: bool,

    // ── model ────────────────────────────────────────────────────
    /// Hard cap on max_tokens in chat requests (was `ITSY_MAX_OUTPUT_TOKENS`).
    /// 0 = auto (thinking_budget + 4k headroom).
    pub max_output_tokens: u32,
    /// Reasoning-token budget per turn (was `ITSY_THINKING_BUDGET`).
    /// 0 = use per-task heuristic.
    pub thinking_budget: u32,

    // ── features (mirrors config.features) ───────────────────────
    pub plan: bool,
    pub snapshot: bool,
    pub snapshot_auto_rollback: bool,
    pub write_guard: bool,
    pub bootstrap: bool,
    pub bootstrap_max_chars: usize,
    pub trust_decay: bool,
    pub temp_adapt: bool,
    pub clarifier: bool,
    pub semantic_merge: bool,
    pub error_diagnosis: bool,
    pub validate_edits: bool,
    pub context_retrieval: bool,
    pub reviewer: bool,
    pub chain: bool,

    // ── tui ──────────────────────────────────────────────────────
    pub auto_approve: bool,
    pub tui_classic: bool,

    // ── git ──────────────────────────────────────────────────────
    pub auto_commit: bool,

    // ── security / network ──────────────────────────────────────
    /// Allow read/write tools to touch absolute paths outside the
    /// project root (was `ITSY_ALLOW_OUTSIDE_PATHS`). Sensitive-path
    /// patterns are still blocked.
    pub allow_outside_paths: bool,
    /// Allow `web_fetch` to hit public internet endpoints (was
    /// `ITSY_ALLOW_PUBLIC_ENDPOINTS`).
    pub allow_public_endpoints: bool,
    /// Respect robots.txt for `web_fetch` (was `ITSY_WEB_RESPECT_ROBOTS`).
    pub web_respect_robots: bool,
    /// Enable web_browse / web_fetch tools (was `ITSY_WEB_BROWSE`).
    pub web_browse: bool,
    /// SearxNG instance for web_search (was `ITSY_SEARX_URL`).
    pub searx_url: Option<String>,

    // ── diff tracker (was ITSY_DIFF_*) ──────────────────────────
    pub diff_context: bool,
    pub diff_context_lines: usize,
    pub diff_max_ratio: f64,
    pub diff_ttl_minutes: u64,

    // ── file-tree tool (was ITSY_FILETREE_*) ────────────────────
    pub filetree_max: usize,
    pub filetree_sort_mtime: bool,

    // ── snapshots (was ITSY_SNAPSHOT_DIR) ───────────────────────
    pub snapshot_dir: Option<PathBuf>,

    // ── code graph (was ITSY_CODEGRAPH_*) ───────────────────────
    pub codegraph_db: Option<PathBuf>,
    pub codegraph_disable: bool,

    // ── test runner (was ITSY_TEST_*) ───────────────────────────
    pub test_disable: bool,
    pub test_runner: Option<String>,

    // ── traces (was ITSY_TRACES_DISABLE) ────────────────────────
    pub traces_disable: bool,

    // ── dedup (was ITSY_DEDUP_*) ────────────────────────────────
    pub dedup_enabled: bool,
    pub dedup_window: usize,
    pub dedup_ttl_secs: u64,
    pub dedup_soft: bool,
    pub dedup_similarity: f64,
    pub dedup_noisy_extra: Vec<String>,

    // ── evidence store (was ITSY_EVIDENCE_DISABLE) ──────────────
    pub evidence_disable: bool,

    // ── plugins (was ITSY_PLUGINS, ITSY_PLUGIN_TIMEOUT_SECS) ────
    pub plugins_spec: Option<String>,
    pub plugin_timeout_secs: u64,
}

impl Settings {
    /// Defaults that mirror what the legacy env-var lookups returned
    /// when the var was unset. Anything tuned during the
    /// terminal-bench fixes (e.g. `max_tool_calls_per_turn = 250`,
    /// `allow_outside_paths = true`) lives here.
    pub fn defaults() -> Self {
        Self {
            verbose: false,
            profile: None,
            debug_sigtstp: false,

            max_tool_calls: 50,
            max_tool_calls_per_turn: 250,
            request_timeout_ms: 120_000,
            bash_timeout: 30,
            shell_persist: true,
            shell_contain: false,
            tool_routing: "auto".into(),
            rtk: true,

            max_output_tokens: 0,
            thinking_budget: 0,

            plan: true,
            snapshot: true,
            snapshot_auto_rollback: true,
            write_guard: true,
            bootstrap: true,
            bootstrap_max_chars: 4000,
            trust_decay: true,
            temp_adapt: true,
            clarifier: true,
            semantic_merge: true,
            error_diagnosis: true,
            validate_edits: false,
            context_retrieval: true,
            reviewer: false,
            chain: false,

            auto_approve: false,
            tui_classic: false,

            auto_commit: false,

            allow_outside_paths: true,
            allow_public_endpoints: false,
            web_respect_robots: false,
            web_browse: false,
            searx_url: None,

            diff_context: false,
            diff_context_lines: 3,
            diff_max_ratio: 0.5,
            diff_ttl_minutes: 30,

            filetree_max: 200,
            filetree_sort_mtime: false,

            snapshot_dir: None,

            codegraph_db: None,
            codegraph_disable: false,

            test_disable: false,
            test_runner: None,

            traces_disable: false,

            dedup_enabled: true,
            dedup_window: 5,
            dedup_ttl_secs: 30,
            dedup_soft: false,
            dedup_similarity: 0.92,
            dedup_noisy_extra: Vec::new(),

            evidence_disable: false,

            plugins_spec: None,
            plugin_timeout_secs: 30,
        }
    }

    /// Populate from a parsed [`Config`]. Each field in `Config` (or one
    /// of its sub-structs) corresponds to a Settings field; anything not
    /// represented in the config schema stays at its default.
    pub fn from_config(cfg: &Config) -> Self {
        let mut s = Self::defaults();
        s.bash_timeout = cfg.tools.bash_timeout;
        s.tool_routing = cfg.tools.tool_routing.clone();
        s.shell_persist = cfg.tools.shell_persist;
        s.web_browse = cfg.tools.web_browse;
        s.auto_approve = cfg.tui.auto_approve;
        s.tui_classic = cfg.tui.classic;
        s.auto_commit = cfg.git.auto_commit;
        s.plan = cfg.features.plan;
        s.snapshot = cfg.features.snapshot;
        s.snapshot_auto_rollback = cfg.features.snapshot_auto_rollback;
        s.write_guard = cfg.features.write_guard;
        s.bootstrap = cfg.features.bootstrap;
        s.trust_decay = cfg.features.trust_decay;
        s.temp_adapt = cfg.features.temp_adapt;
        s.thinking_budget = cfg.features.thinking_budget;
        s.clarifier = cfg.features.clarifier;
        s.semantic_merge = cfg.features.semantic_merge;
        s.error_diagnosis = cfg.features.error_diagnosis;
        s.validate_edits = cfg.features.validate_edits;
        s.context_retrieval = cfg.features.context_retrieval;
        s.reviewer = cfg.features.reviewer;
        s.chain = cfg.features.chain;
        s
    }
}

/// Install the merged settings. Call exactly once at startup, before any
/// other module reads from [`get`]. A second call is a no-op (the value
/// in the lock wins) so tests that run multiple times stay deterministic.
pub fn init(s: Settings) {
    let _ = SETTINGS.set(s);
}

/// Read the merged settings. If [`init`] was never called (e.g. unit
/// tests that don't go through the normal startup path), the defaults
/// are used.
pub fn get() -> &'static Settings {
    SETTINGS.get_or_init(Settings::defaults)
}
