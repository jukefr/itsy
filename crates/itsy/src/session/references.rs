//! `@file` reference resolution.
//!
//! Parses `@path` mentions in user input and injects file content.
//!
//! Syntax:
//!   * `@src/main.rs`        — inject file content
//!   * `@src/main.rs:10-20`  — inject only lines 10..=20
//!   * `@package.json`       — inject file content
//!   * `@src/`               — list directory contents
//!   * `@~/config.json`      — resolve from home dir (allowed but path-checked)
//!
//! Safety:
//!   * Resolved paths must be inside cwd OR home dir (via
//!     `safe_resolve_path`).
//!   * Sensitive paths (`.ssh`, `.aws`, `/etc/shadow`, …) are refused.
//!   * Content is sanitised (ANSI stripped, secrets redacted) before
//!     injection so the model never sees raw API keys / tokens.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::security::{safe_resolve_path, sanitize_tool_output, PathOptions};

/// Matches `@path` but not inside backticks or immediately after a word
/// character. Captures the path portion (without the `@`).
///
/// Rust's `regex` does not support look-behind, so we emulate
/// `(?<![\w`])` by capturing the optional preceding char and rejecting
/// invalid contexts at filter time.
static FILE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[^\w`])@(\.?[^\s`,]*)").expect("valid regex literal"));

/// Optional `:start-end` (or `:start`) line range suffix.
static RANGE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(.+?):(\d+)(?:-(\d+))?$").expect("valid regex literal"));

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
const MAX_LINES: usize = 500;
const MAX_DIR_ENTRIES: usize = 50;
const MAX_FILES_PER_MESSAGE: usize = 20;
const MAX_REF_CHARS: usize = 8000; // ~2 K tokens
const MAX_FILE_CHARS: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RefKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRef {
    /// The raw path as the user typed it, including any line-range suffix.
    pub raw: String,
    /// The display path (relative to cwd when contained).
    pub path: String,
    pub kind: RefKind,
    pub content: String,
    /// Total number of lines for files, or number of entries for dirs.
    pub lines: usize,
}

/// Parse `@file` references from user input.
pub fn resolve_references(input: &str, cwd: &Path) -> Vec<ResolvedRef> {
    let mut out: Vec<ResolvedRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for cap in FILE_REGEX.captures_iter(input) {
        let raw_path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if raw_path.len() < 2 {
            continue;
        }
        // Strip trailing punctuation that users tend to attach to refs:
        // `look at @src/foo.rs,` -> `@src/foo.rs`.
        let raw_path = raw_path.trim_end_matches([',', ';', '.']);
        if raw_path.is_empty() {
            continue;
        }
        if !seen.insert(raw_path.to_string()) {
            continue;
        }
        if out.len() >= MAX_FILES_PER_MESSAGE {
            break;
        }

        // Extract optional :start-end line range
        let (path_part, range) = match RANGE_RE.captures(raw_path) {
            Some(c) => {
                let path = c.get(1).unwrap().as_str().to_string();
                let start: usize = c[2].parse().unwrap_or(1);
                let end: Option<usize> = c.get(3).and_then(|m| m.as_str().parse().ok());
                (path, Some((start, end)))
            }
            None => (raw_path.to_string(), None),
        };

        let is_home = path_part.starts_with("~/") || path_part.starts_with("~\\");
        let safe = match safe_resolve_path(
            &path_part,
            cwd,
            PathOptions { allow_home: is_home, allow_outside: is_home },
        ) {
            Ok(s) => s,
            Err(_) => continue, // silently skip — model gets nothing
        };

        let Ok(meta) = fs::metadata(&safe.full_path) else { continue };

        if meta.is_file() {
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(content) = fs::read_to_string(&safe.full_path) else { continue };
            let total_lines = content.split('\n').count();

            let body = if let Some((start, end)) = range {
                slice_lines(&content, start, end)
            } else if total_lines > MAX_LINES {
                let mut head: String =
                    content.split('\n').take(MAX_LINES).collect::<Vec<_>>().join("\n");
                head.push_str(&format!("\n... ({} more lines)", total_lines - MAX_LINES));
                head
            } else {
                content.clone()
            };

            out.push(ResolvedRef {
                raw: raw_path.to_string(),
                path: safe.display_path,
                kind: RefKind::File,
                content: sanitize_tool_output(&body),
                lines: total_lines,
            });
        } else if meta.is_dir() {
            let Ok(rd) = fs::read_dir(&safe.full_path) else { continue };
            let mut entries: Vec<String> = rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') || name == "node_modules" {
                        return None;
                    }
                    let is_dir =
                        e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    Some(if is_dir { format!("{name}/") } else { name })
                })
                .collect();
            entries.sort();
            entries.truncate(MAX_DIR_ENTRIES);
            let listing = entries.join("\n");
            let n = entries.len();
            out.push(ResolvedRef {
                raw: raw_path.to_string(),
                path: safe.display_path,
                kind: RefKind::Directory,
                content: listing,
                lines: n,
            });
        }
    }

    out
}

