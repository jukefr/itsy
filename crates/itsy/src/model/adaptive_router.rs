//! Learns which routing tier works
//! best per task type based on success/failure history.

use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct AdaptiveRouter {
    pub tier_scores: HashMap<String, HashMap<String, f64>>,
}

impl AdaptiveRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, task_type: &str, tier: &str, success: bool) {
        let map = self.tier_scores.entry(task_type.to_string()).or_default();
        let s = map.entry(tier.to_string()).or_insert(0.6);
        if success {
            *s = (*s * 0.7 + 0.3).min(0.99);
        } else {
            *s = (*s * 0.7 + 0.0).max(0.0);
        }
    }

    pub fn best_tier(&self, task_type: &str) -> Option<String> {
        let map = self.tier_scores.get(task_type)?;
        map.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)).map(|(k, _)| k.clone())
    }
}
