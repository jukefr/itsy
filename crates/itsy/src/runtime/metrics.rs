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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inc_and_get_round_trip() {
        let m = Metrics::new();
        assert_eq!(m.get("unseen"), 0, "missing counter must read as 0");
        m.inc("foo");
        m.inc("foo");
        m.inc("foo");
        assert_eq!(m.get("foo"), 3);
    }

    #[test]
    fn counters_are_independent() {
        let m = Metrics::new();
        m.inc("a");
        m.inc("b");
        m.inc("a");
        assert_eq!(m.get("a"), 2);
        assert_eq!(m.get("b"), 1);
    }

    #[test]
    fn default_is_new() {
        let m = Metrics::default();
        assert_eq!(m.get("any"), 0);
    }
}