fn slice_lines(content: &str, start: usize, end: Option<usize>) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let start_idx = start.saturating_sub(1).min(lines.len());
    let end_idx = end.unwrap_or(start).min(lines.len());
    if end_idx <= start_idx {
        return String::new();
    }
    lines[start_idx..end_idx].join("\n")
}

/// Format resolved references for injection into the conversation.
///
/// Capped at ~2 K tokens (8 K chars) to prevent context overflow when
/// the user types `@dir/` on a large directory.
pub fn format_references_for_prompt(files: &[ResolvedRef]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut output = String::from("\n\n--- Referenced files ---\n");
    let mut total = output.len();

    for (i, file) in files.iter().enumerate() {
        let entry = match file.kind {
            RefKind::File => {
                let capped = if file.content.chars().count() > MAX_FILE_CHARS {
                    let head: String = file.content.chars().take(MAX_FILE_CHARS).collect();
                    format!("{head}\n... ({} lines total, truncated)", file.lines)
                } else {
                    file.content.clone()
                };
                format!("\n[file] {} ({} lines):\n```\n{}\n```\n", file.path, file.lines, capped)
            }
            RefKind::Directory => {
                format!("\n[dir] {}/:\n{}\n", file.path, file.content)
            }
        };
        if total + entry.len() > MAX_REF_CHARS {
            output.push_str(&format!(
                "\n... ({} more files truncated to fit context budget)\n",
                files.len() - i
            ));
            break;
        }
        output.push_str(&entry);
        total += entry.len();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `slice_lines` returns the closed range [start, end].
    #[test]
    fn slice_lines_returns_inclusive_range() {
        let content = "a\nb\nc\nd\ne";
        assert_eq!(slice_lines(content, 2, Some(4)), "b\nc\nd",
            "lines 2..=4 should yield b,c,d");
    }

    /// `slice_lines` with no end returns just the start line.
    #[test]
    fn slice_lines_single_line_when_no_end() {
        let content = "a\nb\nc";
        assert_eq!(slice_lines(content, 2, None), "b");
    }

    /// `slice_lines` clamps to file length without panic.
    #[test]
    fn slice_lines_clamps_oversized_end() {
        let content = "a\nb";
        assert_eq!(slice_lines(content, 1, Some(100)), "a\nb",
            "end past EOF must clamp, not panic");
    }

    /// `slice_lines` returns empty when end <= start.
    #[test]
    fn slice_lines_returns_empty_when_end_le_start() {
        assert_eq!(slice_lines("a\nb\nc", 5, Some(2)), "");
    }

    /// User input with no `@` references resolves to nothing.
    #[test]
    fn no_references_in_plain_text() {
        let cwd = tempfile::tempdir().unwrap();
        let refs = resolve_references("hello world, no refs here", cwd.path());
        assert!(refs.is_empty());
    }

    /// `@path` inside backticks must NOT be treated as a reference.
    /// Anti-regression: an inline-code mention of @foo shouldn't pull file content.
    #[test]
    fn backtick_paths_are_not_resolved() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("foo.txt"), "secret").unwrap();
        let refs = resolve_references("check `@foo.txt` for the answer", cwd.path());
        assert!(refs.is_empty(),
            "@path inside backticks must not resolve; got {} refs", refs.len());
    }

    /// `@file` after a space resolves and loads content.
    #[test]
    fn at_path_after_space_resolves_file() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("foo.txt"), "hello world").unwrap();
        let refs = resolve_references("read @foo.txt please", cwd.path());
        assert_eq!(refs.len(), 1);
        assert!(refs[0].content.contains("hello world"));
        assert!(matches!(refs[0].kind, RefKind::File));
    }

    /// Trailing punctuation (`,`, `;`, `.`) is stripped from the path.
    #[test]
    fn trailing_punctuation_stripped() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("foo.txt"), "x").unwrap();
        let refs = resolve_references("see @foo.txt, then continue", cwd.path());
        assert_eq!(refs.len(), 1, "comma after path must be stripped");
    }

    /// Duplicate refs are de-duplicated.
    #[test]
    fn duplicate_refs_deduped() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("foo.txt"), "x").unwrap();
        let refs = resolve_references("@foo.txt and @foo.txt again", cwd.path());
        assert_eq!(refs.len(), 1,
            "duplicate @foo.txt must be deduplicated; got {} refs", refs.len());
    }

    /// Directory references resolve and list entries.
    #[test]
    fn directory_reference_lists_entries() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("sub")).unwrap();
        std::fs::write(cwd.path().join("sub/a.txt"), "1").unwrap();
        std::fs::write(cwd.path().join("sub/b.txt"), "2").unwrap();
        let refs = resolve_references("look in @sub/", cwd.path());
        assert_eq!(refs.len(), 1);
        assert!(matches!(refs[0].kind, RefKind::Directory));
        assert!(refs[0].content.contains("a.txt"));
        assert!(refs[0].content.contains("b.txt"));
    }

    /// Dot-prefixed and node_modules entries are filtered from dir listings.
    #[test]
    fn directory_listing_filters_dots_and_node_modules() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join("dir/node_modules")).unwrap();
        std::fs::write(cwd.path().join("dir/.hidden"), "x").unwrap();
        std::fs::write(cwd.path().join("dir/visible.txt"), "v").unwrap();
        let refs = resolve_references("@dir/", cwd.path());
        assert_eq!(refs.len(), 1);
        let listing = &refs[0].content;
        assert!(listing.contains("visible.txt"));
        assert!(!listing.contains(".hidden"));
        assert!(!listing.contains("node_modules"));
    }

    /// Line ranges resolve and slice content.
    #[test]
    fn line_range_slices_file() {
        let cwd = tempfile::tempdir().unwrap();
        let content = (1..=20).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(cwd.path().join("nums.txt"), content).unwrap();
        let refs = resolve_references("@nums.txt:5-7", cwd.path());
        assert_eq!(refs.len(), 1);
        assert!(refs[0].content.contains("l5"));
        assert!(refs[0].content.contains("l7"));
        assert!(!refs[0].content.contains("l4"));
        assert!(!refs[0].content.contains("l8"));
    }

    /// Missing files silently skip (model gets nothing, no error injected).
    #[test]
    fn missing_files_silently_skipped() {
        let cwd = tempfile::tempdir().unwrap();
        let refs = resolve_references("read @nonexistent.txt", cwd.path());
        assert!(refs.is_empty(),
            "missing file must produce zero refs, not a fake one");
    }

    /// Empty refs list → empty formatted output.
    #[test]
    fn format_empty_refs_returns_empty_string() {
        assert_eq!(format_references_for_prompt(&[]), "");
    }

    /// Single file ref is formatted with a code fence.
    #[test]
    fn format_single_file_uses_code_fence() {
        let r = ResolvedRef {
            raw: "@foo.txt".into(),
            path: "foo.txt".into(),
            kind: RefKind::File,
            content: "hello".into(),
            lines: 1,
        };
        let out = format_references_for_prompt(&[r]);
        assert!(out.contains("[file] foo.txt"));
        assert!(out.contains("```\nhello\n```"));
        assert!(out.contains("Referenced files"));
    }
}
