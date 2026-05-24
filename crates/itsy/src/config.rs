//! Configuration loader. Precedence (last wins):
//!
//!   1. Built-in defaults
//!   2. `~/.config/itsy/config.toml` (TOML, versioned, migration-aware)
//!   3. CLI flags
//!
//! `ITSY_*` env vars are no longer consulted as of v2 of the schema —
//! every previously-env-only knob now lives in [`Config`] (or one of
//! its sub-structs) and is overridable via a CLI flag. `ITSY_HOME` is
//! the lone exception because it resolves before the config file is
//! loaded; see [`crate::paths`].
//!
//! The TOML file has a `version = "N"` field at the top. On load, any
//! version older than [`CURRENT_CONFIG_VERSION`] is run through the
//! migration chain in [`MIGRATIONS`] before being parsed into [`Config`].

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

/// CLI flags that may override config values.
#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub base_url: Option<String>,
    pub classic: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    #[serde(rename = "baseUrl", alias = "base_url")]
    pub base_url: String,
    pub timeout: u64,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub max_budget_pct: u32,
    pub detected_window: u32,
    pub working_memory_tokens: u32,
    pub summary_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub bash_timeout: u32,
    /// Tool routing mode: "direct" or "two_stage" or "auto".
    #[serde(default = "default_tool_routing")]
    pub tool_routing: String,
    /// Enable web_search / web_fetch tools.
    #[serde(default)]
    pub web_browse: bool,
    /// Use a persistent shell so `cd src` etc. stick across calls.
    #[serde(default = "default_true")]
    pub shell_persist: bool,
    /// Contain bash cwd to project root.
    #[serde(default)]
    pub shell_contain: bool,
    /// Enable reasoning-format toolkit (parses non-JSON tool calls).
    #[serde(default = "default_true")]
    pub rtk: bool,
}

fn default_tool_routing() -> String {
    "direct".into()
}

fn default_true() -> bool {
    true
}

/// Small-model safeguards. Every toggle here is a real field on disk;
/// they used to also be readable from `ITSY_*` env vars but the env-var
/// layer was removed in schema v2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesConfig {
    /// Wrap each turn in a checkpoint so failures can roll back.
    #[serde(default = "default_true")]
    pub snapshot: bool,
    /// Auto-rollback on hard failure (build break, exception).
    #[serde(default = "default_true")]
    pub snapshot_auto_rollback: bool,
    /// Refuse `write_file` on a file the model hasn't read this turn.
    #[serde(default = "default_true")]
    pub write_guard: bool,
    /// Inject a project bootstrap (cwd, file listing, project type) on the
    /// first message of a new session.
    #[serde(default = "default_true")]
    pub bootstrap: bool,
    /// Cap on the size of the bootstrap injection in characters.
    #[serde(default = "default_bootstrap_max")]
    pub bootstrap_max_chars: usize,
    /// Penalise tools that fail and re-promote tools that succeed.
    #[serde(default = "default_true")]
    pub trust_decay: bool,
    /// Adapt sampling temperature based on recent repair history.
    #[serde(default = "default_true")]
    pub temp_adapt: bool,
    /// Hard cap for reasoning-model thinking tokens per turn. `0` means
    /// "use the per-task heuristic".
    #[serde(default)]
    pub thinking_budget: u32,
    /// Detect vague short inputs and inject a system message asking the
    /// model to clarify before acting.
    #[serde(default = "default_true")]
    pub clarifier: bool,
    /// Recover from `patch` failures (`old_str` not found) by asking the
    /// model to merge the intended change into the current file content.
    #[serde(default = "default_true")]
    pub semantic_merge: bool,
    /// Diagnose `bash` failures via an LLM-derived hint prepended to the
    /// tool result.
    #[serde(default = "default_true")]
    pub error_diagnosis: bool,
    /// LLM self-critique after every successful write/patch. Costs one
    /// extra LLM call per edit — off by default for small models.
    #[serde(default)]
    pub validate_edits: bool,
    /// Inject relevant code-graph hits into the system prompt for long
    /// user messages.
    #[serde(default = "default_true")]
    pub context_retrieval: bool,
    /// Enable the on-disk contract feature: model commits to a list of
    /// testable assertions up front, marks each passed/failed with
    /// command evidence, and can't close the contract as `completed`
    /// while any assertion is still pending. Defends against the
    /// "I'm done" fabrication failure mode.
    #[serde(default = "default_true")]
    pub contract: bool,
}

