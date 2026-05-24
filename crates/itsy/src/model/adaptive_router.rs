//! Adaptive model router (port of `src/model/adaptive_router.js`).
//!
//! Tracks per-model success/failure rates and routes traffic to stronger
//! models when the primary's reliability drops. This Rust port extends the
//! JS original with:
//!
//! - **Persistence**: scores are saved to disk so learning survives restarts.
//! - **Per-task-class learning**: each task type (`backend`, `explanation`,
//!   `multi_step`, …) keeps its own stats.
//! - **Confidence intervals**: Wilson lower-bound is used to avoid acting on
//!   tiny sample sizes.
//! - **Exploration vs exploitation**: epsilon-greedy with a configurable
//!   epsilon (`ITSY_ADAPTIVE_EPSILON`, default 0.1).
//! - **Decay of stale data**: a configurable half-life
//!   (`ITSY_ADAPTIVE_DECAY_SECS`) gently fades old observations so a model
//!   that used to fail can recover.
//!
//! Environment variables:
//!   ITSY_MODEL_STRONG          — strong model (e.g. `gpt-4o`)
//!   ITSY_MODEL_MEDIUM          — medium model (e.g. `qwen2.5-coder:32b`)
//!   ITSY_ADAPTIVE_EPSILON      — exploration probability (default 0.1)
//!   ITSY_ADAPTIVE_DECAY_SECS   — decay half-life in seconds (default 86400)
//!   ITSY_ADAPTIVE_STATE_PATH   — override on-disk state path
//!
//! Thresholds (matching the JS original, applied on the Wilson lower bound):
//!   failure-rate > 0.6 → escalate to `ITSY_MODEL_STRONG` (if set)
//!   failure-rate > 0.3 → escalate to `ITSY_MODEL_MEDIUM` (if set)
//!   otherwise          → use `config.model.name`
//!
//! At least 3 observations are required before any routing decision (avoids
//! thrashing on the very first failure).

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::Config;

const DEFAULT_DECAY_SECS: f64 = 86_400.0; // one day half-life
const DEFAULT_EPSILON: f64 = 0.10;
const MIN_CALLS_FOR_DECISION: f64 = 3.0;

/// Persisted per-(task_type, model) statistics.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    /// Effective (decay-weighted) success count.
    #[serde(default)]
    pub successes: f64,
    /// Effective (decay-weighted) failure count.
    #[serde(default)]
    pub fails: f64,
    /// Effective total calls (successes + fails after decay).
    #[serde(default)]
    pub calls: f64,
    /// Unix epoch seconds of the most recent update.
    #[serde(default)]
    pub last_update: u64,
}

impl ModelStats {
    /// Apply exponential decay to the stored counts based on elapsed time.
    fn decay(&mut self, now: u64, half_life_secs: f64) {
        if self.last_update == 0 || self.calls == 0.0 {
            self.last_update = now;
            return;
        }
        let elapsed = now.saturating_sub(self.last_update) as f64;
        if elapsed <= 0.0 || half_life_secs <= 0.0 {
            return;
        }
        let factor = 0.5_f64.powf(elapsed / half_life_secs);
        self.successes *= factor;
        self.fails *= factor;
        self.calls = self.successes + self.fails;
        self.last_update = now;
    }

    pub fn failure_rate(&self) -> f64 {
        if self.calls <= 0.0 {
            return 0.0;
        }
        self.fails / self.calls
    }

    pub fn success_rate(&self) -> f64 {
        if self.calls <= 0.0 {
            return 0.0;
        }
        self.successes / self.calls
    }

    /// Wilson score lower bound (95%) on the failure rate, used so we don't
    /// escalate based on tiny sample sizes.
    pub fn failure_rate_lower_bound(&self) -> f64 {
        wilson_lower_bound(self.fails, self.calls, 1.96)
    }
}

/// Full persisted state — outer map keyed by task_type, inner by model name.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AdaptiveRouterState {
    /// `task_type -> (model_name -> stats)`.
    #[serde(default)]
    pub tier_scores: HashMap<String, HashMap<String, ModelStats>>,
}

