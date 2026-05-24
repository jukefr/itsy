//! Per-Tool Trust Score Decay.
//!
//! Tracks consecutive-failure counts per tool (optionally per task-type) and
//! provides predicates the router uses to demote or drop unreliable tools.
//!
//! Port of upstream `src/tools/trust_decay.js`. Rust additions beyond the JS:
//!   • per `(tool, task_type)` scoring slots (`record_combo` / `level_combo`)
//!   • disk persistence (`load` / `save`)
//!   • `should_avoid` / `summary` helpers
//!
//! Configuration:
//!   ITSY_TRUST_DECAY=false   disable entirely
//!   ITSY_TRUST_WARN=3        consecutive fails before soft-demote
//!   ITSY_TRUST_DROP=5        consecutive fails before hard-drop
//!   ITSY_TRUST_RESET=true    reset counter on any success (default true)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DEFAULT_WARN: u32 = 3;
const DEFAULT_DROP: u32 = 5;

/// Trust level for a tool (or tool+task-type combo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Ok,
    Warn,
    Drop,
}

/// One tracked slot — keyed by tool name or by `tool::task_type`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolScore {
    pub consecutive_fails: u32,
    pub total_fails: u32,
    pub total_calls: u32,
}


/// Per-session trust tracker with optional persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustState {
    pub disabled: bool,
    pub warn_threshold: u32,
    pub drop_threshold: u32,
    pub reset_on_success: bool,
    /// Per-tool scores. Keyed by `tool` for the simple API or by
    /// `tool::task_type` for the combo API.
    #[serde(default)]
    pub scores: HashMap<String, ToolScore>,
}

impl Default for TrustState {
    fn default() -> Self {
        Self {
            disabled: !env_bool("ITSY_TRUST_DECAY", true),
            warn_threshold: env_u32("ITSY_TRUST_WARN", DEFAULT_WARN),
            drop_threshold: env_u32("ITSY_TRUST_DROP", DEFAULT_DROP),
            reset_on_success: env_bool("ITSY_TRUST_RESET", true),
            scores: HashMap::new(),
        }
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// `key=false` returns false; everything else returns `default`.
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key).ok().as_deref() {
        Some("false") | Some("0") | Some("no") => false,
        Some("true") | Some("1") | Some("yes") => true,
        _ => default,
    }
}

fn key_for(tool: &str, task_type: Option<&str>) -> String {
    match task_type {
        Some(t) if !t.is_empty() => format!("{tool}::{t}"),
        _ => tool.to_string(),
    }
}

impl TrustState {
    pub fn new() -> Self { Self::default() }

    /// Record a tool outcome. Mirrors upstream `record(toolName, success)`.
    pub fn record(&mut self, tool: &str, success: bool) {
        self.record_combo(tool, None, success);
    }

    /// Record a tool outcome, optionally tagged by task type.
    pub fn record_combo(&mut self, tool: &str, task_type: Option<&str>, success: bool) {
        if self.disabled || tool.is_empty() { return; }
        let key = key_for(tool, task_type);
        let s = self.scores.entry(key).or_default();
        s.total_calls = s.total_calls.saturating_add(1);
        if success {
            if self.reset_on_success { s.consecutive_fails = 0; }
        } else {
            s.consecutive_fails = s.consecutive_fails.saturating_add(1);
            s.total_fails = s.total_fails.saturating_add(1);
        }
    }

    /// Trust level for a tool. Mirrors upstream `level(toolName)`.
    pub fn level(&self, tool: &str) -> TrustLevel {
        self.level_combo(tool, None)
    }

    pub fn level_combo(&self, tool: &str, task_type: Option<&str>) -> TrustLevel {
        if self.disabled { return TrustLevel::Ok; }
        let key = key_for(tool, task_type);
        let Some(s) = self.scores.get(&key) else { return TrustLevel::Ok; };
        if s.consecutive_fails >= self.drop_threshold { TrustLevel::Drop }
        else if s.consecutive_fails >= self.warn_threshold { TrustLevel::Warn }
        else { TrustLevel::Ok }
    }

    pub fn is_drop(&self, tool: &str) -> bool { matches!(self.level(tool), TrustLevel::Drop) }
    pub fn is_warn(&self, tool: &str) -> bool { matches!(self.level(tool), TrustLevel::Warn) }

