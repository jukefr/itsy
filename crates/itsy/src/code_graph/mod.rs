//! Native Rust code graph subsystem.
//!
//! Replaces the external `budget-aware-mcp` Node.js server with an in-process
//! tree-sitter + SQLite (FTS5) implementation. Exposes a small public API
//! consumed by [`crate::executor`] (the `list_projects`, `graph_search`, and
//! `explain_symbol` tools).
//!
//! Module layout:
//!
//! * [`db`]       — schema bootstrapping + connection helper
//! * [`language`] — extension → grammar + node-kind tables
//! * [`indexer`]  — file walking, tree-sitter parsing, symbol + call extraction
//! * [`query`]    — repo listing, FTS search, symbol explanation

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::Serialize;

mod db;
mod indexer;
mod language;
mod query;

pub use indexer::IndexStats;

/// Per-file size cap. Files larger than this are skipped to keep parse time
/// bounded.
pub const MAX_FILE_BYTES: u64 = 1_024 * 1_024; // 1 MB

/// Per-repo file cap. Once an index pass has visited this many parseable
/// files, the rest of the walk is skipped.
pub const MAX_FILES_PER_REPO: usize = 5_000;

/// Default DB sub-path under the repo root.
pub const DEFAULT_DB_REL: &str = ".itsy/codegraph.db";

#[derive(Debug, Clone, Serialize)]
pub struct RepoSummary {
    pub name: String,
    pub path: String,
    pub file_count: u32,
    pub symbol_count: u32,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolHit {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub signature: Option<String>,
    pub snippet: Option<String>,
    pub repo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolExplanation {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub signature: Option<String>,
    pub callers: Vec<SymbolHit>,
    pub callees: Vec<SymbolHit>,
    pub snippet: Option<String>,
}

/// Native code graph backed by SQLite + FTS5.
///
/// One [`CodeGraph`] owns a single connection guarded by a `parking_lot`
/// mutex. The connection is opened in WAL mode; concurrent reads are cheap
/// and writes serialise on the mutex.
pub struct CodeGraph {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl CodeGraph {
    /// Open (or create) the codegraph DB rooted at `root`.
    ///
    /// The path is `<root>/.itsy/codegraph.db` unless `ITSY_CODEGRAPH_DB`
    /// overrides it.
    pub fn open(root: &Path) -> Result<Self> {
        let db_path = match std::env::var("ITSY_CODEGRAPH_DB") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => root.join(DEFAULT_DB_REL),
        };
        Self::open_at(&db_path)
    }

    /// Open a graph at an explicit DB path. Convenient for tests and tools
    /// that want isolated storage without juggling env vars.
    pub fn open_at(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(db_path)?;
        db::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Index a single repository rooted at `path`, naming it `name`.
    ///
    /// Returns a fresh summary. Existing rows for the same name are removed
    /// before re-indexing so callers can call this repeatedly.
    pub fn index_repo(&self, path: &Path, name: &str) -> Result<RepoSummary> {
        if std::env::var("ITSY_CODEGRAPH_DISABLE").as_deref() == Ok("true") {
            return Ok(RepoSummary {
                name: name.to_string(),
                path: path.display().to_string(),
                file_count: 0,
                symbol_count: 0,
                languages: vec![],
            });
        }
        let mut conn = self.conn.lock();
        indexer::index_repo(&mut conn, path, name)
    }

    pub fn list_repos(&self) -> Result<Vec<RepoSummary>> {
        let conn = self.conn.lock();
        query::list_repos(&conn)
    }

    pub fn search_graph(&self, query_str: &str, max_tokens: u32) -> Result<Vec<SymbolHit>> {
        let conn = self.conn.lock();
        query::search_graph(&conn, query_str, max_tokens)
    }

    pub fn explain_symbol(&self, symbol: &str) -> Result<Option<SymbolExplanation>> {
        let conn = self.conn.lock();
        query::explain_symbol(&conn, symbol)
    }

    /// Path of the underlying SQLite file. Mostly useful in tests.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

// ─── Global singleton ───────────────────────────────────────────────────────

static GLOBAL: OnceLock<CodeGraph> = OnceLock::new();

/// Returns the process-wide [`CodeGraph`], initialising it on first call
/// from the current working directory.
///
/// If the disable env var is set, or if opening fails, returns `None`.
pub fn try_get_code_graph() -> Option<&'static CodeGraph> {
    if std::env::var("ITSY_CODEGRAPH_DISABLE").as_deref() == Ok("true") {
        return None;
    }
    if let Some(g) = GLOBAL.get() {
        return Some(g);
    }
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match CodeGraph::open(&root) {
        Ok(g) => {
            let _ = GLOBAL.set(g);
            GLOBAL.get()
        }
        Err(_) => None,
    }
}

/// Like [`try_get_code_graph`] but panics on failure. Prefer the fallible
/// variant in production code paths; this is convenient in tests.
pub fn get_code_graph() -> &'static CodeGraph {
    try_get_code_graph().expect("code_graph singleton unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn open_in_tempdir() -> (TempDir, CodeGraph) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("codegraph.db");
        let graph = CodeGraph::open_at(&db_path).expect("open_at");
        (dir, graph)
    }

    #[test]
    fn indexes_rust_file() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    helper(a);\n    a + b\n}\nfn helper(x: i32) {}\n",
        )
        .unwrap();