fn default_bootstrap_max() -> usize {
    4000
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            snapshot: true,
            snapshot_auto_rollback: true,
            write_guard: true,
            bootstrap: true,
            bootstrap_max_chars: 4000,
            trust_decay: true,
            temp_adapt: true,
            thinking_budget: 0,
            clarifier: true,
            semantic_merge: true,
            error_diagnosis: true,
            validate_edits: false,
            context_retrieval: true,
            contract: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub show_token_usage: bool,
    pub auto_approve: bool,
    pub theme: String,
    #[serde(default)]
    pub classic: bool,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub auto_commit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiModels {
    pub fast: String,
    pub default: String,
    pub strong: String,
}

/// Per-run hard limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_tool_calls_per_turn")]
    pub max_tool_calls_per_turn: u32,
    /// 0 = auto (`thinking_budget + 4096`).
    #[serde(default)]
    pub max_output_tokens: u32,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tool_calls_per_turn: 250,
            max_output_tokens: 0,
            request_timeout_ms: 120_000,
        }
    }
}

fn default_max_tool_calls_per_turn() -> u32 { 250 }
fn default_request_timeout_ms() -> u64 { 120_000 }

/// Filesystem / network safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Allow read/write tools to touch absolute paths outside the project
    /// root. Sensitive-path patterns (credentials, ssh keys) are still
    /// blocked regardless.
    #[serde(default = "default_true")]
    pub allow_outside_paths: bool,
    /// Allow `web_fetch` to hit public internet endpoints (default:
    /// loopback / link-local only).
    #[serde(default)]
    pub allow_public_endpoints: bool,
    /// Respect robots.txt for `web_fetch`.
    #[serde(default)]
    pub web_respect_robots: bool,
    /// SearxNG instance to use for `web_search`.
    #[serde(default)]
    pub searx_url: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allow_outside_paths: true,
            allow_public_endpoints: false,
            web_respect_robots: false,
            searx_url: None,
        }
    }
}

/// Read-tracker diff display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffConfig {
    /// Show contextual diff vs. last-read state.
    #[serde(default)]
    pub context: bool,
    #[serde(default = "default_diff_lines")]
    pub context_lines: usize,
    #[serde(default = "default_diff_ratio")]
    pub max_ratio: f64,
    #[serde(default = "default_diff_ttl")]
    pub ttl_minutes: u64,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            context: false,
            context_lines: 3,
            max_ratio: 0.5,
            ttl_minutes: 30,
        }
    }
}

fn default_diff_lines() -> usize { 3 }
fn default_diff_ratio() -> f64 { 0.5 }
fn default_diff_ttl() -> u64 { 30 }

/// `find_files` / file-tree tool tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTreeConfig {
    #[serde(default = "default_filetree_max")]
    pub max: usize,
    /// `true` to sort by mtime, `false` for default (alphabetical).
    #[serde(default)]
    pub sort_mtime: bool,
}

impl Default for FileTreeConfig {
    fn default() -> Self {
        Self {
            max: 200,
            sort_mtime: false,
        }
    }
}

fn default_filetree_max() -> usize { 200 }

/// Snapshot store overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotPathsConfig {
    /// Directory that holds snapshot files. `None` → default under
    /// `~/.config/itsy/snapshots`.
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// Native code-graph index tuning.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeGraphConfig {
    /// Path to the SQLite DB. `None` → default under `~/.config/itsy/`.
    #[serde(default)]
    pub db: Option<PathBuf>,
    /// Disable the code-graph index entirely.
    #[serde(default)]
    pub disable: bool,
}

