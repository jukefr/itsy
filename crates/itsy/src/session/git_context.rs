//! Auto git context.
//!
//! When the user mentions phrases like "fix tests", "fix the bug",
//! "what changed", etc., we automatically include recent git diff + log
//! context. Sub-process invocations use argv arrays (never a shell) so
//! repository paths with spaces or unusual characters are safe.

use std::path::Path;
use std::process::Command;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::security::sanitize_tool_output;

static TRIGGERS: Lazy<Vec<Regex>> = Lazy::new(|| {
    let raws: &[&str] = &[
        r"(?i)\b(fix|debug|broken|failing|error|bug)\b.*\b(test|spec|check)\b",
        r"(?i)\bwhat('s| is| did).*chang",
        r"(?i)\brecent (change|commit|edit|update)",
        r"(?i)\bfix (the|this|my)\b",
        r"(?i)\bwhy (is|does|did).*fail",
        r"(?i)\brevert\b",
        r"(?i)\blast (change|commit|edit)",
    ];
    raws.iter().map(|r| Regex::new(r).unwrap()).collect()
});

/// Does this message imply the user wants context about recent changes?
pub fn should_inject_git_context(message: &str) -> bool {
    TRIGGERS.iter().any(|re| re.is_match(message))
}

/// Structured view of the repository state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitContext {
    pub branch: Option<String>,
    pub unstaged_stat: Option<String>,
    pub staged_stat: Option<String>,
    pub diff_excerpt: Option<String>,
    pub recent_commits: Vec<String>,
    pub last_commit: Option<String>,
    pub ignored_file_count: Option<usize>,
}

fn run_git(cwd: &Path, args: &[&str], timeout_secs: u64) -> Option<String> {
    let _ = timeout_secs; // std::process::Command has no built-in timeout.
    let out = Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--git-dir"], 3).is_some()
}

/// Collect the structured git context for `cwd`. Returns `None` outside a repo.
pub fn collect_git_context(cwd: &Path) -> Option<GitContext> {
    if !is_repo(cwd) {
        return None;
    }
    let mut ctx = GitContext::default();

    ctx.branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], 3)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(unstaged) = run_git(cwd, &["diff", "--stat", "--no-color"], 5) {
        let trimmed = unstaged.trim();
        if !trimmed.is_empty() {
            let lines: Vec<&str> = trimmed.lines().collect();
            let capped = if lines.len() > 40 {
                let mut s = lines[..40].join("\n");
                s.push_str(&format!("\n... ({} more files)", lines.len() - 40));
                s
            } else {
                trimmed.to_string()
            };
            ctx.unstaged_stat = Some(sanitize_tool_output(&capped));
        }
    }

    if let Some(staged) = run_git(cwd, &["diff", "--cached", "--stat", "--no-color"], 5) {
        let trimmed = staged.trim();
        if !trimmed.is_empty() {
            ctx.staged_stat = Some(sanitize_tool_output(trimmed));
        }
    }

    if let Some(commits) = run_git(cwd, &["log", "--oneline", "-5"], 3) {
        ctx.recent_commits = commits
            .lines()
            .map(|l| sanitize_tool_output(l.trim()))
            .filter(|l| !l.is_empty())
            .collect();
        ctx.last_commit = ctx.recent_commits.first().cloned();
    }

    if let Some(ignored) = run_git(cwd, &["ls-files", "--others", "-i", "--exclude-standard"], 5) {
        let n = ignored.lines().filter(|l| !l.trim().is_empty()).count();
        ctx.ignored_file_count = Some(n);
    }

    Some(ctx)
}

/// Build the formatted prompt block used by the agent loop. `max_diff_lines`
/// caps the inline diff excerpt size.
pub fn get_git_diff_context_full(cwd: &Path, max_diff_lines: usize) -> Option<String> {
    let mut ctx = collect_git_context(cwd)?;

    // Diff excerpt — fetched lazily because it can be huge.
    if ctx.unstaged_stat.is_some() {
        if let Some(full) = run_git(cwd, &["diff", "--no-color"], 5) {
            let lines: Vec<&str> = full.lines().collect();
            let total = lines.len();
            let take = max_diff_lines.min(total);
            let mut excerpt = lines[..take].join("\n");
            if total > take {
                excerpt.push_str(&format!("\n... ({} more lines)", total - take));
            }
            if !excerpt.is_empty() {
                ctx.diff_excerpt = Some(sanitize_tool_output(&excerpt));
            }
        }
    }

    Some(format_context(&ctx))
}

/// Back-compat helper used by older callers — defaults to 100 lines of diff.
pub fn get_git_diff_context(cwd: &Path) -> Option<String> {
    get_git_diff_context_full(cwd, 100)
}

fn format_context(ctx: &GitContext) -> String {
    let mut body = String::new();
    if let Some(b) = &ctx.branch {
        body.push_str(&format!("Branch: {}\n", b));
    }
    if let Some(s) = &ctx.unstaged_stat {
        body.push_str(&format!("\nUnstaged changes:\n{}\n", s));
    }
    if let Some(d) = &ctx.diff_excerpt {
        body.push_str(&format!("\n{}\n", d));
    }
    if let Some(s) = &ctx.staged_stat {
        body.push_str(&format!("\nStaged changes:\n{}\n", s));
    }
    if !ctx.recent_commits.is_empty() {
        body.push_str("\nRecent commits:\n");
        for c in &ctx.recent_commits {
            body.push_str(&format!("  {}\n", c));
        }
    }
    if let Some(n) = ctx.ignored_file_count {
        if n > 0 {
            body.push_str(&format!("\nIgnored files: {}\n", n));
        }
    }
    if body.trim().is_empty() {
        return String::new();
    }
    format!("\n\n--- Recent git changes ---\n{}\n", body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_match_known_phrases() {
        assert!(should_inject_git_context("can you fix the tests please"));
        assert!(should_inject_git_context("what changed in the last commit?"));
        assert!(should_inject_git_context("revert that"));
        assert!(should_inject_git_context("why does the build fail"));
        assert!(!should_inject_git_context("hello there"));
    }
}
