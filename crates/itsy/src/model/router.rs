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
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static FAST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)\b(fix typo|rename|add comment|format|lint)\b",
        r"(?i)\b(what is|explain|show me|read)\b",
        r"(?i)\b(simple|quick|small|minor)\b",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
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