/// Test-runner tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestsConfig {
    #[serde(default)]
    pub disable: bool,
    /// Override the auto-detected test runner command.
    #[serde(default)]
    pub runner: Option<String>,
}

/// Trace recorder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TracesConfig {
    #[serde(default)]
    pub disable: bool,
}

/// Tool-call deduplicator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_dedup_window")]
    pub window: usize,
    #[serde(default = "default_dedup_ttl")]
    pub ttl_secs: u64,
    /// Warn instead of caching on duplicate.
    #[serde(default)]
    pub soft: bool,
    #[serde(default = "default_dedup_sim")]
    pub similarity: f64,
    /// Extra tool names to bypass dedup (in addition to the built-in list).
    #[serde(default)]
    pub noisy_extra: Vec<String>,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window: 5,
            ttl_secs: 30,
            soft: false,
            similarity: 0.92,
            noisy_extra: Vec::new(),
        }
    }
}

fn default_dedup_window() -> usize { 5 }
fn default_dedup_ttl() -> u64 { 30 }
fn default_dedup_sim() -> f64 { 0.92 }

/// Evidence-memory store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceConfig {
    #[serde(default)]
    pub disable: bool,
}

/// External plugin loader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Comma-separated plugin spec (was `ITSY_PLUGINS`).
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default = "default_plugin_timeout")]
    pub timeout_secs: u64,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            spec: None,
            timeout_secs: 30,
        }
    }
}

fn default_plugin_timeout() -> u64 { 30 }

/// A secondary model used for independent second-opinion calls:
/// adversarial evaluator and any other place where a fresh perspective
/// on the main model's output is useful.
/// Defaults to the main model when not set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecondOpinionConfig {
    /// Model name override. `None` → use main model.
    #[serde(default)]
    pub model: Option<String>,
    /// Endpoint URL override. `None` → use main endpoint.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl SecondOpinionConfig {
    /// Resolve the effective model name, falling back to the main config.
    pub fn resolved_model<'a>(&'a self, main: &'a Config) -> &'a str {
        self.model.as_deref().unwrap_or(&main.model.name)
    }

    /// Resolve the effective endpoint, falling back to the main config.
    pub fn resolved_endpoint<'a>(&'a self, main: &'a Config) -> &'a str {
        self.endpoint.as_deref().unwrap_or(&main.model.base_url)
    }
}

/// Diagnostic / dev knobs that aren't worth a wizard prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagConfig {
    /// Verbose tool output.
    #[serde(default)]
    pub verbose: bool,
    /// Picks a built-in model profile by name.
    #[serde(default)]
    pub profile: Option<String>,
    /// Print SIGTSTP debug info.
    #[serde(default)]
    pub debug_sigtstp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub tui: TuiConfig,
    pub git: GitConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub models: Option<MultiModels>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub diff: DiffConfig,
    #[serde(default)]
    pub filetree: FileTreeConfig,
    #[serde(default)]
    pub snapshots: SnapshotPathsConfig,
    #[serde(default)]
    pub code_graph: CodeGraphConfig,
    #[serde(default)]
    pub tests: TestsConfig,
    #[serde(default)]
    pub traces: TracesConfig,
    #[serde(default)]
    pub dedup: DedupConfig,
    #[serde(default)]
    pub evidence: EvidenceConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub diag: DiagConfig,
    #[serde(default)]
    pub second_opinion: SecondOpinionConfig,
}

/// Schema version currently understood. Bump when a breaking change is
/// introduced and a corresponding entry is added to [`MIGRATIONS`].
pub const CURRENT_CONFIG_VERSION: &str = "2";

