//! Tracks checkpoint state
//! across long-running task runs.

use std::collections::HashMap;

use parking_lot::Mutex;

#[derive(Debug, Default)]
pub struct CheckpointStore {
    pub points: Mutex<HashMap<String, serde_json::Value>>,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, name: &str, value: serde_json::Value) {
        self.points.lock().insert(name.to_string(), value);
    }

    pub fn get(&self, name: &str) -> Option<serde_json::Value> {
        self.points.lock().get(name).cloned()
    }
}
