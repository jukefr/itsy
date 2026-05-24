//! Read-side queries: repo listing, FTS5 search, symbol explanation.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use super::{RepoSummary, SymbolExplanation, SymbolHit};

pub fn list_repos(conn: &Connection) -> Result<Vec<RepoSummary>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, file_count, symbol_count, languages
         FROM repos
         ORDER BY name ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let name: String = row.get(0)?;
            let path: String = row.get(1)?;
            let file_count: i64 = row.get(2)?;
            let symbol_count: i64 = row.get(3)?;
            let langs_json: String = row.get(4)?;
            let languages: Vec<String> =
                serde_json::from_str(&langs_json).unwrap_or_default();
            Ok(RepoSummary {
                name,
                path,
                file_count: file_count.max(0) as u32,
                symbol_count: symbol_count.max(0) as u32,
                languages,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Search symbols by FTS5 match against `name + signature + snippet`.
///
/// The result count is capped by `min(10, max_tokens / 200)` — symbols are
/// roughly 200 tokens per snippet block, so this keeps the formatted output
/// within the caller's token budget.
pub fn search_graph(
    conn: &Connection,
    query: &str,
    max_tokens: u32,
) -> Result<Vec<SymbolHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let Some(fts_query) = sanitize_fts_query(trimmed) else {
        return Ok(vec![]);
    };

    let cap = std::cmp::min(10u32, (max_tokens / 200).max(1));

    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name AS repo
         FROM symbols_fts f
         JOIN symbols s ON s.id = f.rowid
         JOIN repos r ON r.id = s.repo_id
         WHERE symbols_fts MATCH ?1
         ORDER BY bm25(symbols_fts)
         LIMIT ?2",
    )?;

    let rows = stmt
        .query_map(params![fts_query, cap as i64], |row| {
            Ok(SymbolHit {
                name: row.get(0)?,
                kind: row.get(1)?,
                file: row.get(2)?,
                line: {
                    let l: i64 = row.get(3)?;
                    l.max(0) as u32
                },
                signature: row.get(4)?,
                snippet: row.get(5)?,
                repo: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>();

    // FTS may reject malformed queries even after sanitisation — fall back
    // to a LIKE search rather than surfacing the error.
    match rows {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        _ => like_fallback(conn, trimmed, cap),
    }
}

pub fn explain_symbol(conn: &Connection, symbol: &str) -> Result<Option<SymbolExplanation>> {
    let sym = symbol.trim();
    if sym.is_empty() {
        return Ok(None);
    }

    let found = conn
        .query_row(
            "SELECT s.id, s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name
             FROM symbols s
             JOIN repos r ON r.id = s.repo_id
             WHERE s.name = ?1
             ORDER BY s.id ASC
             LIMIT 1",
            params![sym],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;

    let (sym_id, name, kind, file, line, signature, snippet, repo) = match found {
        Some(t) => t,
        None => {
            // Soft LIKE fallback for case-insensitive / partial match.
            let like = format!("%{sym}%");
            let row = conn
                .query_row(
                    "SELECT s.id, s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name
                     FROM symbols s
                     JOIN repos r ON r.id = s.repo_id
                     WHERE s.name LIKE ?1
                     ORDER BY length(s.name) ASC
                     LIMIT 1",
                    params![like],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, String>(7)?,
                        ))
                    },
                )
                .optional()?;
            match row {
                Some(t) => t,
                None => return Ok(None),
            }
        }
    };
    let _ = repo;
    let line_u = line.max(0) as u32;

    // Callers: any symbol that calls into `name`.
    let mut caller_stmt = conn.prepare(
        "SELECT s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name
         FROM calls c
         JOIN symbols s ON s.id = c.caller_id
         JOIN repos r ON r.id = s.repo_id
         WHERE c.callee_name = ?1
         ORDER BY s.file, s.line
         LIMIT 25",
    )?;
    let callers = caller_stmt
        .query_map(params![name], |row| {
            Ok(SymbolHit {
                name: row.get(0)?,
                kind: row.get(1)?,
                file: row.get(2)?,
                line: row.get::<_, i64>(3)?.max(0) as u32,
                signature: row.get(4)?,
                snippet: row.get(5)?,
                repo: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Callees: every distinct callee from this symbol, joined back to the
    // symbols table when unique.
    let mut callee_stmt = conn.prepare(
        "SELECT DISTINCT c.callee_name, c.file, c.line
         FROM calls c
         WHERE c.caller_id = ?1
         ORDER BY c.line
         LIMIT 25",
    )?;
    let raw_callees = callee_stmt
        .query_map(params![sym_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?.max(0) as u32,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut resolve_stmt = conn.prepare(
        "SELECT s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name
         FROM symbols s
         JOIN repos r ON r.id = s.repo_id
         WHERE s.name = ?1
         LIMIT 2",
    )?;
    let mut callees = Vec::new();
    for (callee_name, call_file, call_line) in raw_callees {
        let mut matches = resolve_stmt
            .query_map(params![&callee_name], |row| {
                Ok(SymbolHit {
                    name: row.get(0)?,
                    kind: row.get(1)?,
                    file: row.get(2)?,
                    line: row.get::<_, i64>(3)?.max(0) as u32,
                    signature: row.get(4)?,
                    snippet: row.get(5)?,
                    repo: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() == 1 {
            callees.push(matches.remove(0));
        } else {
            // Unresolved — synthesise a stub hit at the call site.
            callees.push(SymbolHit {
                name: callee_name,
                kind: "call".to_string(),
                file: call_file,
                line: call_line,
                signature: None,
                snippet: None,
                repo: String::new(),
            });
        }
    }

    Ok(Some(SymbolExplanation {
        name,
        kind,
        file,
        line: line_u,
        signature,
        callers,
        callees,
        snippet,
    }))
}

/// Translate a user query into FTS5 syntax.
///
/// Strips characters that FTS5 treats as operators (`"`, `(`, `)`, `*`,
/// `:`, `^`, `-`), then wraps each whitespace-separated term in a prefix
/// match. Returns `None` if every term is too short to be useful (FTS5 trips
/// on bare 1-char tokens with porter tokenizer).
fn sanitize_fts_query(input: &str) -> Option<String> {
    let cleaned: String = input
        .chars()
        .map(|c| match c {
            '"' | '(' | ')' | '*' | ':' | '^' | '-' => ' ',
            _ => c,
        })
        .collect();

    let parts: Vec<String> = cleaned
        .split_whitespace()
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// LIKE-based fallback used when an FTS query yields nothing.
fn like_fallback(conn: &Connection, query: &str, cap: u32) -> Result<Vec<SymbolHit>> {
    let like = format!("%{}%", query.replace(['%', '_'], ""));
    if like.len() <= 2 {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, s.file, s.line, s.signature, s.snippet, r.name
         FROM symbols s
         JOIN repos r ON r.id = s.repo_id
         WHERE s.name LIKE ?1 OR s.signature LIKE ?1
         ORDER BY length(s.name)
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![like, cap as i64], |row| {
            Ok(SymbolHit {
                name: row.get(0)?,
                kind: row.get(1)?,
                file: row.get(2)?,
                line: row.get::<_, i64>(3)?.max(0) as u32,
                signature: row.get(4)?,
                snippet: row.get(5)?,
                repo: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
