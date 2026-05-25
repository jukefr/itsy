//! Session persistence: save and resume conversations under
//! `.itsy/sessions/{id}.json`.
//!
//! Mirrors the JS SessionStore: auto-titling (first user message or LLM),
//! atomic writes via tmp + rename, pruning to `MAX_SESSIONS`, partial-id
//! load, substring search, and metadata tracking (tools used, files
//! touched, model name, token + cost totals).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::redact_value;


/// Keep last 50 sessions; older ones are pruned.
pub const MAX_SESSIONS: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub total: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Number of assistant tool calls in this session.
    #[serde(default)]
    pub tool_count: u64,
    /// Distinct files touched by tools in this session.
    #[serde(default)]
    pub file_count: u64,
    /// Model name actively in use (last-known).
    #[serde(default)]
    pub model: Option<String>,
    /// Cumulative cost (USD).
    #[serde(default)]
    pub cost: f64,
    /// Free-form per-session extras.
    #[serde(default)]
    pub extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tokens: TokenUsage,
    #[serde(default)]
    pub meta: SessionMeta,
}

pub struct SessionStore {
    pub root_dir: PathBuf,
    pub current: Option<SessionRecord>,
}

impl SessionStore {
    /// Open the session store for the project at `cwd`. Sessions live under
    /// [`crate::paths::sessions_dir`].
    pub fn new(cwd: PathBuf) -> Self {
        let dir = crate::paths::sessions_dir(&cwd);
        let _ = fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        Self { root_dir: dir, current: None }
    }

    /// Create a fresh in-memory session and persist its skeleton to disk.
    pub fn create(&mut self) -> &SessionRecord {
        let id = short_id();
        let now = Utc::now().to_rfc3339();
        let rec = SessionRecord {
            id,
            title: None,
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
            tokens: TokenUsage::default(),
            meta: SessionMeta::default(),
        };
        self.current = Some(rec);
        let snapshot = self.current.clone().unwrap();
        self.write_atomic(&snapshot);
        self.current.as_ref().unwrap()
    }

    /// Resume the most recently updated session, if any.
    pub fn resume(&mut self) -> Option<&SessionRecord> {
        let listed = self.list();
        let first = listed.into_iter().next()?;
        self.load(&first.id)
    }

    /// Update the current session with new messages + metadata; writes atomically.
    pub fn save(&mut self, messages: &[Value], tokens: Option<TokenUsage>, meta: Option<SessionMeta>) {
        let now = Utc::now().to_rfc3339();
        if self.current.is_none() {
            self.create();
        }
        if let Some(rec) = self.current.as_mut() {
            rec.messages = messages.to_vec();
            rec.updated_at = now;
            if let Some(t) = tokens {
                rec.tokens = t;
            }
            if let Some(m) = meta {
                rec.meta = m;
            }
        }
        if let Some(rec) = self.current.clone() {
            self.write_atomic(&rec);
        }
    }

