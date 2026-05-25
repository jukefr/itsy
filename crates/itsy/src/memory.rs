//! Structured project memory store. Two backends:
//!
//! * **SQLite + FTS5** (preferred) — single `.itsy/memory.db` file, full-text
//!   search via the FTS5 virtual table, BM25-ranked retrieval. Ports the
//!   upstream `budget-aware-mcp` memory store semantics into pure Rust via
//!   `rusqlite` (bundled SQLite, FTS5 enabled).
//! * **JSON** (fallback) — `.itsy/memory/index.json` + markdown mirrors,
//!   keyword scoring. Only used when SQLite init fails.
//!
//! The public [`MemoryStore`] type enum-dispatches across both. Existing
//! callers see one struct with one set of methods regardless of which backend
//! is in use.

pub mod evidence;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::paths;

// ─── Public types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    #[serde(rename = "type")]
    pub kind: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryObject {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default)]
    pub source: Option<Source>,
}

impl MemoryObject {
    fn new(kind: impl Into<String>, title: impl Into<String>, content: impl Into<String>, tags: Vec<String>) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: short_id(),
            kind: kind.into(),
            title: title.into(),
            content: content.into(),
            tags,
            relations: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            source: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryStats {
    pub total: usize,
    pub by_type: HashMap<String, u32>,
    pub backend: &'static str,
}

// ─── Top-level enum-dispatched store ────────────────────────────────────────

pub struct MemoryStore {
    inner: Backend,
    /// Repo root the store is rooted at. Useful for diagnostics + callers
    /// that key state by workspace.
    pub root_dir: PathBuf,
}

enum Backend {
    Sqlite(SqliteBackend),
    Json(JsonBackend),
}

impl MemoryStore {
    /// Construct a memory store for the project rooted at `cwd`. Data lives
    /// under `paths::memory_db(cwd)` (SQLite) or `paths::project_dir(cwd)`
    /// (JSON fallback). Tries SQLite first; falls back to JSON if SQLite
    /// can't be opened.
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        let _ = paths::ensure_project_dirs(&cwd);
        let inner = match SqliteBackend::open(&cwd) {
            Ok(sqlite) => Backend::Sqlite(sqlite),
            Err(_) => Backend::Json(JsonBackend::new(&cwd)),
        };
        Self { inner, root_dir: cwd }
    }

    /// Force the JSON fallback regardless of SQLite availability. Used by
    /// tests + situations where the user explicitly asks for it.
    pub fn new_json_only(root_dir: impl AsRef<Path>) -> Self {
        let root = root_dir.as_ref().to_path_buf();
        Self { inner: Backend::Json(JsonBackend::new(&root)), root_dir: root }
    }

    pub fn backend_name(&self) -> &'static str {
        match self.inner {
            Backend::Sqlite(_) => "sqlite+fts5",
            Backend::Json(_) => "json",
        }
    }

    pub fn init(&mut self) -> bool {
        match &mut self.inner {
            Backend::Sqlite(_) => true, // schema already created in open()
            Backend::Json(j) => j.init(),
        }
    }

    pub fn remember(&mut self, kind: &str, title: &str, content: &str, tags: Vec<String>) -> MemoryObject {
        let obj = MemoryObject::new(kind, title, content, tags);
        match &mut self.inner {
            Backend::Sqlite(s) => {
                let _ = s.insert(&obj);
            }
            Backend::Json(j) => j.insert(obj.clone()),
        }
        obj
    }

    pub fn load_for_task(&self, task_description: &str) -> Vec<MemoryObject> {
        match &self.inner {
            Backend::Sqlite(s) => s.search(task_description, 5).unwrap_or_default(),
            Backend::Json(j) => j.search(task_description, 5),
        }
    }

    pub fn by_type(&self, kind: &str) -> Vec<MemoryObject> {
        match &self.inner {
            Backend::Sqlite(s) => s.by_type(kind).unwrap_or_default(),
            Backend::Json(j) => j.by_type(kind),
        }
    }

    pub fn all(&self) -> Vec<MemoryObject> {
        match &self.inner {
            Backend::Sqlite(s) => s.all().unwrap_or_default(),
            Backend::Json(j) => j.all(),
        }
    }

    pub fn get(&self, id: &str) -> Option<MemoryObject> {
        match &self.inner {
            Backend::Sqlite(s) => s.get(id).ok().flatten(),
            Backend::Json(j) => j.get(id),
        }
    }

    pub fn forget(&mut self, id: &str) -> bool {
        match &mut self.inner {
            Backend::Sqlite(s) => s.delete(id).unwrap_or(false),
            Backend::Json(j) => j.delete(id),
        }
    }

    pub fn format_for_context(&self, objects: &[MemoryObject], max_tokens: usize) -> String {
        format_for_context(objects, max_tokens)
    }

    pub fn stats(&self) -> MemoryStats {
        let (total, by_type, backend) = match &self.inner {
            Backend::Sqlite(s) => (s.count().unwrap_or(0), s.stats().unwrap_or_default(), "sqlite+fts5"),
            Backend::Json(j) => (j.count(), j.stats(), "json"),
        };
        MemoryStats { total, by_type, backend }
    }
}

