//! Session persistence: save and
//! resume conversations under `.itsy/sessions/`.

use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSIONS_DIR: &str = ".itsy/sessions";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub meta: Value,
}

pub struct SessionStore {
    pub root_dir: PathBuf,
    pub current: Option<SessionRecord>,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        let dir = root.join(SESSIONS_DIR);
        let _ = fs::create_dir_all(&dir);
        Self { root_dir: dir, current: None }
    }

    pub fn create(&mut self) -> &SessionRecord {
        let id = short_id();
        let now = Utc::now().to_rfc3339();
        let rec = SessionRecord {
            id,
            title: None,
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
            meta: Value::Null,
        };
        self.current = Some(rec);
        self.current.as_ref().unwrap()
    }

    pub fn save(&mut self, messages: &[Value], meta: Value) {
        let now = Utc::now().to_rfc3339();
        if self.current.is_none() {
            self.create();
        }
        if let Some(rec) = self.current.as_mut() {
            rec.messages = messages.to_vec();
            rec.meta = meta;
            rec.updated_at = now;
            let path = self.root_dir.join(format!("{}.json", rec.id));
            if let Ok(json) = serde_json::to_string_pretty(rec) {
                let _ = fs::write(path, json);
            }
        }
    }

    pub fn auto_title(&mut self, messages: &[Value]) {
        let current_title_empty = self
            .current
            .as_ref()
            .map(|r| r.title.is_none())
            .unwrap_or(false);
        if !current_title_empty {
            return;
        }
        let first_user = messages.iter().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));
        if let Some(msg) = first_user {
            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                let title: String = content.chars().take(60).collect();
                if let Some(rec) = self.current.as_mut() {
                    rec.title = Some(title);
                }
            }
        }
    }

    pub fn list(&self) -> Vec<SessionRecord> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.root_dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(rec) = serde_json::from_str::<SessionRecord>(&content) {
                            out.push(rec);
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    pub fn load(&mut self, id: &str) -> Option<&SessionRecord> {
        let path = self.root_dir.join(format!("{id}.json"));
        let content = fs::read_to_string(&path).ok()?;
        let rec: SessionRecord = serde_json::from_str(&content).ok()?;
        self.current = Some(rec);
        self.current.as_ref()
    }
}

fn short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(12);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}