/// On-disk shape of `config.toml`. The top-level `version` field is
/// inspected before parsing; older versions are migrated to current before
/// being deserialised into [`Config`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub context: Option<ContextConfig>,
    #[serde(default)]
    pub tools: Option<ToolsConfig>,
    #[serde(default)]
    pub tui: Option<TuiConfig>,
    #[serde(default)]
    pub git: Option<GitConfig>,
    #[serde(default)]
    pub features: Option<FeaturesConfig>,
    #[serde(default)]
    pub models: Option<MultiModels>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    #[serde(default)]
    pub security: Option<SecurityConfig>,
    #[serde(default)]
    pub diff: Option<DiffConfig>,
    #[serde(default)]
    pub filetree: Option<FileTreeConfig>,
    #[serde(default)]
    pub snapshots: Option<SnapshotPathsConfig>,
    #[serde(default)]
    pub code_graph: Option<CodeGraphConfig>,
    #[serde(default)]
    pub tests: Option<TestsConfig>,
    #[serde(default)]
    pub traces: Option<TracesConfig>,
    #[serde(default)]
    pub dedup: Option<DedupConfig>,
    #[serde(default)]
    pub evidence: Option<EvidenceConfig>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub diag: Option<DiagConfig>,
    #[serde(default)]
    pub second_opinion: Option<SecondOpinionConfig>,
}

fn default_version() -> String {
    CURRENT_CONFIG_VERSION.into()
}

/// One step in the migration chain. Takes the raw `toml::Value` of the
/// file at version `from`, returns the equivalent value bumped to the
/// next version. If the migration needs user input to proceed, return
/// `Err(MigrationError::NeedsUserInput(prompt))` so the caller (the init
/// wizard at first launch) can interactively complete it.
pub type Migration = fn(&mut toml::Value) -> Result<()>;

/// Migration chain. Each entry runs in order until the file version
/// matches [`CURRENT_CONFIG_VERSION`]. Entries are keyed by the version
/// they migrate *from*.
pub const MIGRATIONS: &[(&str, Migration)] = &[
    ("1", v1_to_v2),
];

/// v1 → v2 migration: env-var support was dropped. The migration is
/// purely additive — every new section gets created with defaults if it
/// wasn't already present. Nothing existing is dropped.
fn v1_to_v2(value: &mut toml::Value) -> Result<()> {
    let Some(table) = value.as_table_mut() else {
        anyhow::bail!("config root is not a table");
    };
    for key in [
        "limits", "security", "diff", "filetree", "snapshots", "code_graph",
        "tests", "traces", "dedup", "evidence", "plugins", "diag",
    ] {
        table
            .entry(key.to_string())
            .or_insert(toml::Value::Table(Default::default()));
    }
    // [features] picked up a new field this version — give old files the
    // default so deserialisation doesn't fail on missing keys without
    // a #[serde(default)].
    if let Some(features) = table.get_mut("features").and_then(|v| v.as_table_mut()) {
        features
            .entry("bootstrap_max_chars".to_string())
            .or_insert(toml::Value::Integer(4000));
    }
    // [tools] picked up shell_contain + rtk.
    if let Some(tools) = table.get_mut("tools").and_then(|v| v.as_table_mut()) {
        tools
            .entry("shell_contain".to_string())
            .or_insert(toml::Value::Boolean(false));
        tools
            .entry("rtk".to_string())
            .or_insert(toml::Value::Boolean(true));
    }
    table.insert("version".into(), toml::Value::String("2".into()));
    Ok(())
}

