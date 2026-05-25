//! Multi-Model Router.
//!
//! Auto-picks a model name based on task complexity when multiple models are
//! configured. The tiered config (`models.fast` / `default` / `strong` /
//! `escalation`) lives in `itsy.toml`.
//!
//! Two entry points:
//!
//! * [`estimate_complexity`] inspects the user message and returns one of
//!   `"fast"`, `"default"`, or `"strong"`.
//! * [`route_model`] reconciles the estimated complexity (or an explicit
//!   task-type hint) with the configured tiers and returns the chosen model
//!   name.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    Fast,
    Default,
    Strong,
}

impl Complexity {
    pub fn as_str(self) -> &'static str {
        match self {
            Complexity::Fast => "fast",
            Complexity::Default => "default",
            Complexity::Strong => "strong",
        }
    }
}

static STRONG_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)\b(refactor|redesign|architect|rewrite|migrate|convert)\b",
        r"(?i)\b(multi.?file|multiple files|across files|all files)\b",
        r"(?i)\b(system|framework|infrastructure|full.?stack)\b",
        r"(?i)\b(test suite|integration test|e2e)\b",
        r"(?i)\b(and then|step \d|first.*then.*finally)\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid regex literal"))
    .collect()
});

static FAST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)\b(fix typo|rename|add comment|format|lint)\b",
        r"(?i)\b(what is|explain|show me|read)\b",
        r"(?i)\b(simple|quick|small|minor)\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid regex literal"))
    .collect()
});

/// Estimate task complexity from the user message.
pub fn estimate_complexity(message: &str) -> Complexity {
    let msg = message.to_lowercase();
    let len = msg.chars().count();

    if STRONG_PATTERNS.iter().any(|re| re.is_match(&msg)) || len > 500 {
        return Complexity::Strong;
    }

    if FAST_PATTERNS.iter().any(|re| re.is_match(&msg)) && len < 100 {
        return Complexity::Fast;
    }

    Complexity::Default
}

/// Pick the model name based on configured tiers and estimated complexity.
///
/// Mirrors the JS `routeModel(message, config)` helper but accepts a single
/// `message`. If no multi-model config is present, falls back to
/// `config.model.name`.
pub fn route_model_for_message(message: &str, config: &Config) -> String {
    let Some(models) = &config.models else {
        return config.model.name.clone();
    };

    let complexity = estimate_complexity(message);
    match complexity {
        Complexity::Fast => first_non_empty(&[&models.fast, &models.default, &config.model.name]),
        Complexity::Strong => first_non_empty(&[&models.strong, &models.default, &config.model.name]),
        Complexity::Default => first_non_empty(&[&models.default, &config.model.name]),
    }
}

