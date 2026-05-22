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
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

use crate::config::Config;

/// Global runtime settings. Populated once at startup; slash commands
/// like `/web on` can mutate fields at runtime via [`update`].
static SETTINGS: OnceLock<RwLock<Settings>> = OnceLock::new();

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

    // ── model name + endpoint (mirrors config.model.*) ──────────
    /// Default model name (was `ITSY_MODEL`).
    pub model_name: String,
    /// Endpoint base URL (was `ITSY_BASE_URL`).
    pub base_url: String,
    /// Optional "strong" model name for higher-quality tiers
    /// (was `ITSY_MODEL_STRONG`). Falls back to `model_name` if unset.
    pub model_strong: Option<String>,

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
    pub contract: bool,

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

            model_name: String::new(),
            base_url: "http://localhost:1234/v1".into(),
            model_strong: None,

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
            contract: true,

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
        s.model_name = cfg.model.name.clone();
        s.base_url = cfg.model.base_url.clone();
        s.model_strong = cfg.models.as_ref().map(|m| m.strong.clone());
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
        s.contract = cfg.features.contract;
        s
    }
}

impl Settings {
    /// Apply a dotted-path `--set key=value` override. Returns
    /// `Err(message)` if the path doesn't match a known field or the
    /// value can't be parsed. Used by the CLI's repeatable `--set`
    /// flag — see `Cli::set_overrides`.
    pub fn apply_set_override(&mut self, key: &str, value: &str) -> Result<(), String> {
        // Helpers for parsing.
        fn parse_bool(v: &str) -> Result<bool, String> {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(true),
                "0" | "false" | "no" | "off" => Ok(false),
                _ => Err(format!("expected boolean, got `{v}`")),
            }
        }
        fn parse_u32(v: &str) -> Result<u32, String> {
            v.parse().map_err(|e: std::num::ParseIntError| e.to_string())
        }
        fn parse_u64(v: &str) -> Result<u64, String> {
            v.parse().map_err(|e: std::num::ParseIntError| e.to_string())
        }
        fn parse_usize(v: &str) -> Result<usize, String> {
            v.parse().map_err(|e: std::num::ParseIntError| e.to_string())
        }
        fn parse_f64(v: &str) -> Result<f64, String> {
            v.parse().map_err(|e: std::num::ParseFloatError| e.to_string())
        }