fn format_for_context(objects: &[MemoryObject], max_tokens: usize) -> String {
    if objects.is_empty() {
        return String::new();
    }
    let mut out = String::from("<memory>\n");
    let mut tokens = 0;
    for obj in objects {
        let entry = format!("[{}] {}: {}\n", obj.kind, obj.title, obj.content);
        let entry_tokens = entry.len().div_ceil(4);
        if tokens + entry_tokens > max_tokens {
            break;
        }
        out.push_str(&entry);
        tokens += entry_tokens;
    }
    out.push_str("</memory>");
    out
}

// ─── SQLite + FTS5 backend ──────────────────────────────────────────────────

struct SqliteBackend {
    conn: Mutex<Connection>,
}

impl SqliteBackend {
    fn open(cwd: &Path) -> rusqlite::Result<Self> {
        let db_path = paths::memory_db(cwd);
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Verify FTS5 is available — bundled rusqlite ships with it, but a
        // mis-built environment could be missing it. If FTS5 init fails the
        // caller will fall back to the JSON backend.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_objects (
                id          TEXT PRIMARY KEY,
                type        TEXT NOT NULL,
                title       TEXT NOT NULL,
                content     TEXT NOT NULL,
                tags        TEXT NOT NULL DEFAULT '[]',
                relations   TEXT NOT NULL DEFAULT '[]',
                source      TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mem_type ON memory_objects(type);
            CREATE INDEX IF NOT EXISTS idx_mem_updated ON memory_objects(updated_at);

            CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
                id UNINDEXED,
                type UNINDEXED,
                title,
                content,
                tags,
                tokenize = 'porter unicode61 remove_diacritics 1'
            );

            CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_objects BEGIN
                INSERT INTO memory_fts(rowid, id, type, title, content, tags)
                VALUES (new.rowid, new.id, new.type, new.title, new.content, new.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_objects BEGIN
                DELETE FROM memory_fts WHERE rowid = old.rowid;
            END;
            CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_objects BEGIN
                DELETE FROM memory_fts WHERE rowid = old.rowid;
                INSERT INTO memory_fts(rowid, id, type, title, content, tags)
                VALUES (new.rowid, new.id, new.type, new.title, new.content, new.tags);
            END;
            "#,
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn insert(&mut self, obj: &MemoryObject) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        let tags_json = serde_json::to_string(&obj.tags).unwrap_or_else(|_| "[]".into());
        let relations_json = serde_json::to_string(&obj.relations).unwrap_or_else(|_| "[]".into());
        let source_json = obj.source.as_ref().and_then(|s| serde_json::to_string(s).ok());
        conn.execute(
            "INSERT OR REPLACE INTO memory_objects (id, type, title, content, tags, relations, source, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                obj.id,
                obj.kind,
                obj.title,
                obj.content,
                tags_json,
                relations_json,
                source_json,
                obj.created_at,
                obj.updated_at,
            ],
        )?;
        Ok(())
    }

    fn delete(&mut self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM memory_objects WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    fn get(&self, id: &str) -> rusqlite::Result<Option<MemoryObject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, type, title, content, tags, relations, source, created_at, updated_at
             FROM memory_objects WHERE id = ?1",
        )?;
        let obj = stmt.query_row(params![id], row_to_object).optional()?;
        Ok(obj)
    }

    fn all(&self) -> rusqlite::Result<Vec<MemoryObject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, type, title, content, tags, relations, source, created_at, updated_at
             FROM memory_objects ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_object)?;
        rows.collect()
    }

    fn by_type(&self, kind: &str) -> rusqlite::Result<Vec<MemoryObject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, type, title, content, tags, relations, source, created_at, updated_at
             FROM memory_objects WHERE type = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![kind], row_to_object)?;
        rows.collect()
    }

    fn count(&self) -> rusqlite::Result<usize> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM memory_objects", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    fn stats(&self) -> rusqlite::Result<HashMap<String, u32>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT type, COUNT(*) FROM memory_objects GROUP BY type")?;
        let rows = stmt.query_map([], |r| {
            let t: String = r.get(0)?;
            let c: i64 = r.get(1)?;
            Ok((t, c as u32))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (t, c) = row?;
            out.insert(t, c);
        }
        Ok(out)
    }

    /// FTS5 full-text search. Falls back to LIKE on title/tags if the FTS
    /// query is empty or returns no results — this matches the
    /// budget-aware-mcp semantics of "always try to surface something
    /// relevant rather than nothing."
    fn search(&self, task: &str, limit: usize) -> rusqlite::Result<Vec<MemoryObject>> {
        let query = build_fts_query(task);
        if !query.is_empty() {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.type, m.title, m.content, m.tags, m.relations, m.source,
                        m.created_at, m.updated_at
                 FROM memory_objects m
                 JOIN memory_fts f ON f.rowid = m.rowid
                 WHERE memory_fts MATCH ?1
                 ORDER BY bm25(memory_fts), m.updated_at DESC
                 LIMIT ?2",
            )?;
            let rows: Vec<MemoryObject> = stmt
                .query_map(params![query, limit as i64], row_to_object)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !rows.is_empty() {
                return Ok(rows);
            }
        }
        // Fallback: substring match on title / content / tags.
        let mut hits = Vec::new();
        let conn = self.conn.lock();
        let needle = format!("%{}%", task.replace(['%', '_'], ""));
        let mut stmt = conn.prepare(
            "SELECT id, type, title, content, tags, relations, source, created_at, updated_at
             FROM memory_objects
             WHERE title LIKE ?1 OR content LIKE ?1 OR tags LIKE ?1
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        for row in stmt.query_map(params![needle, limit as i64], row_to_object)? {
            hits.push(row?);
        }
        Ok(hits)
    }
}

