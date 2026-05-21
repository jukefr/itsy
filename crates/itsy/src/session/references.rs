//! Resolves @-prefixed file references
//! in user messages and formats them for the prompt.

use std::fs;
use std::path::{Path, PathBuf};

use crate::security::{safe_resolve_path, PathOptions};

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub raw: String,
    pub path: String,
    pub content: String,
}

pub fn resolve_references(message: &str, cwd: &Path) -> Vec<ResolvedRef> {
    let re = regex::Regex::new(r"@([\w./-]+)").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(message) {
        let raw = cap[0].to_string();
        let p = cap[1].to_string();
        if let Ok(safe) = safe_resolve_path(&p, cwd, PathOptions::default()) {
            if let Ok(content) = fs::read_to_string(&safe.full_path) {
                out.push(ResolvedRef {
                    raw,
                    path: safe.display_path,
                    content,
                });
            }
        }
    }
    out
}

pub fn format_references_for_prompt(refs: &[ResolvedRef]) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\nReferenced files:\n");
    for r in refs {
        out.push_str(&format!("--- {} ---\n{}\n", r.path, r.content));
    }
    out
}
