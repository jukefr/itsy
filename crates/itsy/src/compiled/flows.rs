//! Holds named multi-step "flow"
//! descriptors. The JS version exports an empty record by default; we mirror
//! that surface for callers that import it.

use std::collections::HashMap;

pub type FlowStep = serde_json::Value;

#[derive(Default)]
pub struct FlowRegistry {
    pub flows: HashMap<String, Vec<FlowStep>>,
}

impl FlowRegistry {
    pub fn new() -> Self {
        Self { flows: HashMap::new() }
    }
}