    /// Both `Warn` and `Drop` signal avoidance.
    pub fn should_avoid(&self, tool: &str) -> bool {
        !matches!(self.level(tool), TrustLevel::Ok)
    }

    pub fn should_avoid_combo(&self, tool: &str, task_type: Option<&str>) -> bool {
        !matches!(self.level_combo(tool, task_type), TrustLevel::Ok)
    }

    /// Reset just one slot.
    pub fn reset_tool(&mut self, tool: &str) {
        self.scores.remove(tool);
    }

    /// Reset all state. Mirrors upstream `reset()`.
    pub fn reset(&mut self) { self.scores.clear(); }

    /// Brief one-line summary of demoted tools — `None` if nothing is demoted.
    pub fn summary(&self) -> Option<String> {
        let mut dropped: Vec<String> = Vec::new();
        let mut warned: Vec<String> = Vec::new();
        for (name, s) in &self.scores {
            if s.consecutive_fails >= self.drop_threshold {
                dropped.push(format!("{name}(x{})", s.consecutive_fails));
            } else if s.consecutive_fails >= self.warn_threshold {
                warned.push(format!("{name}(x{})", s.consecutive_fails));
            }
        }
        if dropped.is_empty() && warned.is_empty() { return None; }
        let mut parts: Vec<String> = Vec::new();
        if !dropped.is_empty() { parts.push(format!("dropped: {}", dropped.join(", "))); }
        if !warned.is_empty() { parts.push(format!("warned: {}", warned.join(", "))); }
        Some(parts.join("; "))
    }

    /// Filter a tool-name list: drop blacklisted tools, push warned ones to
    /// the back. Mirrors upstream `filterAndSort(tools)`.
    pub fn filter_and_sort<T: Clone, F: Fn(&T) -> &str>(&self, items: &[T], name_of: F) -> Vec<T> {
        if self.disabled { return items.to_vec(); }
        let mut ok: Vec<T> = Vec::new();
        let mut warned: Vec<T> = Vec::new();
        for it in items {
            match self.level(name_of(it)) {
                TrustLevel::Drop => continue,
                TrustLevel::Warn => warned.push(it.clone()),
                TrustLevel::Ok => ok.push(it.clone()),
            }
        }
        ok.extend(warned);
        ok
    }

    /// Load a previously-persisted state from disk. Returns a fresh default
    /// state if the file is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist state to disk (writes to a sibling `.tmp` then renames).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = with_tmp_suffix(path);
        let body = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    }
}

fn with_tmp_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[allow(clippy::field_reassign_with_default)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_thresholds() {
        let mut st = TrustState::default();
        
        st.warn_threshold = 2;
        st.drop_threshold = 4;
        for _ in 0..2 { st.record("foo", false); }
        assert_eq!(st.level("foo"), TrustLevel::Warn);
        for _ in 0..2 { st.record("foo", false); }
        assert_eq!(st.level("foo"), TrustLevel::Drop);
        st.record("foo", true);
        assert_eq!(st.level("foo"), TrustLevel::Ok);
    }

    #[test]
    fn should_avoid_and_summary() {
        let mut st = TrustState::default();
        
        st.warn_threshold = 1;
        st.drop_threshold = 99;
        st.record("bar", false);
        assert!(st.should_avoid("bar"));
        assert!(st.summary().is_some());
    }

    #[test]
    fn combo_keys_are_independent() {
        let mut st = TrustState::default();
        
        st.warn_threshold = 1;
        st.drop_threshold = 2;
        st.record_combo("grep", Some("search"), false);
        st.record_combo("grep", Some("search"), false);
        assert_eq!(st.level_combo("grep", Some("search")), TrustLevel::Drop);
        assert_eq!(st.level_combo("grep", Some("edit")), TrustLevel::Ok);
    }

    #[test]
    fn filter_and_sort_demotes_and_drops() {
        let mut st = TrustState::default();
        
        st.warn_threshold = 1;
        st.drop_threshold = 3;
        st.record("bad", false);
        for _ in 0..3 { st.record("dead", false); }
        let items = vec!["good", "bad", "dead"];
        let out = st.filter_and_sort(&items, |s| *s);
        assert_eq!(out, vec!["good", "bad"]);
    }

    #[test]
    fn round_trip_persistence() {
        let mut st = TrustState::default();
        
        st.record("foo", false);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trust.json");
        st.save(&path).unwrap();
        let loaded = TrustState::load(&path);
        assert!(loaded.scores.contains_key("foo"));
    }
}
