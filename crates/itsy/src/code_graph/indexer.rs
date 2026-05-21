//! Repo walker + tree-sitter symbol extractor.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::Utc;
use ignore::WalkBuilder;
use rusqlite::{params, Connection};
use tree_sitter::{Node, Parser};

use super::language::Lang;
use super::{RepoSummary, MAX_FILES_PER_REPO, MAX_FILE_BYTES};

/// Stats returned alongside [`RepoSummary`] for diagnostic use.
#[derive(Debug, Default, Clone)]
pub struct IndexStats {
    pub files_walked: usize,
    pub files_indexed: usize,
    pub symbols_inserted: usize,
    pub calls_inserted: usize,
    pub files_skipped_too_large: usize,
    pub parse_errors: usize,
}

pub fn index_repo(conn: &mut Connection, path: &Path, name: &str) -> Result<RepoSummary> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Reset any previous data for this repo name; ON DELETE CASCADE drops
    // symbols + calls.
    conn.execute("DELETE FROM repos WHERE name = ?1", params![name])?;

    let tx = conn.transaction()?;
    let indexed_at = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO repos (name, path, indexed_at, file_count, symbol_count, languages)
         VALUES (?1, ?2, ?3, 0, 0, '[]')",
        params![name, abs.to_string_lossy(), indexed_at],
    )?;
    let repo_id = tx.last_insert_rowid();

    let mut stats = IndexStats::default();
    let mut languages: BTreeSet<&'static str> = BTreeSet::new();

    let walker = WalkBuilder::new(&abs)
        .git_ignore(true)
        .git_exclude(true)
        .hidden(false)
        .build();

    for dent in walker {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        stats.files_walked += 1;
        if stats.files_indexed >= MAX_FILES_PER_REPO {
            break;
        }
        let file_path = dent.path();
        let Some(lang) = Lang::from_path(file_path) else { continue };

        let meta = match std::fs::metadata(file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            stats.files_skipped_too_large += 1;
            continue;
        }

        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let rel = file_path
            .strip_prefix(&abs)
            .unwrap_or(file_path)
            .to_string_lossy()
            .replace('\\', "/");

        if index_file(&tx, repo_id, lang, &rel, &source, &mut stats).is_ok() {
            stats.files_indexed += 1;
            languages.insert(lang.name());
        } else {
            stats.parse_errors += 1;
        }
    }

    let langs_json =
        serde_json::to_string(&languages.iter().copied().collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into());

    tx.execute(
        "UPDATE repos SET file_count = ?1, symbol_count = ?2, languages = ?3 WHERE id = ?4",
        params![
            stats.files_indexed as i64,
            stats.symbols_inserted as i64,
            langs_json,
            repo_id
        ],
    )?;
    tx.commit()?;

    Ok(RepoSummary {
        name: name.to_string(),
        path: abs.to_string_lossy().into_owned(),
        file_count: stats.files_indexed as u32,
        symbol_count: stats.symbols_inserted as u32,
        languages: languages.iter().map(|s| s.to_string()).collect(),
    })
}

fn index_file(
    conn: &Connection,
    repo_id: i64,
    lang: Lang,
    rel_path: &str,
    source: &str,
    stats: &mut IndexStats,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|e| anyhow!("set_language failed for {}: {e}", lang.name()))?;
    let tree = parser
        .parse(source.as_bytes(), None)
        .ok_or_else(|| anyhow!("parse returned None"))?;

    let lines: Vec<&str> = source.lines().collect();
    let symbol_kinds = lang.symbol_kinds();
    let call_kinds: &[&str] = lang.call_kinds();

    // Walk the tree iteratively. For each symbol-kind node we record the
    // symbol and then recurse into its body, tracking call sites as edges
    // owned by the enclosing symbol.
    let mut stack: Vec<(Node, Option<i64>, Option<String>)> =
        vec![(tree.root_node(), None, None)];

    let mut insert_sym = conn.prepare_cached(
        "INSERT INTO symbols (repo_id, name, kind, file, line, signature, parent, language, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut insert_call = conn.prepare_cached(
        "INSERT INTO calls (caller_id, callee_name, file, line) VALUES (?1, ?2, ?3, ?4)",
    )?;

    while let Some((node, caller_id, parent_name)) = stack.pop() {
        let kind_str = node.kind();

        // Symbol detection
        if let Some(sym_kind) = symbol_kinds
            .iter()
            .find(|(k, _)| *k == kind_str)
            .map(|(_, v)| *v)
        {
            // For TS/JS variable_declarator we only record it if the
            // initializer is an arrow / function expression.
            let is_var_decl = kind_str == "variable_declarator";
            let counts = if is_var_decl {
                variable_init_is_function(&node)
            } else {
                true
            };
            if counts {
                if let Some(name) = node_name(&node, source, lang) {
                    let line = node.start_position().row as u32 + 1;
                    let signature = first_line_signature(source, &node);
                    let snippet = snippet_around(&lines, line as usize);
                    insert_sym.execute(params![
                        repo_id,
                        name,
                        sym_kind,
                        rel_path,
                        line as i64,
                        signature,
                        parent_name.clone(),
                        lang.name(),
                        snippet,
                    ])?;
                    let id = conn.last_insert_rowid();
                    stats.symbols_inserted += 1;
                    // Walk children with this symbol as the new caller.
                    push_children(&mut stack, &node, Some(id), Some(name.clone()));
                    continue;
                }
            }
        }

        // Call detection (only meaningful when nested inside a symbol).
        if call_kinds.contains(&kind_str) {
            if let Some(cid) = caller_id {
                if let Some(callee) = call_target_name(&node, source) {
                    let line = node.start_position().row as u32 + 1;
                    insert_call.execute(params![cid, callee, rel_path, line as i64])?;
                    stats.calls_inserted += 1;
                }
            }
        }

        push_children(&mut stack, &node, caller_id, parent_name);
    }

    Ok(())
}