fn first_non_empty(opts: &[&String]) -> String {
    for o in opts {
        if !o.is_empty() {
            return (*o).clone();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Backwards-compatible task-type router kept for existing callers
// (`cognition_adapter`, `prompts`, etc).
// ---------------------------------------------------------------------------

/// Pick a model name from configured tiers based on a coarse task-type tag.
pub fn route_model(config: &Config, task_type: &str) -> String {
    let Some(models) = &config.models else {
        return config.model.name.clone();
    };
    match task_type {
        "explanation" | "search" => models.fast.clone(),
        "backend" | "multi_step" | "debugging" => models.strong.clone(),
        _ => models.default.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refactor/redesign signals route to `strong`.
    #[test]
    fn refactor_signals_strong() {
        assert_eq!(estimate_complexity("refactor the auth module"), Complexity::Strong);
        assert_eq!(estimate_complexity("rewrite the parser"), Complexity::Strong);
        assert_eq!(estimate_complexity("redesign the database schema"), Complexity::Strong);
    }

    /// Multi-file signals route to `strong`.
    #[test]
    fn multi_file_signals_strong() {
        assert_eq!(estimate_complexity("change this across files"), Complexity::Strong);
        assert_eq!(estimate_complexity("multi-file refactor"), Complexity::Strong);
    }

    /// Very long messages (>500 chars) route to `strong` regardless of content.
    /// Anti-regression: a complex request shouldn't slip through to `fast`
    /// just because it lacks specific trigger keywords.
    #[test]
    fn very_long_message_routes_to_strong() {
        let msg = "do something ".repeat(50); // ~650 chars
        assert_eq!(estimate_complexity(&msg), Complexity::Strong);
    }

    /// Quick/small intents route to `fast` — but only when the message is short.
    #[test]
    fn fast_intents_route_to_fast_when_short() {
        assert_eq!(estimate_complexity("fix typo in readme"), Complexity::Fast);
        assert_eq!(estimate_complexity("explain this"), Complexity::Fast);
        assert_eq!(estimate_complexity("what is X"), Complexity::Fast);
    }

    /// Fast-keyword + long message → still `default`, not `fast`.
    /// Avoids a "explain everything about the entire codebase" → fast misroute.
    #[test]
    fn fast_keyword_long_message_routes_default() {
        let msg = format!("explain this thoroughly {}", "and ".repeat(40));
        assert_eq!(estimate_complexity(&msg), Complexity::Default,
            "long messages must not route to fast just because they contain a fast keyword");
    }

    /// Unmatched messages default to `default`.
    #[test]
    fn unmatched_routes_to_default() {
        assert_eq!(estimate_complexity("add a feature to handle x"), Complexity::Default);
    }

    /// `route_model_for_message` falls back to the base model when no tiered
    /// config is set. This is the single-model deployment path.
    #[test]
    fn route_model_falls_back_to_single_model_when_no_tiers() {
        use crate::config::ModelConfig;
        let mut cfg = make_test_config();
        cfg.model = ModelConfig {
            provider: "openai".into(),
            name: "only-model".into(),
            base_url: "http://localhost".into(),
            timeout: 60,
            api_key: None,
        };
        cfg.models = None;
        assert_eq!(route_model_for_message("anything", &cfg), "only-model");
    }

    /// When tiers are configured, message routes to the right tier.
    #[test]
    fn route_model_picks_correct_tier_from_complexity() {
        use crate::config::{ModelConfig, MultiModels};
        let mut cfg = make_test_config();
        cfg.model = ModelConfig {
            provider: "openai".into(),
            name: "fallback".into(),
            base_url: "http://localhost".into(),
            timeout: 60,
            api_key: None,
        };
        cfg.models = Some(MultiModels {
            fast: "fast-m".into(),
            default: "default-m".into(),
            strong: "strong-m".into(),
        });
        assert_eq!(route_model_for_message("refactor everything", &cfg), "strong-m");
        assert_eq!(route_model_for_message("fix typo", &cfg), "fast-m");
        assert_eq!(route_model_for_message("add a feature", &cfg), "default-m");
    }

    /// If a tier has an empty model name, route falls through to the next
    /// non-empty option — never returns an empty string when any tier is set.
    #[test]
    fn route_model_falls_through_empty_tier() {
        use crate::config::{ModelConfig, MultiModels};
        let mut cfg = make_test_config();
        cfg.model = ModelConfig {
            provider: "openai".into(),
            name: "fallback".into(),
            base_url: "http://localhost".into(),
            timeout: 60,
            api_key: None,
        };
        cfg.models = Some(MultiModels {
            fast: "".into(),  // empty → must fall through
            default: "default-m".into(),
            strong: "".into(), // empty → must fall through
        });
        // fast empty → default
        assert_eq!(route_model_for_message("fix typo", &cfg), "default-m");
        // strong empty → default
        assert_eq!(route_model_for_message("refactor everything", &cfg), "default-m");
    }

    fn make_test_config() -> Config {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        Config {
            model: ModelConfig {
                provider: "openai".into(),
                name: "x".into(),
                base_url: "http://localhost".into(),
                timeout: 60,
                api_key: None,
            },
            context: ContextConfig {
                max_budget_pct: 70, detected_window: 32_768,
                working_memory_tokens: 8192, summary_threshold: 8000,
            },
            tools: ToolsConfig {
                bash_timeout: 30, tool_routing: "direct".into(),
                web_browse: false, shell_persist: true, shell_contain: false, rtk: true,
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
}
