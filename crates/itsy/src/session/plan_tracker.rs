//! Tracks the agent's stated plan
//! (an enumerated TODO list) so we can re-anchor when the model drifts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub idx: u32,
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PlanTracker {
    pub items: Vec<PlanItem>,
}

impl PlanTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(text: &str) -> Vec<PlanItem> {
        let re = regex::Regex::new(r"^(?m)\s*(\d+)\.\s+(.+)$").unwrap();
        re.captures_iter(text)
            .filter_map(|c| {
                let idx: u32 = c[1].parse().ok()?;
                Some(PlanItem { idx, text: c[2].to_string(), done: false })
            })
            .collect()
    }

    pub fn set(&mut self, items: Vec<PlanItem>) {
        self.items = items;
    }

    pub fn mark_done(&mut self, idx: u32) {
        for item in &mut self.items {
            if item.idx == idx {
                item.done = true;
            }
        }
    }

    pub fn next_pending(&self) -> Option<&PlanItem> {
        self.items.iter().find(|i| !i.done)
    }

    pub fn pretty(&self) -> String {
        self.items
            .iter()
            .map(|i| format!("{}. [{}] {}", i.idx, if i.done { "x" } else { " " }, i.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
