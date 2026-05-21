//! Skill system — reusable prompt templates that teach the model specific behaviors.
//!
//! Skills are markdown files with optional YAML frontmatter. They live in:
//!   - `.itsy/skills/`                   (project-level)
//!   - `~/.config/itsy/skills/`          (user-level / global)
//!
//! Skill format (markdown with YAML frontmatter):
//! ```text
//! ---
//! name: code-review
//! trigger: manual          # "manual" (via /skill), "auto" (always injected), "match" (keyword match)
//! keywords: [review, pr, quality]
//! ---
//! When reviewing code, follow these guidelines:
//! 1. Check for security issues first
//! 2. Then correctness
//! 3. Then style
//! ```
//!
//! Commands:
//!   /skill list          — show all available skills
//!   /skill add <name>    — create a new skill interactively
//!   /skill use <name>    — inject a skill into the current conversation
//!   /skill edit <name>   — edit an existing skill
//!   /skill remove <name> — delete a skill

use std::fs;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// How a skill is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Only via explicit `/skill use <name>`.
    Manual,
    /// Always injected into the prompt.
    Auto,
    /// Injected when one of `keywords` matches the user message.
    Match,
}

impl Trigger {
    pub fn from_str_lenient(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "auto" => Trigger::Auto,
            "match" => Trigger::Match,
            _ => Trigger::Manual,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Auto => "auto",
            Trigger::Match => "match",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub trigger: Trigger,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Short description (optional — kept for back-compat with older skills).
    #[serde(default)]
    pub description: String,
    /// The markdown body that gets injected into the prompt.
    pub content: String,
    /// Path to the source file on disk.
    pub path: PathBuf,
}

/// Lightweight summary returned by [`SkillManager::list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub trigger: Trigger,
    pub keywords: Vec<String>,
    pub preview: String,
}

#[derive(Debug, Default)]
pub struct SkillManager {
    project_dir: PathBuf,
    pub skills: Vec<Skill>,
}

static FRONTMATTER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)^---\n(.*?)\n---\n(.*)$").unwrap());
static META_LINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\w+):\s*(.+)$").unwrap());

