//! Smart File-Tree Pruning.
//!
//! Scores and ranks files by relevance so the model sees the most useful
//! subset of a large repo. Mirrors the upstream JS implementation:
//!
//! Scoring heuristics (higher = more relevant):
//!   +3  recently modified (mtime within last 24h)
//!   +2  recently modified (mtime within last 7d)
//!   +1  recently modified (mtime within last 30d)
//!   +2  source file extension (.py .js .ts .go .rs .java etc.)
//!   +1  test file (test_*.py, *.test.js, *_test.go etc.)
//!   +1  config/manifest file (package.json, Cargo.toml, go.mod etc.)
//!   -2  generated/build output (*.min.js, *.map, package-lock.json, ...)
//!   +bonus for files matching the current task keywords (cap +4)
//!
//! Skips well-known dependency/build directories.
//!
//! Configuration:
//!   ITSY_FILETREE_MAX=50         max files to return
//!   ITSY_FILETREE_SORT=mtime|score   sort mode (default: score)

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;

const DEFAULT_MAX: usize = 50;
const DEFAULT_MAX_DEPTH: usize = 6;
const DEFAULT_MAX_COLLECT: usize = 2000;

static SOURCE_EXTS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "py", "js", "ts", "jsx", "tsx", "mjs", "cjs", "go", "rs", "java", "kt", "scala", "swift",
        "c", "cpp", "h", "hpp", "cs", "fs", "fsx", "rb", "php", "lua", "r", "jl", "ex", "exs",
        "sh", "bash", "zsh", "fish", "ps1", "yaml", "yml", "toml", "json", "xml", "env", "md",
        "txt", "rst", "adoc", "html", "css", "scss", "less", "vue", "svelte", "sql", "graphql",
        "proto",
    ]
    .into_iter()
    .collect()
});

static CONFIG_FILES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "package.json", "package-lock.json", "yarn.lock", "pnpm-lock.yaml",
        "tsconfig.json", "jsconfig.json", ".eslintrc.json", ".prettierrc",
        "Cargo.toml", "Cargo.lock", "go.mod", "go.sum",
        "pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", "Pipfile",
        "pom.xml", "build.gradle", "build.gradle.kts",
        "Makefile", "Dockerfile", "docker-compose.yml", "docker-compose.yaml",
        ".gitignore", ".gitattributes", "README.md", "CHANGELOG.md",
        "Gemfile", "Gemfile.lock", ".ruby-version", ".nvmrc",
    ]
    .into_iter()
    .collect()
});

static SKIP_DIRS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "node_modules", "vendor", ".git", ".svn", "__pycache__",
        ".venv", "venv", "env", ".env",
        "dist", "build", "out", "output", "target", ".next", ".nuxt",
        "coverage", ".nyc_output", ".pytest_cache", ".mypy_cache",
        "tmp", "temp", ".tmp", ".cache",
    ]
    .into_iter()
    .collect()
});

/// Returns true if the basename looks like a generated/uninteresting file.
fn is_generated(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".map")
        || lower.ends_with(".d.ts")
        || lower.ends_with(".pyc")
        || lower == "package-lock.json"
        || lower == "yarn.lock"
        || lower == "pnpm-lock.yaml"
}

/// Returns true if the basename looks like a test file.
fn is_test_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("test_")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_test.")
}

/// A single scored entry produced by `scored_file_listing`.
#[derive(Debug, Clone)]
pub struct ScoredEntry {
    pub rel: String,
    pub full: PathBuf,
    pub name: String,
    pub ext: String,
    pub score: i32,
    /// mtime as milliseconds since UNIX epoch.
    pub mtime: u128,
}

#[derive(Debug, Clone, Copy)]
pub struct ListOpts {
    pub max_depth: usize,
    pub max_collect: usize,
    pub max: usize,
}

impl Default for ListOpts {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_collect: DEFAULT_MAX_COLLECT,
            max: env_max(),
        }
    }
}

fn env_max() -> usize {
    let v = crate::settings::get().filetree_max;
    if v > 0 { v } else { DEFAULT_MAX }
}

fn env_sort_mtime() -> bool {
    crate::settings::get().filetree_sort_mtime
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn icon_for(ext: &str, name: &str) -> &'static str {
    if CONFIG_FILES.contains(name) {
        return "[cfg]";
    }
    match ext {
        "rs" => "[rs]",
        "py" => "[py]",
        "js" | "mjs" | "cjs" | "jsx" => "[js]",
        "ts" | "tsx" => "[ts]",
        "go" => "[go]",
        "java" | "kt" | "scala" => "[jvm]",
        "c" | "cpp" | "h" | "hpp" => "[c++]",
        "cs" => "[cs]",
        "rb" => "[rb]",
        "php" => "[php]",
        "swift" => "[sw]",
        "md" | "rst" | "adoc" | "txt" => "[doc]",
        "json" | "toml" | "yaml" | "yml" | "xml" => "[data]",
        "html" | "css" | "scss" | "less" | "vue" | "svelte" => "[web]",
        "sh" | "bash" | "zsh" | "fish" | "ps1" => "[sh]",
        "sql" | "graphql" | "proto" => "[api]",
        _ => "[file]",
    }
}