/// Adaptive router with per-task-class learning, decay, and persistence.
#[derive(Debug, Default)]
pub struct AdaptiveRouter {
    state: Mutex<AdaptiveRouterState>,
    half_life_secs: f64,
    epsilon: f64,
    state_path: Option<PathBuf>,
}

impl AdaptiveRouter {
    /// Build a fresh in-memory router (no persistence).
    pub fn new() -> Self {
        Self {
            state: Mutex::new(AdaptiveRouterState::default()),
            half_life_secs: env_f64("ITSY_ADAPTIVE_DECAY_SECS", DEFAULT_DECAY_SECS),
            epsilon: env_f64("ITSY_ADAPTIVE_EPSILON", DEFAULT_EPSILON).clamp(0.0, 1.0),
            state_path: None,
        }
    }

    /// Build a router that loads from / saves to `path`. If the file does
    /// not exist (or is corrupt), the router starts empty.
    pub fn with_persistence(path: PathBuf) -> Self {
        let state = load_state(&path).unwrap_or_default();
        Self {
            state: Mutex::new(state),
            half_life_secs: env_f64("ITSY_ADAPTIVE_DECAY_SECS", DEFAULT_DECAY_SECS),
            epsilon: env_f64("ITSY_ADAPTIVE_EPSILON", DEFAULT_EPSILON).clamp(0.0, 1.0),
            state_path: Some(path),
        }
    }

    /// Default on-disk path: `$ITSY_ADAPTIVE_STATE_PATH` or
    /// `<config_dir>/adaptive_router.json`.
    pub fn default_state_path() -> PathBuf {
        if let Some(p) = env::var("ITSY_ADAPTIVE_STATE_PATH").ok().filter(|s| !s.is_empty()) {
            return PathBuf::from(p);
        }
        crate::paths::config_dir().join("adaptive_router.json")
    }

    /// Record the outcome of a chat-completion call.
    pub fn record_call(&self, task_type: &str, model_name: &str, success: bool) {
        if model_name.is_empty() {
            return;
        }
        let now = now_secs();
        {
            let mut state = self.state.lock().expect("adaptive router state poisoned");
            let inner = state
                .tier_scores
                .entry(task_type.to_string())
                .or_default();
            let entry = inner.entry(model_name.to_string()).or_default();
            entry.decay(now, self.half_life_secs);
            if success {
                entry.successes += 1.0;
            } else {
                entry.fails += 1.0;
            }
            entry.calls = entry.successes + entry.fails;
            entry.last_update = now;
        }
        self.persist();
    }

    /// Backwards-compatible alias.
    pub fn record(&self, task_type: &str, tier: &str, success: bool) {
        self.record_call(task_type, tier, success);
    }

    /// Return the decay-adjusted failure rate for `model_name` under
    /// `task_type` (0.0 if unknown).
    pub fn failure_rate(&self, task_type: &str, model_name: &str) -> f64 {
        let now = now_secs();
        let mut state = self.state.lock().expect("adaptive router state poisoned");
        let Some(inner) = state.tier_scores.get_mut(task_type) else {
            return 0.0;
        };
        let Some(entry) = inner.get_mut(model_name) else {
            return 0.0;
        };
        entry.decay(now, self.half_life_secs);
        entry.failure_rate()
    }