impl ConfigFile {
    /// Parse a TOML file. Runs any pending schema migrations before
    /// deserialising into the typed struct.
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read config: {}", path.display()))?;
        let mut value: toml::Value = toml::from_str(&text)
            .with_context(|| format!("parse config: {}", path.display()))?;
        Self::apply_migrations(&mut value)?;
        let parsed: ConfigFile = value
            .try_into()
            .with_context(|| format!("deserialise config: {}", path.display()))?;
        Ok(parsed)
    }

    /// Apply migrations in-place, bumping the embedded `version` to the
    /// current value. No-op if the file already matches.
    pub fn apply_migrations(value: &mut toml::Value) -> Result<()> {
        loop {
            let current = value
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("1")
                .to_string();
            if current == CURRENT_CONFIG_VERSION {
                return Ok(());
            }
            let migrator = MIGRATIONS
                .iter()
                .find(|(from, _)| *from == current)
                .map(|(_, m)| *m);
            match migrator {
                Some(m) => m(value)?,
                None => {
                    anyhow::bail!(
                        "config version {current} has no migration to {CURRENT_CONFIG_VERSION}",
                    );
                }
            }
        }
    }

    /// Save to disk, prepending a friendly header. Atomic via tmp+rename.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut body = String::new();
        body.push_str("# itsy config — see https://github.com/jukefr/itsy\n");
        body.push_str("# This file is migration-aware: the `version` field at top\n");
        body.push_str("# is honored on load and the file is rewritten in newer\n");
        body.push_str("# layouts as needed. Do not remove `version`.\n\n");
        body.push_str(&toml::to_string_pretty(self).with_context(|| "serialise config")?);
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Merge any populated sections of this file into a base [`Config`].
    pub fn apply_to(&self, config: &mut Config) {
        if let Some(m) = self.model.clone() {
            config.model = m;
        }
        if let Some(c) = self.context.clone() {
            config.context = c;
        }
        if let Some(t) = self.tools.clone() {
            config.tools = t;
        }
        if let Some(t) = self.tui.clone() {
            config.tui = t;
        }
        if let Some(g) = self.git.clone() {
            config.git = g;
        }
        if let Some(f) = self.features.clone() {
            config.features = f;
        }
        if self.models.is_some() {
            config.models = self.models.clone();
        }
        if let Some(v) = self.limits.clone() {
            config.limits = v;
        }
        if let Some(v) = self.security.clone() {
            config.security = v;
        }
        if let Some(v) = self.diff.clone() {
            config.diff = v;
        }
        if let Some(v) = self.filetree.clone() {
            config.filetree = v;
        }
        if let Some(v) = self.snapshots.clone() {
            config.snapshots = v;
        }
        if let Some(v) = self.code_graph.clone() {
            config.code_graph = v;
        }
        if let Some(v) = self.tests.clone() {
            config.tests = v;
        }
        if let Some(v) = self.traces.clone() {
            config.traces = v;
        }
        if let Some(v) = self.dedup.clone() {
            config.dedup = v;
        }
        if let Some(v) = self.evidence.clone() {
            config.evidence = v;
        }
        if let Some(v) = self.plugins.clone() {
            config.plugins = v;
        }
        if let Some(v) = self.diag.clone() {
            config.diag = v;
        }
        if let Some(v) = self.second_opinion.clone() {
            config.second_opinion = v;
        }
    }
}

impl From<&Config> for ConfigFile {
    fn from(c: &Config) -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION.into(),
            model: Some(c.model.clone()),
            context: Some(c.context.clone()),
            tools: Some(c.tools.clone()),
            tui: Some(c.tui.clone()),
            git: Some(c.git.clone()),
            features: Some(c.features.clone()),
            models: c.models.clone(),
            limits: Some(c.limits.clone()),
            security: Some(c.security.clone()),
            diff: Some(c.diff.clone()),
            filetree: Some(c.filetree.clone()),
            snapshots: Some(c.snapshots.clone()),
            code_graph: Some(c.code_graph.clone()),
            tests: Some(c.tests.clone()),
            traces: Some(c.traces.clone()),
            dedup: Some(c.dedup.clone()),
            evidence: Some(c.evidence.clone()),
            plugins: Some(c.plugins.clone()),
            diag: Some(c.diag.clone()),
            second_opinion: if c.second_opinion.model.is_some() || c.second_opinion.endpoint.is_some() {
                Some(c.second_opinion.clone())
            } else {
                None
            },
        }
    }
}

/// Backwards-compatible no-op. The env-var bridge was removed in
/// schema v2 — every consumer now reads from [`crate::settings`]. This
/// shim stays so external callers that still invoke
/// `load_dotenv()` keep compiling during the deprecation window.
pub fn load_dotenv() {
    // Intentionally empty. Persistent settings live in `config.toml`.
}