impl SkillManager {
    pub fn new() -> Self {
        Self::with_project_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    pub fn with_project_dir<P: Into<PathBuf>>(project_dir: P) -> Self {
        let mut sm = Self { project_dir: project_dir.into(), skills: Vec::new() };
        sm.load();
        sm
    }

    fn skill_dirs(&self) -> Vec<PathBuf> {
        vec![crate::paths::skills_dir()]
    }

    /// Backward-compatible alias for old API. Resets the project dir and reloads.
    pub fn load_from(&mut self, root: &Path) {
        self.project_dir = root.to_path_buf();
        self.skills.clear();
        self.load();
    }

    fn load(&mut self) {
        for dir in self.skill_dirs() {
            if !dir.exists() {
                continue;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                let content = match fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let filename = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                if let Some(skill) = parse_skill(&content, &filename, &path) {
                    // Replace existing skill with same name (project overrides global).
                    self.skills.retain(|s| s.name != skill.name);
                    self.skills.push(skill);
                }
            }
        }
    }

    /// Return a summary view of all loaded skills.
    pub fn list(&self) -> Vec<SkillSummary> {
        self.skills
            .iter()
            .map(|s| {
                let preview = if s.content.chars().count() > 80 {
                    let cut: String = s.content.chars().take(80).collect();
                    format!("{}...", cut)
                } else {
                    s.content.clone()
                };
                SkillSummary {
                    name: s.name.clone(),
                    trigger: s.trigger,
                    keywords: s.keywords.clone(),
                    preview,
                }
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Skills that should auto-inject for the given message (auto + matching keyword skills).
    pub fn get_auto_skills(&self, message: &str) -> Vec<Skill> {
        let msg = message.to_lowercase();
        let mut out = Vec::new();
        for s in &self.skills {
            match s.trigger {
                Trigger::Auto => out.push(s.clone()),
                Trigger::Match if !s.keywords.is_empty() => {
                    if s.keywords.iter().any(|kw| msg.contains(&kw.to_lowercase())) {
                        out.push(s.clone());
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Create a new skill on disk and register it in memory.
    pub fn add(
        &mut self,
        name: &str,
        content: &str,
        trigger: Trigger,
        keywords: &[String],
    ) -> std::io::Result<Skill> {
        let dir = crate::paths::skills_dir();
        fs::create_dir_all(&dir)?;

        let mut frontmatter = String::from("---\n");
        frontmatter.push_str(&format!("name: {}\n", name));
        frontmatter.push_str(&format!("trigger: {}\n", trigger.as_str()));
        if !keywords.is_empty() {
            frontmatter.push_str(&format!("keywords: [{}]\n", keywords.join(", ")));
        }
        frontmatter.push_str("---\n");

        let full = format!("{}{}\n", frontmatter, content);
        let safe_name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let file_path = dir.join(format!("{}.md", safe_name));
        fs::write(&file_path, &full)?;

        let skill = Skill {
            name: name.to_string(),
            trigger,
            keywords: keywords.to_vec(),
            description: String::new(),
            content: content.to_string(),
            path: file_path,
        };
        self.skills.retain(|s| s.name != skill.name);
        self.skills.push(skill.clone());
        Ok(skill)
    }

    /// Delete a skill from disk and memory. Returns true on success.
    pub fn remove(&mut self, name: &str) -> bool {
        let pos = match self.skills.iter().position(|s| s.name == name) {
            Some(p) => p,
            None => return false,
        };
        let skill = self.skills.remove(pos);
        if skill.path.exists() {
            let _ = fs::remove_file(&skill.path);
        }
        true
    }

    /// Format a list of skills as a "Active skills:" prompt section.
    pub fn format_for_prompt(&self, skills: &[Skill]) -> String {
        if skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nActive skills:\n");
        let mut first = true;
        for s in skills {
            if !first {
                out.push_str("\n\n");
            }
            first = false;
            out.push_str(&format!("[{}] {}", s.name, s.content));
        }
        out
    }
}

fn parse_skill(content: &str, filename: &str, path: &Path) -> Option<Skill> {
    let stem = filename.trim_end_matches(".md").to_string();
    if let Some(caps) = FRONTMATTER_RE.captures(content) {
        let frontmatter = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let body = caps.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();

        let mut name = stem.clone();
        let mut trigger = Trigger::Manual;
        let mut keywords: Vec<String> = Vec::new();
        let mut description = String::new();

        for line in frontmatter.lines() {
            if let Some(m) = META_LINE_RE.captures(line) {
                let key = m.get(1).unwrap().as_str();
                let value = m.get(2).unwrap().as_str().trim();
                match key {
                    "name" => name = value.to_string(),
                    "trigger" => trigger = Trigger::from_str_lenient(value),
                    "description" => description = value.to_string(),
                    "keywords" => {
                        if value.starts_with('[') && value.ends_with(']') {
                            keywords = value[1..value.len() - 1]
                                .split(',')
                                .map(|s| {
                                    s.trim()
                                        .trim_matches('\'')
                                        .trim_matches('"')
                                        .to_string()
                                })
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                    }
                    _ => {}
                }
            }
        }

        Some(Skill {
            name,
            trigger,
            keywords,
            description,
            content: body,
            path: path.to_path_buf(),
        })
    } else {
        Some(Skill {
            name: stem,
            trigger: Trigger::Manual,
            keywords: Vec::new(),
            description: String::new(),
            content: content.trim().to_string(),
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let src = "---\nname: foo\ntrigger: match\nkeywords: [a, b, c]\n---\nbody text\n";
        let s = parse_skill(src, "foo.md", Path::new("foo.md")).unwrap();
        assert_eq!(s.name, "foo");
        assert_eq!(s.trigger, Trigger::Match);
        assert_eq!(s.keywords, vec!["a", "b", "c"]);
        assert_eq!(s.content, "body text");
    }

    #[test]
    fn no_frontmatter_uses_filename() {
        let s = parse_skill("plain body", "code-review.md", Path::new("x.md")).unwrap();
        assert_eq!(s.name, "code-review");
        assert_eq!(s.trigger, Trigger::Manual);
    }
}
