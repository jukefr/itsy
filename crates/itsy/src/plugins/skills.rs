//! Reads `.itsy/skills/*.md` skill
//! files and selects relevant ones to inject into the system prompt.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SkillManager {
    pub skills: Vec<Skill>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from(&mut self, root: &Path) {
        let dir = root.join(".itsy").join("skills");
        if !dir.exists() {
            return;
        }
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                self.skills.push(parse_skill(&content, entry.file_name().to_string_lossy().as_ref()));
            }
        }
    }

    pub fn get_auto_skills(&self, message: &str) -> Vec<Skill> {
        let lc = message.to_lowercase();
        self.skills
            .iter()
            .filter(|s| s.keywords.iter().any(|k| lc.contains(k)))
            .cloned()
            .collect()
    }

    pub fn format_for_prompt(&self, skills: &[Skill]) -> String {
        if skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nRelevant skills:\n");
        for s in skills {
            out.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        out
    }
}

fn parse_skill(content: &str, file_name: &str) -> Skill {
    let mut name = file_name.trim_end_matches(".md").to_string();
    let mut description = String::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut body = String::new();
    let mut in_front = false;
    let mut consumed_front = false;
    for (i, line) in content.lines().enumerate() {
        if i == 0 && line.trim() == "---" {
            in_front = true;
            continue;
        }
        if in_front && line.trim() == "---" {
            in_front = false;
            consumed_front = true;
            continue;
        }
        if in_front {
            if let Some(rest) = line.strip_prefix("name:") {
                name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("description:") {
                description = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("keywords:") {
                keywords = rest
                    .trim_matches(|c: char| c == '[' || c == ']')
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            continue;
        }
        if consumed_front || i > 0 {
            body.push_str(line);
            body.push('\n');
        }
    }
    Skill { name, description, body, keywords }
}
