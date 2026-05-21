//! Structured project memory store with markdown
//! mirroring and keyword-based retrieval.

pub mod evidence;

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

pub const MEMORY_DIR: &str = ".itsy/memory";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    #[serde(rename = "type")]
    pub kind: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryObject {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default)]
    pub source: Option<Source>,
}

impl MemoryObject {
    fn new(kind: impl Into<String>, title: impl Into<String>, content: impl Into<String>, tags: Vec<String>) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: short_id(),
            kind: kind.into(),
            title: title.into(),
            content: content.into(),
            tags,
            relations: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            source: None,
        }
    }
}

fn short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(&bytes)
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{:02x}", b));
        }
        out
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    objects: Vec<MemoryObject>,
    #[serde(rename = "updatedAt", default)]
    updated_at: String,
}

pub struct MemoryStore {
    pub root_dir: PathBuf,
    mem_dir: PathBuf,
    objects: Vec<MemoryObject>,
}

impl MemoryStore {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        let root = root_dir.as_ref().to_path_buf();
        let mem_dir = root.join(MEMORY_DIR);
        let mut store = Self { root_dir: root, mem_dir, objects: Vec::new() };
        store.load();
        store
    }

    pub fn init(&mut self) -> bool {
        if !self.mem_dir.exists() {
            let _ = fs::create_dir_all(&self.mem_dir);
        }
        self.save();
        true
    }

    pub fn load(&mut self) {
        if !self.mem_dir.exists() {
            return;
        }
        let index = self.mem_dir.join("index.json");
        if !index.exists() {
            return;
        }
        if let Ok(content) = fs::read_to_string(&index) {
            if let Ok(file) = serde_json::from_str::<StoreFile>(&content) {
                self.objects = file.objects;
            }
        }
    }

    pub fn save(&self) {
        if !self.mem_dir.exists() {
            let _ = fs::create_dir_all(&self.mem_dir);
        }
        let file = StoreFile {
            version: 1,
            objects: self.objects.clone(),
            updated_at: Utc::now().to_rfc3339(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = fs::write(self.mem_dir.join("index.json"), json);
        }
        for obj in &self.objects {
            let filename = format!("{}-{}.md", obj.kind, obj.id);
            let md = format!(
                "# {}\n\nType: {}\nTags: {}\nCreated: {}\n\n{}\n",
                obj.title,
                obj.kind,
                obj.tags.join(", "),
                obj.created_at,
                obj.content,
            );
            let _ = fs::write(self.mem_dir.join(filename), md);
        }
    }

    pub fn remember(&mut self, kind: &str, title: &str, content: &str, tags: Vec<String>) -> MemoryObject {
        let obj = MemoryObject::new(kind, title, content, tags);
        self.objects.push(obj.clone());
        self.save();
        obj
    }

    pub fn load_for_task(&self, task_description: &str) -> Vec<MemoryObject> {
        if self.objects.is_empty() {
            return Vec::new();
        }
        let words: Vec<String> = task_description
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        let mut scored: Vec<(MemoryObject, u32)> = Vec::new();
        for obj in &self.objects {
            let title_lc = obj.title.to_lowercase();
            let text = format!("{} {} {}", obj.title, obj.content, obj.tags.join(" ")).to_lowercase();
            let mut score = 0u32;
            for word in &words {
                if word.len() < 3 {
                    continue;
                }
                if text.contains(word) {
                    score += 1;
                }
                if title_lc.contains(word) {
                    score += 3;
                }
                if obj.tags.iter().any(|t| t.contains(word)) {
                    score += 2;
                }
            }
            if score > 0 {
                scored.push((obj.clone(), score));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().take(5).map(|(o, _)| o).collect()
    }

    pub fn by_type(&self, kind: &str) -> Vec<MemoryObject> {
        self.objects.iter().filter(|o| o.kind == kind).cloned().collect()
    }

    pub fn all(&self) -> Vec<MemoryObject> {
        self.objects.clone()
    }

    pub fn get(&self, id: &str) -> Option<MemoryObject> {
        self.objects.iter().find(|o| o.id == id).cloned()
    }

    pub fn forget(&mut self, id: &str) -> bool {
        if let Some(idx) = self.objects.iter().position(|o| o.id == id) {
            let removed = self.objects.remove(idx);
            let filename = format!("{}-{}.md", removed.kind, removed.id);
            let _ = fs::remove_file(self.mem_dir.join(filename));
            self.save();
            true
        } else {
            false
        }
    }

    pub fn format_for_context(&self, objects: &[MemoryObject], max_tokens: usize) -> String {
        if objects.is_empty() {
            return String::new();
        }
        let mut out = String::from("<memory>\n");
        let mut tokens = 0;
        for obj in objects {
            let entry = format!("[{}] {}: {}\n", obj.kind, obj.title, obj.content);
            let entry_tokens = entry.len().div_ceil(4);
            if tokens + entry_tokens > max_tokens {
                break;
            }
            out.push_str(&entry);
            tokens += entry_tokens;
        }
        out.push_str("</memory>");
        out
    }

    pub fn stats(&self) -> MemoryStats {
        let mut by_type = std::collections::HashMap::new();
        for obj in &self.objects {
            *by_type.entry(obj.kind.clone()).or_insert(0) += 1;
        }
        MemoryStats { total: self.objects.len(), by_type }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryStats {
    pub total: usize,
    pub by_type: std::collections::HashMap<String, u32>,
}
