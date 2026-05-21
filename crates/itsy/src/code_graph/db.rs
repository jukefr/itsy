//! Schema bootstrapping for the code graph DB.
//!
//! Tables:
//!
//! * `repos`       — one row per indexed project
//! * `symbols`     — declarations extracted from source files
//! * `calls`       — `(caller_id, callee_name, file, line)` edges
//! * `symbols_fts` — FTS5 virtual table mirroring `symbols.{name,signature,snippet}`

use rusqlite::Connection;

/// Idempotently create every table, index, and trigger.
///
/// Safe to call on an already-initialised DB; each statement uses
/// `IF NOT EXISTS`.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS repos (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL UNIQUE,
            path        TEXT NOT NULL,
            indexed_at  TEXT NOT NULL,
            file_count  INTEGER NOT NULL DEFAULT 0,
            symbol_count INTEGER NOT NULL DEFAULT 0,
            languages   TEXT NOT NULL DEFAULT '[]'
        );

        CREATE TABLE IF NOT EXISTS symbols (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_id     INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
            name        TEXT NOT NULL,
            kind        TEXT NOT NULL,
            file        TEXT NOT NULL,
            line        INTEGER NOT NULL,
            signature   TEXT,
            parent      TEXT,
            language    TEXT NOT NULL,
            snippet     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_sym_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_sym_repo ON symbols(repo_id);
        CREATE INDEX IF NOT EXISTS idx_sym_kind ON symbols(kind);

        CREATE TABLE IF NOT EXISTS calls (
            caller_id   INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
            callee_name TEXT NOT NULL,
            file        TEXT NOT NULL,
            line        INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_calls_caller ON calls(caller_id);
        CREATE INDEX IF NOT EXISTS idx_calls_callee ON calls(callee_name);

        CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
            name,
            signature,
            snippet,
            content='symbols',
            content_rowid='id',
            tokenize='porter unicode61'
        );

        CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
            INSERT INTO symbols_fts(rowid, name, signature, snippet)
            VALUES (new.id, new.name, COALESCE(new.signature, ''), COALESCE(new.snippet, ''));
        END;
        CREATE TRIGGER IF NOT EXISTS symbols_ad AFTER DELETE ON symbols BEGIN
            INSERT INTO symbols_fts(symbols_fts, rowid, name, signature, snippet)
            VALUES ('delete', old.id, old.name, COALESCE(old.signature, ''), COALESCE(old.snippet, ''));
        END;
        CREATE TRIGGER IF NOT EXISTS symbols_au AFTER UPDATE ON symbols BEGIN
            INSERT INTO symbols_fts(symbols_fts, rowid, name, signature, snippet)
            VALUES ('delete', old.id, old.name, COALESCE(old.signature, ''), COALESCE(old.snippet, ''));
            INSERT INTO symbols_fts(rowid, name, signature, snippet)
            VALUES (new.id, new.name, COALESCE(new.signature, ''), COALESCE(new.snippet, ''));
        END;
        "#,
    )?;
    Ok(())
}