    /// Pick the best-scoring model for `task_type`, or `None` if no learning
    /// signal exists yet.
    pub fn best_tier(&self, task_type: &str) -> Option<String> {
        let now = now_secs();
        let mut state = self.state.lock().expect("adaptive router state poisoned");
        let inner = state.tier_scores.get_mut(task_type)?;
        // Decay before comparing.
        for entry in inner.values_mut() {
            entry.decay(now, self.half_life_secs);
        }
        inner
            .iter()
            .filter(|(_, s)| s.calls >= MIN_CALLS_FOR_DECISION)
            .max_by(|a, b| {
                a.1.success_rate()
                    .partial_cmp(&b.1.success_rate())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(k, _)| k.clone())
    }

    /// Select the model to use for `task_type` given the configured primary.
    /// Implements the JS thresholds with Wilson lower-bound smoothing plus
    /// epsilon-greedy exploration.
    pub fn select_model(&self, config: &Config, task_type: &str) -> ModelChoice {
        let primary_model = config.model.name.clone();
        let primary_url = config.model.base_url.clone();
        if primary_model.is_empty() {
            return ModelChoice { model: primary_model, url: primary_url };
        }

        // Epsilon-greedy: occasionally pick a non-primary tier to explore.
        if self.epsilon > 0.0 {
            let roll: f64 = rand::thread_rng().r#gen();
            if roll < self.epsilon {
                if let Some(explore) = self.explore_choice(&primary_model) {
                    return ModelChoice { model: explore, url: primary_url };
                }
            }
        }

        let stats = {
            let now = now_secs();
            let mut state = self.state.lock().expect("adaptive router state poisoned");
            state
                .tier_scores
                .get_mut(task_type)
                .and_then(|inner| inner.get_mut(&primary_model))
                .map(|entry| {
                    entry.decay(now, self.half_life_secs);
                    entry.clone()
                })
        };

        let Some(stats) = stats else {
            return ModelChoice { model: primary_model, url: primary_url };
        };
        if stats.calls < MIN_CALLS_FOR_DECISION {
            return ModelChoice { model: primary_model, url: primary_url };
        }

        let rate = stats.failure_rate_lower_bound();

        if rate > 0.6 {
            if let Some(strong) = env::var("ITSY_MODEL_STRONG").ok().filter(|s| !s.is_empty()) {
                if strong != primary_model {
                    return ModelChoice { model: strong, url: primary_url };
                }
            }
        }
        if rate > 0.3 {
            if let Some(medium) = env::var("ITSY_MODEL_MEDIUM").ok().filter(|s| !s.is_empty()) {
                if medium != primary_model {
                    return ModelChoice { model: medium, url: primary_url };
                }
            }
        }
        ModelChoice { model: primary_model, url: primary_url }
    }

    /// Reset all tracked stats (called on session start / test runs).
    pub fn reset(&self) {
        {
            let mut state = self.state.lock().expect("adaptive router state poisoned");
            state.tier_scores.clear();
        }
        self.persist();
    }

    /// Snapshot of the current state (used for inspection and persistence).
    pub fn snapshot(&self) -> AdaptiveRouterState {
        self.state.lock().expect("adaptive router state poisoned").clone()
    }

    fn explore_choice(&self, primary: &str) -> Option<String> {
        let candidates = [
            env::var("ITSY_MODEL_STRONG").ok(),
            env::var("ITSY_MODEL_MEDIUM").ok(),
        ];
        let alts: Vec<String> = candidates
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty() && s != primary)
            .collect();
        if alts.is_empty() {
            return None;
        }
        let idx = rand::thread_rng().gen_range(0..alts.len());
        Some(alts[idx].clone())
    }

    fn persist(&self) {
        let Some(path) = self.state_path.as_ref() else {
            return;
        };
        let snapshot = self.snapshot();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(serialized) = serde_json::to_string_pretty(&snapshot) {
            let _ = fs::write(path, serialized);
        }
    }
}

/// Selected model + URL pair (mirrors the JS `{model, url}` shape).
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub model: String,
    pub url: String,
}

// ─── Singleton ────────────────────────────────────────────────────────────

static GLOBAL_ROUTER: Lazy<AdaptiveRouter> =
    Lazy::new(|| AdaptiveRouter::with_persistence(AdaptiveRouter::default_state_path()));