        let summary = graph.index_repo(&repo, "rust-tiny").unwrap();
        assert_eq!(summary.languages, vec!["rust".to_string()]);
        assert!(summary.symbol_count >= 2, "got {summary:?}");

        let repos = graph.list_repos().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "rust-tiny");

        let hits = graph.search_graph("add", 4000).unwrap();
        assert!(hits.iter().any(|h| h.name == "add"), "hits={hits:?}");

        let explained = graph.explain_symbol("add").unwrap().expect("add resolves");
        assert_eq!(explained.name, "add");
        // `add` calls `helper` which is itself a recorded symbol.
        assert!(
            explained.callees.iter().any(|c| c.name == "helper"),
            "callees={:?}",
            explained.callees
        );
    }

    #[test]
    fn indexes_typescript_file() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("ts-repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("api.ts"),
            "export function greet(name: string): string {\n  return `hi ${name}`;\n}\n\
             export class Service {\n  run() { greet(\"world\"); }\n}\n",
        )
        .unwrap();

        let summary = graph.index_repo(&repo, "ts-tiny").unwrap();
        assert!(summary.languages.contains(&"typescript".to_string()));
        assert!(summary.symbol_count >= 2);

        let hits = graph.search_graph("greet", 4000).unwrap();
        assert!(hits.iter().any(|h| h.name == "greet"));

        let explained = graph.explain_symbol("greet").unwrap().unwrap();
        assert_eq!(explained.name, "greet");
    }

    #[test]
    fn indexes_python_file() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("py-repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("mod.py"),
            "def fib(n):\n    return n if n < 2 else fib(n - 1) + fib(n - 2)\n\nclass Calc:\n    def go(self):\n        return fib(5)\n",
        )
        .unwrap();

        let summary = graph.index_repo(&repo, "py-tiny").unwrap();
        assert!(summary.languages.contains(&"python".to_string()));
        assert!(summary.symbol_count >= 2);

        let hits = graph.search_graph("fib", 4000).unwrap();
        assert!(hits.iter().any(|h| h.name == "fib"), "hits={hits:?}");

        let explained = graph.explain_symbol("fib").unwrap().unwrap();
        assert_eq!(explained.name, "fib");
    }

    #[test]
    fn short_fts_query_is_graceful() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("x.rs"), "fn foo() {}\n").unwrap();
        graph.index_repo(&repo, "x").unwrap();

        // 1-char query: should return an empty result, not panic / not error.
        let hits = graph.search_graph("a", 4000).unwrap();
        assert!(hits.is_empty());

        // empty query
        let hits = graph.search_graph("", 4000).unwrap();
        assert!(hits.is_empty());

        // pure punctuation
        let hits = graph.search_graph("()*", 4000).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn reindex_replaces_previous_rows() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("v1.rs"), "fn alpha() {}\n").unwrap();
        graph.index_repo(&repo, "same").unwrap();
        assert!(graph.explain_symbol("alpha").unwrap().is_some());

        // Replace contents and re-index under the same repo name.
        fs::remove_file(repo.join("v1.rs")).unwrap();
        fs::write(repo.join("v2.rs"), "fn beta() {}\n").unwrap();
        graph.index_repo(&repo, "same").unwrap();

        assert!(graph.explain_symbol("alpha").unwrap().is_none());
        assert!(graph.explain_symbol("beta").unwrap().is_some());

        let repos = graph.list_repos().unwrap();
        assert_eq!(repos.len(), 1, "should not duplicate repo row");
    }

    #[test]
    fn empty_repo_indexes_to_zero_symbols() {
        let (dir, graph) = open_in_tempdir();
        let repo = dir.path().join("empty");
        fs::create_dir_all(&repo).unwrap();
        // A non-indexable file extension — should be walked but produce no
        // symbols.
        fs::write(repo.join("notes.txt"), "nothing to see\n").unwrap();
        let summary = graph.index_repo(&repo, "empty").unwrap();
        assert_eq!(summary.symbol_count, 0);
        assert_eq!(summary.file_count, 0);
    }
}