/// Walk a directory and return scored file entries.
pub fn scored_file_listing(root: &Path, task_hint: &str, opts: ListOpts) -> Vec<ScoredEntry> {
    let now = now_ms();
    let tokens: Vec<String> = task_hint
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(String::from)
        .collect();

    let mut entries: Vec<ScoredEntry> = Vec::new();
    walk(root, root, 0, opts.max_depth, opts.max_collect, &tokens, now, &mut entries);
    entries
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_collect: usize,
    tokens: &[String],
    now: u128,
    out: &mut Vec<ScoredEntry>,
) {
    if depth > max_depth || out.len() >= max_collect {
        return;
    }
    let listing = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    for ent in listing.flatten() {
        if out.len() >= max_collect {
            break;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let file_type = match ent.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if SKIP_DIRS.contains(name.as_str()) {
                continue;
            }
            // Skip most dot-dirs but allow .marrow (matches upstream).
            if name.starts_with('.') && name != ".marrow" {
                continue;
            }
            walk(root, &ent.path(), depth + 1, max_depth, max_collect, tokens, now, out);
        } else if file_type.is_file() {
            let full = ent.path();
            let rel = full.strip_prefix(root).unwrap_or(&full).display().to_string();
            let ext = full
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();

            let mut score: i32 = 0;

            if SOURCE_EXTS.contains(ext.as_str()) {
                score += 2;
            }
            if CONFIG_FILES.contains(name.as_str()) {
                score += 1;
            }
            if is_test_file(&name) {
                score += 1;
            }
            if is_generated(&name) {
                score -= 2;
            }

            let mtime_ms = std::fs::metadata(&full)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);

            if mtime_ms > 0 {
                let age = now.saturating_sub(mtime_ms);
                if age < 86_400_000 {
                    score += 3;
                } else if age < 604_800_000 {
                    score += 2;
                } else if age < 2_592_000_000 {
                    score += 1;
                }
            }

            if !tokens.is_empty() {
                let rel_lower = rel.to_lowercase();
                let hits = tokens.iter().filter(|t| rel_lower.contains(t.as_str())).count() as i32;
                score += (hits * 2).min(4);
            }

            out.push(ScoredEntry {
                rel,
                full,
                name,
                ext,
                score,
                mtime: mtime_ms,
            });
        }
    }
}

fn sort_entries(entries: &mut [ScoredEntry]) {
    if env_sort_mtime() {
        entries.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    } else {
        entries.sort_by(|a, b| b.score.cmp(&a.score).then(b.mtime.cmp(&a.mtime)));
    }
}

/// Return a ranked file listing for injection into the model.
pub fn get_smart_listing(root: &Path, task_hint: &str, max: usize) -> Vec<ScoredEntry> {
    let opts = ListOpts { max, ..ListOpts::default() };
    let mut entries = scored_file_listing(root, task_hint, opts);
    sort_entries(&mut entries);
    entries.truncate(max);
    entries
}

/// Format a file listing for the model. Adds a language-stats summary footer.
///
/// Signature kept compatible with the previous stub: `(root, hint, max)`.
pub fn format_smart_listing(root: &Path, hint: &str, max: usize) -> String {
    let opts = ListOpts { max, ..ListOpts::default() };
    let mut all = scored_file_listing(root, hint, opts);
    let total = all.len();
    sort_entries(&mut all);

    if all.is_empty() {
        return "No source files found.".to_string();
    }

    let files: Vec<ScoredEntry> = all.iter().take(max).cloned().collect();

    let header = if total > files.len() {
        format!(
            "Top {} relevant files ({}+ total, use find_files for specific patterns):\n",
            files.len(),
            total
        )
    } else {
        format!("{} files:\n", files.len())
    };

    let body = files
        .iter()
        .map(|f| format!("{} {}", icon_for(&f.ext, &f.name), f.rel))
        .collect::<Vec<_>>()
        .join("\n");

    let stats = language_stats(&files);
    if stats.is_empty() {
        format!("{header}{body}")
    } else {
        format!("{header}{body}\n\nLanguages: {stats}")
    }
}

/// Build a compact "ext: count" summary string of the top languages.
fn language_stats(entries: &[ScoredEntry]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in entries {
        if !e.ext.is_empty() && SOURCE_EXTS.contains(e.ext.as_str()) {
            *counts.entry(e.ext.as_str()).or_insert(0) += 1;
        }
    }
    let mut pairs: Vec<(&&str, &usize)> = counts.iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    pairs
        .into_iter()
        .take(6)
        .map(|(ext, n)| format!(".{ext}:{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dir_message() {
        let tmp = tempfile::tempdir().unwrap();
        let out = format_smart_listing(tmp.path(), "", 10);
        assert_eq!(out, "No source files found.");
    }

    #[test]
    fn detects_source_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let out = format_smart_listing(tmp.path(), "", 10);
        assert!(out.contains("main.rs"));
        assert!(out.contains("Cargo.toml"));
    }

    #[test]
    fn skips_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("foo.js"), "x").unwrap();
        std::fs::write(tmp.path().join("real.js"), "y").unwrap();
        let entries = get_smart_listing(tmp.path(), "", 10);
        assert!(entries.iter().any(|e| e.name == "real.js"));
        assert!(!entries.iter().any(|e| e.rel.contains("node_modules")));
    }
}
