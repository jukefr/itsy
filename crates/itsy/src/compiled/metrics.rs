//! In-process metric counters; the JS
//! version is similarly thin.

use parking_lot::Mutex;
use std::collections::HashMap;

pub struct Metrics {
    counters: Mutex<HashMap<String, u64>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self { counters: Mutex::new(HashMap::new()) }
    }

    pub fn inc(&self, name: &str) {
        let mut g = self.counters.lock();
        *g.entry(name.to_string()).or_insert(0) += 1;
    }

    pub fn get(&self, name: &str) -> u64 {
        *self.counters.lock().get(name).unwrap_or(&0)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