        match key {
            // core
            "verbose" => self.verbose = parse_bool(value)?,
            "profile" => self.profile = Some(value.to_string()),
            "debug_sigtstp" => self.debug_sigtstp = parse_bool(value)?,

            // tool budgets
            "max_tool_calls" => self.max_tool_calls = parse_u32(value)?,
            "max_tool_calls_per_turn" => self.max_tool_calls_per_turn = parse_u32(value)?,
            "request_timeout_ms" => self.request_timeout_ms = parse_u64(value)?,
            "bash_timeout" => self.bash_timeout = parse_u32(value)?,
            "shell_persist" => self.shell_persist = parse_bool(value)?,
            "shell_contain" => self.shell_contain = parse_bool(value)?,
            "tool_routing" => self.tool_routing = value.to_string(),
            "rtk" => self.rtk = parse_bool(value)?,

            // model
            "max_output_tokens" => self.max_output_tokens = parse_u32(value)?,
            "thinking_budget" => self.thinking_budget = parse_u32(value)?,

            // features
            "features.plan" | "plan" => self.plan = parse_bool(value)?,
            "features.snapshot" | "snapshot" => self.snapshot = parse_bool(value)?,
            "features.snapshot_auto_rollback" => self.snapshot_auto_rollback = parse_bool(value)?,
            "features.write_guard" => self.write_guard = parse_bool(value)?,
            "features.bootstrap" | "bootstrap" => self.bootstrap = parse_bool(value)?,
            "features.bootstrap_max_chars" => self.bootstrap_max_chars = parse_usize(value)?,
            "features.trust_decay" => self.trust_decay = parse_bool(value)?,
            "features.temp_adapt" => self.temp_adapt = parse_bool(value)?,
            "features.clarifier" => self.clarifier = parse_bool(value)?,
            "features.semantic_merge" => self.semantic_merge = parse_bool(value)?,
            "features.error_diagnosis" => self.error_diagnosis = parse_bool(value)?,
            "features.validate_edits" => self.validate_edits = parse_bool(value)?,
            "features.context_retrieval" => self.context_retrieval = parse_bool(value)?,
            "features.reviewer" => self.reviewer = parse_bool(value)?,
            "features.chain" => self.chain = parse_bool(value)?,
            "features.contract" | "contract" => self.contract = parse_bool(value)?,

            // tui
            "tui.auto_approve" | "auto_approve" => self.auto_approve = parse_bool(value)?,
            "tui.classic" => self.tui_classic = parse_bool(value)?,

            // git
            "git.auto_commit" | "auto_commit" => self.auto_commit = parse_bool(value)?,

            // security / network
            "security.allow_outside_paths" | "allow_outside_paths" => {
                self.allow_outside_paths = parse_bool(value)?;
            }
            "security.allow_public_endpoints" | "allow_public_endpoints" => {
                self.allow_public_endpoints = parse_bool(value)?;
            }
            "security.web_respect_robots" | "web_respect_robots" => {
                self.web_respect_robots = parse_bool(value)?;
            }
            "security.searx_url" | "searx_url" => self.searx_url = Some(value.to_string()),
            "tools.web_browse" | "web_browse" => self.web_browse = parse_bool(value)?,

            // diff
            "diff.context" => self.diff_context = parse_bool(value)?,
            "diff.context_lines" => self.diff_context_lines = parse_usize(value)?,
            "diff.max_ratio" => self.diff_max_ratio = parse_f64(value)?,
            "diff.ttl_minutes" => self.diff_ttl_minutes = parse_u64(value)?,

            // filetree
            "filetree.max" => self.filetree_max = parse_usize(value)?,
            "filetree.sort_mtime" => self.filetree_sort_mtime = parse_bool(value)?,

            // snapshots
            "snapshots.dir" => self.snapshot_dir = Some(PathBuf::from(value)),

            // code_graph
            "code_graph.db" => self.codegraph_db = Some(PathBuf::from(value)),
            "code_graph.disable" => self.codegraph_disable = parse_bool(value)?,

            // tests
            "tests.disable" => self.test_disable = parse_bool(value)?,
            "tests.runner" => self.test_runner = Some(value.to_string()),

            // traces
            "traces.disable" => self.traces_disable = parse_bool(value)?,

            // dedup
            "dedup.enabled" => self.dedup_enabled = parse_bool(value)?,
            "dedup.window" => self.dedup_window = parse_usize(value)?,
            "dedup.ttl_secs" => self.dedup_ttl_secs = parse_u64(value)?,
            "dedup.soft" => self.dedup_soft = parse_bool(value)?,
            "dedup.similarity" => self.dedup_similarity = parse_f64(value)?,
            "dedup.noisy_extra" => {
                self.dedup_noisy_extra = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }

            // evidence
            "evidence.disable" => self.evidence_disable = parse_bool(value)?,

            // plugins
            "plugins.spec" => self.plugins_spec = Some(value.to_string()),
            "plugins.timeout_secs" => self.plugin_timeout_secs = parse_u64(value)?,

            unknown => return Err(format!("unknown setting `{unknown}`")),
        }
        Ok(())
    }
}

/// Install the merged settings. Call exactly once at startup, before any
/// other module reads from [`get`]. A second call replaces the contents
/// (useful for tests).
pub fn init(s: Settings) {
    match SETTINGS.get() {
        Some(lock) => {
            *lock.write().expect("settings lock poisoned") = s;
        }
        None => {
            let _ = SETTINGS.set(RwLock::new(s));
        }
    }
}