fn row_to_object(row: &rusqlite::Row) -> rusqlite::Result<MemoryObject> {
    let tags_json: String = row.get(4)?;
    let relations_json: String = row.get(5)?;
    let source_json: Option<String> = row.get(6)?;
    Ok(MemoryObject {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        relations: serde_json::from_str(&relations_json).unwrap_or_default(),
        source: source_json.as_deref().and_then(|s| serde_json::from_str(s).ok()),
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

/// Build an FTS5 MATCH query string from a free-form task description.
/// Strategy: tokenise to alphanumeric words ≥3 chars, lowercase, dedupe,
/// quote each one (to escape FTS5 operators), join with OR. Empty result
/// means we have nothing to search for.
fn build_fts_query(task: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for raw in task.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 3 {
            continue;
        }
        let lc = raw.to_lowercase();
        if STOP_WORDS.contains(&lc.as_str()) {
            continue;
        }
        if seen.insert(lc.clone()) {
            // FTS5 phrase-quote each term so reserved words ("AND", "OR",
            // "NOT", "NEAR") don't get interpreted as operators.
            terms.push(format!("\"{}\"", lc.replace('"', "\"\"")));
        }
    }
    terms.join(" OR ")
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "that", "this", "what", "when",
    "where", "which", "while", "would", "should", "could", "your", "you", "are",
    "but", "not", "have", "has", "had", "was", "were", "been", "being",
];

// ─── JSON fallback backend ──────────────────────────────────────────────────

struct JsonBackend {
    mem_dir: PathBuf,
    objects: Vec<MemoryObject>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    objects: Vec<MemoryObject>,
    #[serde(rename = "updatedAt", default)]
    updated_at: String,
}

impl JsonBackend {
    fn new(cwd: &Path) -> Self {
        // JSON fallback stores everything alongside the (would-be) SQLite DB
        // in the project's slot — `<config>/projects/<id>/memory/`.
        let mem_dir = paths::project_dir(cwd).join("memory");
        let mut backend = Self { mem_dir, objects: Vec::new() };
        backend.load();
        backend
    }

    fn init(&mut self) -> bool {
        if !self.mem_dir.exists() {
            let _ = fs::create_dir_all(&self.mem_dir);
        }
        self.save();
        true
    }

    fn load(&mut self) {
        let index = self.mem_dir.join("index.json");
        if !index.exists() {
            return;
        }
        if let Ok(content) = fs::read_to_string(&index) {
            if let Ok(file) = serde_json::from_str::<StoreFile>(&content) {
                self.objects = file.objects;
            }
        }
    }

    fn save(&self) {
        if !self.mem_dir.exists() {
            let _ = fs::create_dir_all(&self.mem_dir);
        }
        let file = StoreFile {
            version: 1,
            objects: self.objects.clone(),
            updated_at: Utc::now().to_rfc3339(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = fs::write(self.mem_dir.join("index.json"), json);
        }
        for obj in &self.objects {
            let filename = format!("{}-{}.md", obj.kind, obj.id);
            let md = format!(
                "# {}\n\nType: {}\nTags: {}\nCreated: {}\n\n{}\n",
                obj.title,
                obj.kind,
                obj.tags.join(", "),
                obj.created_at,
                obj.content,
            );
            let _ = fs::write(self.mem_dir.join(filename), md);
        }
    }

    fn insert(&mut self, obj: MemoryObject) {
        self.objects.push(obj);
        self.save();
    }

    fn delete(&mut self, id: &str) -> bool {
        if let Some(idx) = self.objects.iter().position(|o| o.id == id) {
            let removed = self.objects.remove(idx);
            let filename = format!("{}-{}.md", removed.kind, removed.id);
            let _ = fs::remove_file(self.mem_dir.join(filename));
            self.save();
            true
        } else {
            false
        }
    }

    fn get(&self, id: &str) -> Option<MemoryObject> {
        self.objects.iter().find(|o| o.id == id).cloned()
    }

    fn all(&self) -> Vec<MemoryObject> {
        self.objects.clone()
    }

    fn by_type(&self, kind: &str) -> Vec<MemoryObject> {
        self.objects.iter().filter(|o| o.kind == kind).cloned().collect()
    }

    fn count(&self) -> usize {
        self.objects.len()
    }

    fn stats(&self) -> HashMap<String, u32> {
        let mut out = HashMap::new();
        for obj in &self.objects {
            *out.entry(obj.kind.clone()).or_insert(0) += 1;
        }
        out
    }

    /// Keyword scoring identical to the original JS fallback path.
    fn search(&self, task: &str, limit: usize) -> Vec<MemoryObject> {
        if self.objects.is_empty() {
            return Vec::new();
        }
        let words: Vec<String> = task.to_lowercase().split_whitespace().map(String::from).collect();
        let mut scored: Vec<(MemoryObject, u32)> = Vec::new();
        for obj in &self.objects {
            let title_lc = obj.title.to_lowercase();
            let text = format!("{} {} {}", obj.title, obj.content, obj.tags.join(" ")).to_lowercase();
            let mut score = 0u32;
            for word in &words {
                if word.len() < 3 {
                    continue;
                }
                if text.contains(word) {
                    score += 1;
                }
                if title_lc.contains(word) {
                    score += 3;
                }
                if obj.tags.iter().any(|t| t.contains(word)) {
                    score += 2;
                }
            }
            if score > 0 {
                scored.push((obj.clone(), score));
            }
        }
        scored.sort_by_key(|b| std::cmp::Reverse(b.1));
        scored.into_iter().take(limit).map(|(o, _)| o).collect()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(8);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh(root: &Path) -> MemoryStore {
        MemoryStore::new(root)
    }

    #[test]
    fn remember_and_recall_via_sqlite() {
        let dir = tempdir().unwrap();
        let mut store = fresh(dir.path());
        assert_eq!(store.backend_name(), "sqlite+fts5");
        let obj = store.remember("decision", "use-rust", "Port the agent to Rust", vec!["rust".into()]);
        assert_eq!(obj.kind, "decision");
        let hits = store.load_for_task("rust port");
        assert!(hits.iter().any(|o| o.id == obj.id), "FTS should find the rust entry");
    }

    #[test]
    fn fts_query_construction() {
        let q = build_fts_query("port the agent to rust");
        // "the" is a stop word so it shouldn't appear
        assert!(q.contains("\"port\""));
        assert!(q.contains("\"agent\""));
        assert!(q.contains("\"rust\""));
        assert!(!q.contains("\"the\""));
        assert!(q.contains(" OR "));
    }

    #[test]
    fn forget_removes() {
        let dir = tempdir().unwrap();
        let mut store = fresh(dir.path());
        let obj = store.remember("note", "x", "y", vec![]);
        assert!(store.get(&obj.id).is_some());
        assert!(store.forget(&obj.id));
        assert!(store.get(&obj.id).is_none());
    }

    #[test]
    fn stats_count_by_type() {
        let dir = tempdir().unwrap();
        let mut store = fresh(dir.path());
        store.remember("decision", "a", "x", vec![]);
        store.remember("decision", "b", "y", vec![]);
        store.remember("workflow", "c", "z", vec![]);
        let s = store.stats();
        assert_eq!(s.total, 3);
        assert_eq!(s.by_type.get("decision").copied(), Some(2));
        assert_eq!(s.by_type.get("workflow").copied(), Some(1));
    }

    /// Bug #2 regression: ExecCtx::memory was previously rebuilt as a fresh
    /// `MemoryStore::new(...)` on every tool call. With the JSON backend
    /// (no shared DB), writes made by one tool call were invisible to the
    /// next. The fix is to share the same `Arc<Mutex<MemoryStore>>` across
    /// every tool call in a session. This test pins that contract: cloning
    /// the Arc and writing through one handle MUST be visible via another.
    #[test]
    fn shared_arc_writes_visible_to_clone() {
        use std::sync::Arc;
        use parking_lot::Mutex;
        let dir = tempdir().unwrap();
        let shared: Arc<Mutex<MemoryStore>> = Arc::new(Mutex::new(MemoryStore::new(dir.path())));

        // ExecCtx instance #1 — writes
        let ctx_a = shared.clone();
        let obj = ctx_a.lock().remember("decision", "use rust", "rewrote agent", vec!["rust".into()]);

        // ExecCtx instance #2 — reads, must see #1's write
        let ctx_b = shared.clone();
        let items = ctx_b.lock().load_for_task("rust");
        assert!(items.iter().any(|o| o.id == obj.id),
            "second Arc handle must see write from first handle; got {} items", items.len());

        // And forgetting via #2 is visible to #1.
        let removed = ctx_b.lock().forget(&obj.id);
        assert!(removed);
        let after = ctx_a.lock().load_for_task("rust");
        assert!(!after.iter().any(|o| o.id == obj.id),
            "forget via one handle must be visible to the other");
    }

    /// Two independent `MemoryStore::new` instances pointing at the same
    /// `cwd` MUST converge through their shared persistence backend
    /// (SQLite by default). This documents the cross-process invariant
    /// the agent relies on when child tools spawn out-of-band.
    ///
    /// Uses `new_json_only` so the test is independent of the global
    /// `~/.config/itsy/projects/<hash>/memory.db` location — the SQLite
    /// path is keyed by a hash of the cwd and other parallel tests can
    /// poison the cache in ways that mask the per-instance contract.
    #[test]
    fn fresh_stores_on_same_cwd_see_each_others_writes_via_disk() {
        let dir = tempdir().unwrap();
        let mut a = MemoryStore::new_json_only(dir.path());
        let obj = a.remember("note", "persist-me", "across-instances", vec![]);
        let b = MemoryStore::new_json_only(dir.path());
        let hits = b.load_for_task("persist-me");
        assert!(hits.iter().any(|o| o.id == obj.id),
            "second store on same cwd must see write via shared backend; got {} hits", hits.len());
    }
}
