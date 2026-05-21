//! Configuration loader. Precedence (last wins):
//!
//!   1. Built-in defaults
//!   2. `~/.config/itsy/config.toml` (TOML, versioned, migration-aware)
//!   3. `~/.config/itsy/.env` env-var overrides
//!   4. Live process environment (`ITSY_*`)
//!   5. CLI flags
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
    /// Tool routing mode: "direct" or "two_stage". Env var
    /// `ITSY_TOOL_ROUTING` still overrides this at runtime.
    #[serde(default = "default_tool_routing")]
    pub tool_routing: String,
    /// Enable web_search / web_fetch tools. Sets `ITSY_WEB_BROWSE`.
    #[serde(default)]
    pub web_browse: bool,
    /// Use a persistent shell so `cd src` etc. stick across calls. Sets
    /// `ITSY_SHELL_PERSIST`.
    #[serde(default = "default_true")]
    pub shell_persist: bool,
}

fn default_tool_routing() -> String {
    "direct".into()
}

fn default_true() -> bool {
    true
}

/// Small-model safeguards. Each toggle here propagates to the matching
/// `ITSY_*` env var the runtime reads — so the env always wins, but the
/// TOML provides a stable default per project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesConfig {
    /// Auto-detect multi-step tasks and ask the model to commit to a plan
    /// before executing. Sets `ITSY_PLAN`.
    #[serde(default = "default_true")]
    pub plan: bool,
    /// Wrap each turn in a checkpoint so failures can roll back. Sets
    /// `ITSY_SNAPSHOT`.
    #[serde(default = "default_true")]
    pub snapshot: bool,
    /// Auto-rollback on hard failure (build break, exception). Sets
    /// `ITSY_SNAPSHOT_AUTO_ROLLBACK`.
    #[serde(default = "default_true")]
    pub snapshot_auto_rollback: bool,
    /// Refuse `write_file` on a file the model hasn't read this turn. Sets
    /// `ITSY_WRITE_GUARD`.
    #[serde(default = "default_true")]
    pub write_guard: bool,
    /// Inject a project bootstrap (cwd, file listing, project type) on the
    /// first message of a new session. Sets `ITSY_BOOTSTRAP`.
    #[serde(default = "default_true")]
    pub bootstrap: bool,
    /// Penalise tools that fail and re-promote tools that succeed. Sets
    /// `ITSY_TRUST_DECAY`.
    #[serde(default = "default_true")]
    pub trust_decay: bool,
    /// Adapt sampling temperature based on recent repair history. Sets
    /// `ITSY_TEMP_ADAPT`.
    #[serde(default = "default_true")]
    pub temp_adapt: bool,
    /// Hard cap for reasoning-model thinking tokens per turn. `0` means
    /// "use the per-task heuristic". Sets `ITSY_THINKING_BUDGET`.
    #[serde(default)]
    pub thinking_budget: u32,
    /// Detect vague short inputs and inject a system message asking the
    /// model to clarify before acting. Sets `ITSY_CLARIFIER`.
    #[serde(default = "default_true")]
    pub clarifier: bool,
    /// Recover from `patch` failures (`old_str` not found) by asking the
    /// model to merge the intended change into the current file content.
    /// Sets `ITSY_SEMANTIC_MERGE`.
    #[serde(default = "default_true")]
    pub semantic_merge: bool,
    /// Diagnose `bash` failures via an LLM-derived hint prepended to the
    /// tool result. Sets `ITSY_ERROR_DIAGNOSIS`.
    #[serde(default = "default_true")]
    pub error_diagnosis: bool,
    /// LLM self-critique after every successful write/patch. Costs one
    /// extra LLM call per edit — off by default for small models. Sets
    /// `ITSY_VALIDATE_EDITS`.
    #[serde(default)]
    pub validate_edits: bool,
    /// Inject relevant code-graph hits into the system prompt for long
    /// user messages. Sets `ITSY_CONTEXT_RETRIEVAL`.
    #[serde(default = "default_true")]
    pub context_retrieval: bool,
    /// Post-hoc reviewer that asks the model "does this look right?" at
    /// the end of each turn. Costs an extra LLM call per turn. Sets
    /// `ITSY_REVIEWER`.
    #[serde(default)]
    pub reviewer: bool,
    /// Run the user prompt through a planner→executor chain before the
    /// main agent loop. Costs an extra LLM call. Sets `ITSY_CHAIN`.
    #[serde(default)]
    pub chain: bool,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            plan: true,
            snapshot: true,
            snapshot_auto_rollback: true,
            write_guard: true,
            bootstrap: true,
            trust_decay: true,
            temp_adapt: true,
            thinking_budget: 0,
            clarifier: true,
            semantic_merge: true,
            error_diagnosis: true,
            validate_edits: false,
            context_retrieval: true,
            reviewer: false,
            chain: false,
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
pub struct EscalationConfig {
    pub enabled: bool,
    pub max_per_session: u32,
    pub confirm: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub tui: TuiConfig,
    pub escalation: EscalationConfig,
    pub git: GitConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub models: Option<MultiModels>,
}