fn push_children<'a>(
    stack: &mut Vec<(Node<'a>, Option<i64>, Option<String>)>,
    node: &Node<'a>,
    caller_id: Option<i64>,
    parent_name: Option<String>,
) {
    let mut cursor = node.walk();
    // Push in reverse so traversal is left-to-right.
    let children: Vec<Node<'a>> = node.children(&mut cursor).collect();
    for child in children.into_iter().rev() {
        stack.push((child, caller_id, parent_name.clone()));
    }
}

/// Extract a human-readable name for a declaration node.
fn node_name(node: &Node, source: &str, lang: Lang) -> Option<String> {
    // Try the conventional `name` field first.
    if let Some(name_node) = node.child_by_field_name("name") {
        return slice_text(source, &name_node).map(|s| s.to_string());
    }

    match (lang, node.kind()) {
        // Rust impl blocks expose their `type` field instead of `name`.
        (Lang::Rust, "impl_item") => node
            .child_by_field_name("type")
            .and_then(|n| slice_text(source, &n).map(|s| s.to_string()))
            .map(|s| format!("impl {s}")),

        // TS/JS variable_declarator: name is the first child.
        (_, "variable_declarator") => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .and_then(|n| slice_text(source, &n).map(|s| s.to_string())),

        // Go type_spec
        (Lang::Go, "type_spec") => node
            .child_by_field_name("name")
            .and_then(|n| slice_text(source, &n).map(|s| s.to_string())),

        _ => None,
    }
}

/// Returns true if a TS/JS `variable_declarator` initialises to a
/// function-like expression.
fn variable_init_is_function(node: &Node) -> bool {
    let Some(value) = node.child_by_field_name("value") else {
        return false;
    };
    matches!(
        value.kind(),
        "arrow_function" | "function" | "function_expression" | "generator_function"
    )
}

/// Pull a callee identifier out of a call_expression / call node.
fn call_target_name(node: &Node, source: &str) -> Option<String> {
    let target = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("name"))
        .or_else(|| node.named_child(0))?;

    match target.kind() {
        "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
        | "scoped_identifier" => slice_text(source, &target).map(|s| s.to_string()),
        // member_expression / selector_expression / field_expression — take the rightmost identifier
        "member_expression" | "selector_expression" | "field_expression" | "scoped_type_identifier" => {
            let mut cursor = target.walk();
            let last_id = target
                .children(&mut cursor)
                .filter(|c| {
                    matches!(
                        c.kind(),
                        "identifier"
                            | "field_identifier"
                            | "property_identifier"
                            | "type_identifier"
                    )
                })
                .last();
            last_id.and_then(|n| slice_text(source, &n).map(|s| s.to_string()))
        }
        _ => slice_text(source, &target).map(|s| s.to_string()),
    }
}

fn slice_text<'a>(source: &'a str, node: &Node) -> Option<&'a str> {
    let start = node.start_byte();
    let end = node.end_byte();
    if end > source.len() || start > end {
        return None;
    }
    std::str::from_utf8(&source.as_bytes()[start..end]).ok()
}

/// First line of the declaration, trimmed and capped at 200 chars.
fn first_line_signature(source: &str, node: &Node) -> Option<String> {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    if start >= end {
        return None;
    }
    let chunk = &source[start..end];
    let line = chunk.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let mut s = line.to_string();
    if s.len() > 200 {
        s.truncate(200);
        s.push_str("…");
    }
    Some(s)
}

/// Three lines of context centred on the declaration's first line.
fn snippet_around(lines: &[&str], line_1based: usize) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let idx = line_1based.saturating_sub(1);
    let start = idx.saturating_sub(1);
    let end = (idx + 2).min(lines.len()); // 3 lines: prev, current, next
    if start >= end {
        return None;
    }
    Some(lines[start..end].join("\n"))
}