fn slot() -> &'static RwLock<Settings> {
    SETTINGS.get_or_init(|| RwLock::new(Settings::defaults()))
}

/// Read the merged settings. Cheap: takes a read lock, which is
/// uncontended in steady state. Hold the guard only for as long as
/// needed (or `clone()` the field you need).
pub fn get() -> RwLockReadGuard<'static, Settings> {
    slot().read().expect("settings lock poisoned")
}

/// Snapshot a cloned `Settings` for callers that want owned data
/// without holding a lock guard.
pub fn snapshot() -> Settings {
    slot().read().expect("settings lock poisoned").clone()
}

/// Apply a mutation in-place. Used by slash commands like `/web on`
/// that flip a toggle at runtime.
pub fn update<F: FnOnce(&mut Settings)>(f: F) {
    let mut guard = slot().write().expect("settings lock poisoned");
    f(&mut guard);
}

/// Populate [`Settings`] from a `Config` and pull in the additional
/// sections that don't live in `Config`'s sub-structs (e.g. `[limits]`,
/// `[security]`, `[diff]`). Mirrors `Settings::from_config` plus the
/// freshly-added v2 sections.
pub fn from_full_config(cfg: &Config) -> Settings {
    let mut s = Settings::from_config(cfg);
    // limits
    s.max_tool_calls = cfg.limits.max_tool_calls;
    s.max_tool_calls_per_turn = cfg.limits.max_tool_calls_per_turn;
    s.max_output_tokens = cfg.limits.max_output_tokens;
    s.request_timeout_ms = cfg.limits.request_timeout_ms;
    // tools (the new fields)
    s.shell_contain = cfg.tools.shell_contain;
    s.rtk = cfg.tools.rtk;
    // features (the new field)
    s.bootstrap_max_chars = cfg.features.bootstrap_max_chars;
    // security
    s.allow_outside_paths = cfg.security.allow_outside_paths;
    s.allow_public_endpoints = cfg.security.allow_public_endpoints;
    s.web_respect_robots = cfg.security.web_respect_robots;
    s.searx_url = cfg.security.searx_url.clone();
    // diff
    s.diff_context = cfg.diff.context;
    s.diff_context_lines = cfg.diff.context_lines;
    s.diff_max_ratio = cfg.diff.max_ratio;
    s.diff_ttl_minutes = cfg.diff.ttl_minutes;
    // filetree
    s.filetree_max = cfg.filetree.max;
    s.filetree_sort_mtime = cfg.filetree.sort_mtime;
    // snapshots
    s.snapshot_dir = cfg.snapshots.dir.clone();
    // code_graph
    s.codegraph_db = cfg.code_graph.db.clone();
    s.codegraph_disable = cfg.code_graph.disable;
    // tests
    s.test_disable = cfg.tests.disable;
    s.test_runner = cfg.tests.runner.clone();
    // traces
    s.traces_disable = cfg.traces.disable;
    // dedup
    s.dedup_enabled = cfg.dedup.enabled;
    s.dedup_window = cfg.dedup.window;
    s.dedup_ttl_secs = cfg.dedup.ttl_secs;
    s.dedup_soft = cfg.dedup.soft;
    s.dedup_similarity = cfg.dedup.similarity;
    s.dedup_noisy_extra = cfg.dedup.noisy_extra.clone();
    // evidence
    s.evidence_disable = cfg.evidence.disable;
    // plugins
    s.plugins_spec = cfg.plugins.spec.clone();
    s.plugin_timeout_secs = cfg.plugins.timeout_secs;
    // diag
    s.verbose = cfg.diag.verbose;
    s.profile = cfg.diag.profile.clone();
    s.debug_sigtstp = cfg.diag.debug_sigtstp;
    // tui
    s.tui_classic = cfg.tui.classic;
    s
}