/// Schema version currently understood. Bump when a breaking change is
/// introduced and a corresponding entry is added to [`MIGRATIONS`].
pub const CURRENT_CONFIG_VERSION: &str = "1";

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
    pub escalation: Option<EscalationConfig>,
    #[serde(default)]
    pub git: Option<GitConfig>,
    #[serde(default)]
    pub features: Option<FeaturesConfig>,
    #[serde(default)]
    pub models: Option<MultiModels>,
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
///
/// Add entries here when bumping the schema. v1 is the initial shipped
/// schema so this list is empty for now.
pub const MIGRATIONS: &[(&str, Migration)] = &[
    // ("1", v1_to_v2),  // example: when shipping v2, add the migrator here
];

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
        if let Some(e) = self.escalation.clone() {
            config.escalation = e;
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
            escalation: Some(c.escalation.clone()),
            git: Some(c.git.clone()),
            features: Some(c.features.clone()),
            models: c.models.clone(),
        }
    }
}

/// Propagate config toggles into the corresponding `ITSY_*` env vars so the
/// many code paths that read env vars (snapshot, write_guard, plan_tracker,
/// shell_session, web_browse, …) see the configured values without each
/// having to add a separate `Config` plumbing path.
///
/// **Precedence:** an env var that is *already set* in the process is never
/// overwritten — so `ITSY_WEB_BROWSE=true ./itsy ...` always wins over the
/// TOML, matching the documented load order.
pub fn propagate_features_to_env(config: &Config) {
    let mut set = |name: &str, val: &str| {
        if env::var(name).is_err() {
            // SAFETY: called during single-threaded startup before any
            // worker threads touch the environment.
            unsafe { env::set_var(name, val) };
        }
    };
    let f = &config.features;
    set("ITSY_PLAN", bool_env(f.plan));
    set("ITSY_SNAPSHOT", bool_env(f.snapshot));
    set("ITSY_SNAPSHOT_AUTO_ROLLBACK", bool_env(f.snapshot_auto_rollback));
    set("ITSY_WRITE_GUARD", bool_env(f.write_guard));
    set("ITSY_BOOTSTRAP", bool_env(f.bootstrap));
    set("ITSY_TRUST_DECAY", bool_env(f.trust_decay));
    set("ITSY_TEMP_ADAPT", bool_env(f.temp_adapt));
    if f.thinking_budget > 0 {
        set("ITSY_THINKING_BUDGET", &f.thinking_budget.to_string());
    }
    set("ITSY_SHELL_PERSIST", bool_env(config.tools.shell_persist));
    set("ITSY_WEB_BROWSE", bool_env(config.tools.web_browse));
    set("ITSY_TOOL_ROUTING", &config.tools.tool_routing);
    set("ITSY_CLARIFIER", bool_env(f.clarifier));
    set("ITSY_SEMANTIC_MERGE", bool_env(f.semantic_merge));
    set("ITSY_ERROR_DIAGNOSIS", bool_env(f.error_diagnosis));
    set("ITSY_VALIDATE_EDITS", bool_env(f.validate_edits));
    set("ITSY_CONTEXT_RETRIEVAL", bool_env(f.context_retrieval));
    set("ITSY_REVIEWER", bool_env(f.reviewer));
    set("ITSY_CHAIN", bool_env(f.chain));
}

