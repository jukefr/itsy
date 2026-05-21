//! Tracks tool trust scores with simple
//! exponential decay between successes.

use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct TrustState {
    pub scores: HashMap<String, f64>,
}

impl TrustState {
    pub fn record(&mut self, tool: &str, success: bool) {
        let s = self.scores.entry(tool.to_string()).or_insert(0.6);
        if success {
            *s = (*s * 0.7 + 0.3).min(0.99);
        } else {
            *s = (*s * 0.7 + 0.0).max(0.0);
        }
    }

    pub fn get(&self, tool: &str) -> f64 {
        *self.scores.get(tool).unwrap_or(&0.6)
    }
}
