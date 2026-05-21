//! Tracks evidence (file paths, snippets)
//! that the agent has collected during a turn for later citation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceLog {
    pub entries: Vec<EvidenceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub kind: String,
    pub path: Option<String>,
    pub snippet: Option<String>,
    pub note: Option<String>,
}

impl EvidenceLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, path: impl Into<String>, note: impl Into<String>) {
        self.entries.push(EvidenceEntry { kind: "file".into(), path: Some(path.into()), snippet: None, note: Some(note.into()) });
    }

    pub fn add_snippet(&mut self, path: impl Into<String>, snippet: impl Into<String>) {
        self.entries.push(EvidenceEntry { kind: "snippet".into(), path: Some(path.into()), snippet: Some(snippet.into()), note: None });
    }

    pub fn add_note(&mut self, note: impl Into<String>) {
        self.entries.push(EvidenceEntry { kind: "note".into(), path: None, snippet: None, note: Some(note.into()) });
    }

    pub fn format_for_prompt(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nEvidence collected this turn:\n");
        for e in &self.entries {
            match (&e.path, &e.note) {
                (Some(p), Some(n)) => out.push_str(&format!("- {p}: {n}\n")),
                (Some(p), None) => out.push_str(&format!("- {p}\n")),
                (None, Some(n)) => out.push_str(&format!("- {n}\n")),
                _ => {}
            }
        }
        out
    }
}