/// Read a non-empty env var. Retained for *external* env vars such as
/// `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OLLAMA_HOST` — those are
/// upstream conventions and are still honoured. **Do not use this for
/// any `ITSY_*` variable**; use [`crate::settings::get`] instead.
fn env_str(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

pub fn load_config(flags: &Flags) -> Config {
    // Start from built-in defaults. Every section has its own `Default`
    // impl. `Config` itself doesn't derive `Default` because the
    // top-level [model] section is mandatory and has no sensible blank
    // value — we materialise it here.
    let mut config = Config {
        model: ModelConfig {
            provider: "openai".into(),
            name: String::new(),
            base_url: "http://localhost:1234/v1".into(),
            timeout: 300,
            api_key: None,
        },
        context: ContextConfig {
            max_budget_pct: 70,
            detected_window: 128_000,
            working_memory_tokens: 500,
            summary_threshold: 200,
        },
        tools: ToolsConfig {
            bash_timeout: 30,
            tool_routing: "direct".into(),
            web_browse: false,
            shell_persist: true,
            shell_contain: false,
            rtk: true,
        },
        tui: TuiConfig {
            show_token_usage: true,
            auto_approve: false,
            theme: "dark".into(),
            classic: false,
        },
        git: GitConfig { auto_commit: false },
        features: FeaturesConfig::default(),
        models: None,
        limits: LimitsConfig::default(),
        security: SecurityConfig::default(),
        diff: DiffConfig::default(),
        filetree: FileTreeConfig::default(),
        snapshots: SnapshotPathsConfig::default(),
        code_graph: CodeGraphConfig::default(),
        tests: TestsConfig::default(),
        traces: TracesConfig::default(),
        dedup: DedupConfig::default(),
        evidence: EvidenceConfig::default(),
        plugins: PluginsConfig::default(),
        diag: DiagConfig::default(),
        second_opinion: SecondOpinionConfig::default(),
    };

    // Merge the persisted TOML file on top of defaults. If the file is
    // missing or fails to parse, defaults stand.
    let toml_path = crate::paths::config_file();
    if toml_path.exists() {
        if let Ok(file) = ConfigFile::load_from_path(&toml_path) {
            file.apply_to(&mut config);
        }
    }

    // Apply CLI overrides last.
    if let Some(m) = flags.model.clone() {
        config.model.name = m;
    }
    if let Some(p) = flags.provider.clone() {
        config.model.provider = p;
    }
    if let Some(b) = flags.endpoint.clone().or_else(|| flags.base_url.clone()) {
        config.model.base_url = b;
    }
    // Normalise: prepend http:// if the URL lacks a scheme. `reqwest` and our
    // SSRF guard both refuse `host:port/path` URLs without one.
    config.model.base_url = normalize_base_url(&config.model.base_url);
    if flags.classic {
        config.tui.classic = true;
    }
    if flags.verbose {
        config.diag.verbose = true;
    }

    config
}

/// Prepend `http://` if the URL string lacks a scheme. Trims a trailing
/// slash so `host:port/v1/` and `host:port/v1` are equivalent. Falls back
/// to the input unchanged when it already has a scheme.
pub fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "http://localhost:1234/v1".to_string();
    }
    let has_scheme = trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("unix://");
    if has_scheme {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Build auth headers for API requests (mirrors `buildAuthHeaders`).
pub fn build_auth_headers(config: &Config) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = env_str("OPENAI_API_KEY")
        .or_else(|| env_str("ANTHROPIC_API_KEY"))
        .or_else(|| env_str("DEEPSEEK_API_KEY"))
        .or_else(|| config.model.api_key.clone());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    if config.model.base_url.contains("openrouter.ai") {
        if let Ok(v) = HeaderValue::from_str("itsy") {
            headers.insert("X-Title", v);
        }
    }
    headers
}