/// Process-wide adaptive router (lazy-loaded from disk on first use).
pub fn get_adaptive_router() -> &'static AdaptiveRouter {
    &GLOBAL_ROUTER
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn load_state(path: &PathBuf) -> Option<AdaptiveRouterState> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Wilson score lower bound for a binomial proportion `successes / n` at
/// confidence `z` (1.96 ≈ 95%). Returns 0.0 when `n` is zero.
fn wilson_lower_bound(successes: f64, n: f64, z: f64) -> f64 {
    if n <= 0.0 {
        return 0.0;
    }
    let phat = successes / n;
    let denom = 1.0 + z * z / n;
    let centre = phat + z * z / (2.0 * n);
    let margin = z * ((phat * (1.0 - phat) + z * z / (4.0 * n)) / n).sqrt();
    ((centre - margin) / denom).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_config() -> Config {
        // Build a minimal Config the same way config::load would, but without
        // touching the filesystem or env. We just need model.name + base_url.
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        Config {
            model: ModelConfig {
                provider: "openai".into(),
                name: "qwen2.5-coder:8b".into(),
                base_url: "http://localhost:1234/v1".into(),
                timeout: 60,
                api_key: None,
            },
            context: ContextConfig {
                max_budget_pct: 70,
                detected_window: 32_768,
                working_memory_tokens: 8192,
                summary_threshold: 8000,
            },
            tools: ToolsConfig {
                bash_timeout: 30,
                tool_routing: "direct".into(),
                web_browse: false,
                shell_persist: true,
                shell_contain: false,
                rtk: true,
            },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: crate::config::FeaturesConfig::default(),
            models: None,
            limits: crate::config::LimitsConfig::default(),
            security: crate::config::SecurityConfig::default(),
            diff: crate::config::DiffConfig::default(),
            filetree: crate::config::FileTreeConfig::default(),
            snapshots: crate::config::SnapshotPathsConfig::default(),
            code_graph: crate::config::CodeGraphConfig::default(),
            tests: crate::config::TestsConfig::default(),
            traces: crate::config::TracesConfig::default(),
            dedup: crate::config::DedupConfig::default(),
            evidence: crate::config::EvidenceConfig::default(),
            plugins: crate::config::PluginsConfig::default(),
            diag: crate::config::DiagConfig::default(),
            second_opinion: crate::config::SecondOpinionConfig::default(),
        }
    }

    #[test]
    fn record_and_failure_rate() {
        let r = AdaptiveRouter::new();
        for _ in 0..3 {
            r.record_call("backend", "m1", false);
        }
        r.record_call("backend", "m1", true);
        let fr = r.failure_rate("backend", "m1");
        assert!(fr > 0.5 && fr < 1.0, "got {fr}");
    }

    #[test]
    fn best_tier_picks_higher_success_rate() {
        let r = AdaptiveRouter::new();
        for _ in 0..5 { r.record_call("explanation", "fast", true); }
        for _ in 0..5 { r.record_call("explanation", "slow", false); }
        assert_eq!(r.best_tier("explanation").as_deref(), Some("fast"));
    }

    #[test]
    fn select_model_needs_minimum_calls() {
        let r = AdaptiveRouter::new();
        let c = empty_config();
        // No data → primary.
        let choice = r.select_model(&c, "backend");
        assert_eq!(choice.model, c.model.name);
        // One failure isn't enough to escalate.
        r.record_call("backend", &c.model.name, false);
        let choice = r.select_model(&c, "backend");
        assert_eq!(choice.model, c.model.name);
    }

    #[test]
    fn wilson_bound_sane() {
        // 100% failure on 1 sample should not say "definitely fails".
        let lb = wilson_lower_bound(1.0, 1.0, 1.96);
        assert!(lb < 0.5, "tiny sample shouldn't escalate: {lb}");
        // 100% failure on a big sample should say "very likely fails".
        let lb = wilson_lower_bound(95.0, 100.0, 1.96);
        assert!(lb > 0.85, "big sample should be confident: {lb}");
    }

    #[test]
    fn persistence_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("router.json");
        {
            let r = AdaptiveRouter::with_persistence(path.clone());
            for _ in 0..4 { r.record_call("backend", "m1", false); }
            r.record_call("backend", "m1", true);
        }
        // Reload — stats should still be there.
        let r2 = AdaptiveRouter::with_persistence(path);
        assert!(r2.failure_rate("backend", "m1") > 0.5);
    }

    #[test]
    fn reset_clears_state() {
        let r = AdaptiveRouter::new();
        r.record_call("x", "m", false);
        r.reset();
        assert_eq!(r.failure_rate("x", "m"), 0.0);
    }
}
