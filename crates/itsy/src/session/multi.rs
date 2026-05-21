//! Multi-session orchestrator: lets the agent
//! work on several conversations in parallel and pick which is active.

use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct MultiSession {
    pub sessions: Mutex<HashMap<String, serde_json::Value>>,
    pub active: Mutex<Option<String>>,
}

impl MultiSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_active(&self, id: &str) {
        *self.active.lock() = Some(id.to_string());
    }

    pub fn record(&self, id: &str, value: serde_json::Value) {
        self.sessions.lock().insert(id.to_string(), value);
    }

    pub fn get(&self, id: &str) -> Option<serde_json::Value> {
        self.sessions.lock().get(id).cloned()
    }
}