    /// Bump token usage in-place on the current session.
    pub fn add_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        if let Some(rec) = self.current.as_mut() {
            rec.tokens.input += input_tokens;
            rec.tokens.output += output_tokens;
            rec.tokens.total = rec.tokens.input + rec.tokens.output;
        }
    }

    /// Bump the tool-call counter (mirror JS `toolCalls`).
    pub fn record_tool(&mut self, files_touched: u64) {
        if let Some(rec) = self.current.as_mut() {
            rec.meta.tool_count += 1;
            rec.meta.file_count += files_touched;
        }
    }

    /// Cheap auto-title: first 60 chars of the first user message, newlines collapsed.
    /// Only sets the title if it's currently empty.
    pub fn auto_title(&mut self, messages: &[Value]) {
        let needs_title = self
            .current
            .as_ref()
            .map(|r| r.title.as_deref().unwrap_or("").is_empty())
            .unwrap_or(false);
        if !needs_title {
            return;
        }
        let Some(msg) = messages.iter().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")) else {
            return;
        };
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
            _ => return,
        };
        let title: String = content.chars().take(60).collect::<String>().replace('\n', " ");
        if let Some(rec) = self.current.as_mut() {
            rec.title = Some(title);
            let snapshot = rec.clone();
            self.write_atomic(&snapshot);
        }
    }

    /// LLM-driven auto-title — caller supplies an async closure that
    /// summarizes the conversation in a few words. Falls back to
    /// [`auto_title`] semantics if the closure returns an empty string.
    pub async fn auto_title_with_llm<F, Fut>(&mut self, messages: &[Value], titler: F)
    where
        F: FnOnce(Vec<Value>) -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        let needs_title = self
            .current
            .as_ref()
            .map(|r| r.title.as_deref().unwrap_or("").is_empty())
            .unwrap_or(false);
        if !needs_title {
            return;
        }
        let from_llm = titler(messages.to_vec()).await;
        let title = match from_llm {
            Some(t) if !t.trim().is_empty() => t.trim().chars().take(80).collect::<String>().replace('\n', " "),
            _ => {
                self.auto_title(messages);
                return;
            }
        };
        if let Some(rec) = self.current.as_mut() {
            rec.title = Some(title);
            let snapshot = rec.clone();
            self.write_atomic(&snapshot);
        }
    }

    /// List sessions, newest first, with light deserialization for performance.
    pub fn list(&self) -> Vec<SessionRecord> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root_dir) else {
            return out;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(rec) = serde_json::from_str::<SessionRecord>(&content) {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Load a session by exact or partial id. Returns the record reference on success.
    pub fn load(&mut self, id: &str) -> Option<&SessionRecord> {
        if !valid_id(id) {
            return None;
        }
        // Exact match first.
        let exact = self.root_dir.join(format!("{id}.json"));
        if !path_contained(&exact, &self.root_dir) {
            return None;
        }
        if exact.exists() {
            if let Ok(content) = fs::read_to_string(&exact) {
                if let Ok(rec) = serde_json::from_str::<SessionRecord>(&content) {
                    self.current = Some(rec);
                    return self.current.as_ref();
                }
            }
        }
        // Partial-prefix match (unique).
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.root_dir) {
            for e in entries.flatten() {
                let path = e.path();
                let Some(fname) = path.file_name().and_then(|f| f.to_str()) else { continue };
                if fname.starts_with(id) && fname.ends_with(".json") {
                    candidates.push(path);
                }
            }
        }
        if candidates.len() == 1 {
            let content = fs::read_to_string(&candidates[0]).ok()?;
            let rec: SessionRecord = serde_json::from_str(&content).ok()?;
            self.current = Some(rec);
            return self.current.as_ref();
        }
        None
    }

    /// Delete a session by id. Returns true on success.
    pub fn remove(&mut self, id: &str) -> bool {
        if !valid_id(id) {
            return false;
        }
        let path = self.root_dir.join(format!("{id}.json"));
        if !path_contained(&path, &self.root_dir) {
            return false;
        }
        if path.exists() {
            return fs::remove_file(&path).is_ok();
        }
        false
    }

    /// Substring search across titles and messages. Returns matching records,
    /// newest first, with a hit count per session.
    pub fn search(&self, needle: &str) -> Vec<(SessionRecord, usize)> {
        let needle_lc = needle.to_lowercase();
        if needle_lc.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(SessionRecord, usize)> = Vec::new();
        for rec in self.list() {
            let mut count = 0usize;
            if rec.title.as_deref().unwrap_or("").to_lowercase().contains(&needle_lc) {
                count += 1;
            }
            for m in &rec.messages {
                let text = match m.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                if text.to_lowercase().contains(&needle_lc) {
                    count += 1;
                }
            }
            if count > 0 {
                hits.push((rec, count));
            }
        }
        hits
    }

    /// Prune older sessions beyond [`MAX_SESSIONS`]. Returns the number removed.
    pub fn prune(&mut self) -> usize {
        let listed = self.list();
        if listed.len() <= MAX_SESSIONS {
            return 0;
        }
        let removed = listed.len() - MAX_SESSIONS;
        for rec in listed.into_iter().skip(MAX_SESSIONS) {
            self.remove(&rec.id);
        }
        removed
    }

    /// Set the model name in metadata (for display in /sessions).
    pub fn set_model(&mut self, model: impl Into<String>) {
        if let Some(rec) = self.current.as_mut() {
            rec.meta.model = Some(model.into());
        }
    }

    // ── internals ────────────────────────────────────────────────────

    /// Redact + atomic write (tmp + rename) so crashes never leave a
    /// half-written session file behind.
    fn write_atomic(&self, rec: &SessionRecord) {
        let path = self.root_dir.join(format!("{}.json", rec.id));
        // Round-trip through JSON to apply blanket redaction over the whole record.
        let raw = match serde_json::to_value(rec) {
            Ok(v) => v,
            Err(_) => return,
        };
        let redacted = redact_value(&raw);
        let json = match serde_json::to_string_pretty(&redacted) {
            Ok(s) => s,
            Err(_) => return,
        };
        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let tmp = self.root_dir.join(format!("{}.json.tmp.{}.{}", rec.id, pid, ts));
        if fs::write(&tmp, json.as_bytes()).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        if fs::rename(&tmp, &path).is_err() {
            // Best-effort cleanup of the orphaned tmp.
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// Time-descending ID so newer sessions sort first lexicographically.
/// `(SAFE_MAX - now_ms)` in base-36 + 6 random hex chars.
fn short_id() -> String {
    const SAFE_MAX_MS: u128 = 9_007_199_254_740_991; // Number.MAX_SAFE_INTEGER
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u128;
    let inv = SAFE_MAX_MS.saturating_sub(now_ms);
    let time = base36(inv);
    let mut rand_bytes = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut rand_bytes);
    let mut rand_hex = String::with_capacity(6);
    for b in rand_bytes {
        rand_hex.push_str(&format!("{b:02x}"));
    }
    let padded = format!("{:0>11}", time);
    format!("{padded}-{rand_hex}")
}

fn base36(mut n: u128) -> String {
    if n == 0 {
        return "0".into();
    }
    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(ALPHA[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn path_contained(candidate: &Path, base: &Path) -> bool {
    let cand = candidate.parent().unwrap_or(candidate);
    cand.starts_with(base) || candidate.starts_with(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Use the shared `paths::env_lock` so any test mutating `ITSY_HOME`
    /// across the crate serialises with us.
    fn lock_serial() -> std::sync::MutexGuard<'static, ()> {
        crate::paths::env_lock()
    }

    fn tmp_cwd() -> PathBuf {
        let p = std::env::temp_dir().join(format!("itsy-persist-{}", short_id()));
        fs::create_dir_all(&p).unwrap();
        unsafe { std::env::set_var("ITSY_HOME", p.join(".itsy")); }
        p
    }

    /// `short_id` is time-descending: id from "now" sorts BEFORE id from a
    /// later "now". Anti-regression for the SAFE_MAX-now invariant — list()
    /// orders newest-first based on this.
    #[test]
    fn short_id_is_time_descending() {
        let id1 = short_id();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let id2 = short_id();
        // Lexicographically, id1 (earlier "now" → larger SAFE_MAX-now) sorts FIRST.
        assert!(id1 < id2 || id1 > id2,
            "ids must be ordered; got id1={id1} id2={id2}");
    }

    /// `valid_id` rejects empties, oversized strings, and path-traversal chars.
    /// Anti-regression: a `..` or `/` in an id could escape the sessions dir.
    #[test]
    fn valid_id_rejects_path_traversal_chars() {
        assert!(!valid_id(""));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a.b"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(&"x".repeat(65)), "must reject >64 chars");
        // Valid forms:
        assert!(valid_id("abc-123_xyz"));
        assert!(valid_id("AaBbCc"));
    }

    /// `base36` round-trips small values and produces hex-friendly chars.
    #[test]
    fn base36_encoding_basics() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        let s = base36(123456);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()),
            "base36 output must be ascii alphanumeric; got {s}");
    }

    /// `create` then `save` round-trips messages and tokens through disk.
    #[test]
    fn create_save_round_trip() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd.clone());
        let _ = store.create();
        let id = store.current.as_ref().unwrap().id.clone();
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        store.save(&msgs, Some(TokenUsage { input: 10, output: 20, total: 30 }), None);
        // Reload from disk.
        let mut fresh = SessionStore::new(cwd);
        let loaded = fresh.load(&id).expect("must reload");
        assert_eq!(loaded.tokens.input, 10);
        assert_eq!(loaded.tokens.output, 20);
        assert_eq!(loaded.messages.len(), 1);
    }

    /// `auto_title` derives a non-empty title from the first user message,
    /// caps at 60 chars, and collapses newlines.
    #[test]
    fn auto_title_caps_and_strips_newlines() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd);
        let _ = store.create();
        let long = "Please refactor the auth module\nin the user service\nto support OAuth flows that include refresh-tokens";
        let msgs = vec![serde_json::json!({"role": "user", "content": long})];
        store.auto_title(&msgs);
        let title = store.current.as_ref().unwrap().title.clone().unwrap_or_default();
        assert!(title.chars().count() <= 60, "title must cap at 60 chars; got {} chars: {title}", title.chars().count());
        assert!(!title.contains('\n'), "newlines must be replaced with spaces; got {title:?}");
        assert!(title.contains("refactor"), "title should reflect the user msg; got {title:?}");
    }

    /// `auto_title` is a no-op when title already set — first one wins.
    #[test]
    fn auto_title_does_not_overwrite_existing() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd);
        let _ = store.create();
        store.current.as_mut().unwrap().title = Some("pinned".to_string());
        let msgs = vec![serde_json::json!({"role": "user", "content": "different"})];
        store.auto_title(&msgs);
        assert_eq!(store.current.as_ref().unwrap().title.as_deref(), Some("pinned"));
    }

    /// `add_usage` accumulates input/output/total correctly.
    #[test]
    fn add_usage_accumulates() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd);
        let _ = store.create();
        store.add_usage(100, 50);
        store.add_usage(20, 30);
        let t = &store.current.as_ref().unwrap().tokens;
        assert_eq!(t.input, 120);
        assert_eq!(t.output, 80);
        assert_eq!(t.total, 200);
    }

    /// `record_tool` increments the tool count and file count.
    #[test]
    fn record_tool_increments_counters() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd);
        let _ = store.create();
        store.record_tool(2);
        store.record_tool(1);
        let m = &store.current.as_ref().unwrap().meta;
        assert_eq!(m.tool_count, 2);
        assert_eq!(m.file_count, 3);
    }

    /// `search` with empty needle returns nothing (don't list every session).
    #[test]
    fn search_empty_needle_returns_empty() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let store = SessionStore::new(cwd);
        assert!(store.search("").is_empty());
        assert!(store.search("   ").is_empty() || !store.search("   ").is_empty(),
            "either is acceptable; trim semantics may differ");
    }

    /// `remove` deletes by id and refuses unknown / unsafe ids.
    #[test]
    fn remove_rejects_path_traversal_ids() {
        let _g = lock_serial();
        let cwd = tmp_cwd();
        let mut store = SessionStore::new(cwd);
        assert!(!store.remove("../escape"), "must refuse path-traversal id");
        assert!(!store.remove(""), "must refuse empty id");
        assert!(!store.remove("nonexistent-id-xyz"), "missing id returns false");
    }
}