/// Build auth headers from explicit fields (no &Config dependency).
pub fn build_auth_headers_for(
    api_key: Option<&str>,
    base_url: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let key = env_str("OPENAI_API_KEY")
        .or_else(|| env_str("ANTHROPIC_API_KEY"))
        .or_else(|| env_str("DEEPSEEK_API_KEY"))
        .or_else(|| api_key.map(String::from));
    if let Some(k) = key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    if base_url.contains("openrouter.ai") {
        if let Ok(v) = HeaderValue::from_str("itsy") {
            headers.insert("X-Title", v);
        }
    }
    headers
}

/// Mirror of `checkEndpoint` — prints status, mutates context window if the
/// remote advertises one. Returns whether the endpoint was reachable.
pub async fn check_endpoint(config: &mut Config) -> bool {
    let ollama_host = env_str("OLLAMA_HOST").unwrap_or_else(|| "http://localhost:11434".into());
    let base_url = if !config.model.base_url.is_empty() {
        config.model.base_url.clone()
    } else {
        ollama_host.clone()
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    if config.model.provider == "openai" || base_url.contains("/v1") {
        let mut req = client.get(format!("{base_url}/models"));
        let api_key = env_str("OPENAI_API_KEY")
            .or_else(|| env_str("ANTHROPIC_API_KEY"))
            .or_else(|| env_str("DEEPSEEK_API_KEY"));
        if let Some(k) = api_key {
            req = req.bearer_auth(k);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let models = data.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                    if !models.is_empty() {
                        println!("  Connected: {base_url}");
                        println!("  Model: {}", config.model.name);
                        let want = config.model.name.as_str();
                        let active = models
                            .iter()
                            .find(|m| {
                                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let nm = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                id.contains(want) || nm.contains(want)
                            })
                            .unwrap_or(&models[0]);
                        if let Some(cl) = active.get("context_length").and_then(|v| v.as_u64()) {
                            config.context.detected_window = cl as u32;
                            println!("  Context: {cl} tokens");
                        }
                    }
                }
                true
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                println!("  ⚠ Cannot reach endpoint at {base_url}");
                println!("  Check that your model server is running and accessible.");
                if status == 401 || status == 403 {
                    println!("  Got {status} — set OPENAI_API_KEY in .env if your server requires auth.");
                }
                false
            }
            Err(_) => {
                println!("  ⚠ Cannot reach endpoint at {base_url}");
                println!("  Check that your model server is running and the URL is correct.");
                false
            }
        }
    } else {
        let host = ollama_host;
        match client.get(format!("{host}/api/tags")).send().await {
            Ok(resp) if resp.status().is_success() => {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                let want = config.model.name.split(':').next().unwrap_or("");
                let has_model = data
                    .get("models")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter().any(|m| {
                            m.get("name").and_then(|n| n.as_str()).is_some_and(|n| n.contains(want))
                        })
                    })
                    .unwrap_or(false);
                if !has_model {
                    println!("  ⚠ Model \"{}\" not found in Ollama.", config.model.name);
                    println!("  Run: ollama pull {}", config.model.name);
                    return false;
                }
                true
            }
            _ => {
                println!("  ⚠ Ollama not running. Start it with: ollama serve");
                false
            }
        }
    }
}

/// Resolve which API key, if any, would be used for the current provider.
pub fn resolve_api_key(config: &Config) -> Option<String> {
    env_str("OPENAI_API_KEY")
        .or_else(|| env_str("ANTHROPIC_API_KEY"))
        .or_else(|| env_str("DEEPSEEK_API_KEY"))
        .or_else(|| config.model.api_key.clone())
}

/// Returns a stable map of provider → env-var name for diagnostic UI.
pub fn provider_env_map() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("anthropic", "ANTHROPIC_API_KEY");
    m.insert("openai", "OPENAI_API_KEY");
    m.insert("deepseek", "DEEPSEEK_API_KEY");
    m
}

pub fn deps_ok() -> Result<()> {
    Ok(())
}