fn bool_env(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_str(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

/// Load `.env` overrides from the user's config directory. Project-local
/// `.env` files are *not* read — runtime state and config live under
/// `~/.config/itsy/`. The full TOML config (see [`crate::paths::config_file`])
/// is the canonical place for user settings; `.env` is a fallback for one-off
/// env overrides.
pub fn load_dotenv() {
    let paths = [crate::paths::config_dir().join(".env")];
    for p in &paths {
        if !p.exists() {
            continue;
        }
        let Ok(content) = fs::read_to_string(p) else { continue };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some(eq) = trimmed.find('=') else { continue };
            let key = trimmed[..eq].trim();
            let mut value = trimmed[eq + 1..].trim().to_string();
            let bytes = value.as_bytes();
            if value.len() >= 2
                && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                    || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
            {
                value = value[1..value.len() - 1].to_string();
            }
            if env::var(key).is_err() {
                // SAFETY: process is still single-threaded during startup.
                unsafe { env::set_var(key, value) };
            }
        }
        break;
    }
}

pub fn load_config(flags: &Flags) -> Config {
    let base_url_default = if let Some(host) = env_str("OLLAMA_HOST") {
        format!("{host}/v1")
    } else {
        "http://localhost:1234/v1".to_string()
    };

    let mut config = Config {
        model: ModelConfig {
            provider: env_str("ITSY_PROVIDER").unwrap_or_else(|| "openai".into()),
            name: env_str("ITSY_MODEL").unwrap_or_default(),
            base_url: env_str("ITSY_BASE_URL").unwrap_or(base_url_default),
            timeout: env_or("ITSY_MODEL_TIMEOUT", 300u64),
            api_key: None,
        },
        context: ContextConfig {
            max_budget_pct: env_or("ITSY_CONTEXT_BUDGET", 70),
            detected_window: env_or("ITSY_CONTEXT_WINDOW", 128_000),
            working_memory_tokens: 500,
            summary_threshold: 200,
        },
        tools: ToolsConfig {
            bash_timeout: env_or("ITSY_BASH_TIMEOUT", 30),
            tool_routing: env_str("ITSY_TOOL_ROUTING").unwrap_or_else(|| "direct".into()),
            web_browse: env_str("ITSY_WEB_BROWSE").as_deref() == Some("true"),
            shell_persist: env_str("ITSY_SHELL_PERSIST").as_deref() != Some("false"),
        },
        tui: TuiConfig {
            show_token_usage: true,
            auto_approve: env_str("ITSY_AUTO_APPROVE").as_deref() == Some("true"),
            theme: env_str("ITSY_THEME").unwrap_or_else(|| "dark".into()),
            classic: false,
        },
        escalation: EscalationConfig {
            enabled: true,
            max_per_session: env_or("ITSY_ESCALATION_MAX", 5),
            confirm: env_str("ITSY_ESCALATION_CONFIRM").as_deref() != Some("false"),
            provider: None,
            api_key: None,
            model: env_str("ITSY_ESCALATION_MODEL"),
        },
        git: GitConfig {
            auto_commit: env_str("ITSY_AUTO_COMMIT").as_deref() == Some("true"),
        },
        features: FeaturesConfig {
            plan: env_str("ITSY_PLAN").as_deref() != Some("false"),
            snapshot: env_str("ITSY_SNAPSHOT").as_deref() != Some("false"),
            snapshot_auto_rollback: env_str("ITSY_SNAPSHOT_AUTO_ROLLBACK").as_deref() != Some("false"),
            write_guard: env_str("ITSY_WRITE_GUARD").as_deref() != Some("false"),
            bootstrap: env_str("ITSY_BOOTSTRAP").as_deref() != Some("false"),
            trust_decay: env_str("ITSY_TRUST_DECAY").as_deref() != Some("false"),
            temp_adapt: env_str("ITSY_TEMP_ADAPT").as_deref() != Some("false"),
            thinking_budget: env_or("ITSY_THINKING_BUDGET", 0u32),
            clarifier: env_str("ITSY_CLARIFIER").as_deref() != Some("false"),
            semantic_merge: env_str("ITSY_SEMANTIC_MERGE").as_deref() != Some("false"),
            error_diagnosis: env_str("ITSY_ERROR_DIAGNOSIS").as_deref() != Some("false"),
            validate_edits: env_str("ITSY_VALIDATE_EDITS").as_deref() == Some("true"),
            context_retrieval: env_str("ITSY_CONTEXT_RETRIEVAL").as_deref() != Some("false"),
            reviewer: env_str("ITSY_REVIEWER").as_deref() == Some("true"),
            chain: env_str("ITSY_CHAIN").as_deref() == Some("true"),
        },
        models: None,
    };

    if env_str("ITSY_MODEL_FAST").is_some() || env_str("ITSY_MODEL_STRONG").is_some() {
        config.models = Some(MultiModels {
            fast: env_str("ITSY_MODEL_FAST").unwrap_or_else(|| config.model.name.clone()),
            default: env_str("ITSY_MODEL_DEFAULT").unwrap_or_else(|| config.model.name.clone()),
            strong: env_str("ITSY_MODEL_STRONG").unwrap_or_else(|| config.model.name.clone()),
        });
    }

    // Merge TOML file on top of env-derived defaults (env still wins on
    // individual fields below; we apply TOML before env-overrides at the
    // top of this function — but env_str() was already evaluated, so we
    // walk back through it after merge for keys that have ITSY_* overrides).
    let toml_path = crate::paths::config_file();
    if toml_path.exists() {
        if let Ok(file) = ConfigFile::load_from_path(&toml_path) {
            file.apply_to(&mut config);
            // Env vars still take precedence over the TOML file. Re-apply
            // any that are actually set.
            if let Some(v) = env_str("ITSY_PROVIDER") {
                config.model.provider = v;
            }
            if let Some(v) = env_str("ITSY_MODEL") {
                config.model.name = v;
            }
            if let Some(v) = env_str("ITSY_BASE_URL") {
                config.model.base_url = v;
            }
        }
    }

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

    // Push TOML-configured feature toggles into ITSY_* env vars so the
    // many env-var consumers (snapshot, write_guard, plan_tracker, …)
    // pick them up automatically. Env vars that are already set are
    // left alone, preserving the documented load order.
    propagate_features_to_env(&config);

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

fn extract_value(line: &str) -> Option<String> {
    let after_eq = line.find('=')?;
    let mut v = line[after_eq + 1..].trim();
    if let Some(idx) = v.find('#') {
        v = v[..idx].trim();
    }
    let v = v.trim_matches(|c| c == '"' || c == '\'').trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
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
