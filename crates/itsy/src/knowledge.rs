//! Loads bundled knowledge files from the
//! `knowledge/` directory at the workspace root.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct KnowledgeDoc {
    pub key: String,
    pub content: String,
}

pub fn load_all(root: &Path) -> Vec<KnowledgeDoc> {
    let mut out = Vec::new();
    let dir = root.join("knowledge");
    if !dir.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
        if entry.file_type().is_file() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext == "md" || ext == "txt" {
                if let Ok(content) = fs::read_to_string(path) {
                    let key = path.strip_prefix(&dir).unwrap_or(path).to_string_lossy().into_owned();
                    out.push(KnowledgeDoc { key, content });
                }
            }
        }
    }
    out
}
