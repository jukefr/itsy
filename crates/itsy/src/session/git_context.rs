//! Injects git diff context into the
//! prompt when the user mentions diffs, branches, or recent commits.

use std::path::Path;
use std::process::Command;

pub fn should_inject_git_context(message: &str) -> bool {
    let lc = message.to_lowercase();
    ["diff", "git", "branch", "commit", "staged", "uncommitted"]
        .iter()
        .any(|kw| lc.contains(kw))
}

pub fn get_git_diff_context(cwd: &Path) -> Option<String> {
    let status = Command::new("git").args(["status", "--short"]).current_dir(cwd).output().ok()?;
    if !status.status.success() {
        return None;
    }
    let diff = Command::new("git")
        .args(["diff", "--stat", "--shortstat"])
        .current_dir(cwd)
        .output()
        .ok()?;
    let mut out = String::new();
    out.push_str("git status:\n");
    out.push_str(&String::from_utf8_lossy(&status.stdout));
    if !diff.stdout.is_empty() {
        out.push_str("\ngit diff stat:\n");
        out.push_str(&String::from_utf8_lossy(&diff.stdout));
    }
    Some(out)
}
