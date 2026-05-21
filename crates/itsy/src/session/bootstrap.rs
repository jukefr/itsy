//! Generates a brief "project bootstrap"
//! summary that gets injected into the first message of a new session.

use std::fs;
use std::path::Path;

pub fn bootstrap_context(cwd: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("cwd: {}\n", cwd.display()));
    if let Ok(entries) = fs::read_dir(cwd) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names.truncate(30);
        out.push_str(&format!("entries: {}\n", names.join(", ")));
    }
    let markers = [
        ("package.json", "Node project"),
        ("Cargo.toml", "Rust project"),
        ("pyproject.toml", "Python (PEP 518)"),
        ("go.mod", "Go module"),
        ("Gemfile", "Ruby"),
    ];
    for (file, label) in &markers {
        if cwd.join(file).exists() {
            out.push_str(&format!("kind: {label}\n"));
            break;
        }
    }
    out
}
