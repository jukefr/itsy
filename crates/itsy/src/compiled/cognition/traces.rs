//! In-memory trace buffer.

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    pub trace_id: String,
    pub at: String,
    pub kind: String,
    pub data: serde_json::Value,
}

pub struct TraceBuffer {
    events: Mutex<VecDeque<TraceEvent>>,
    capacity: usize,
}

impl TraceBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { events: Mutex::new(VecDeque::with_capacity(capacity)), capacity }
    }

    pub fn record(&self, trace_id: &str, kind: &str, data: serde_json::Value) {
        let mut g = self.events.lock();
        if g.len() >= self.capacity {
            g.pop_front();
        }
        g.push_back(TraceEvent {
            trace_id: trace_id.to_string(),
            at: Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            data,
        });
    }

    pub fn dump(&self) -> Vec<TraceEvent> {
        self.events.lock().iter().cloned().collect()
    }
}

impl Default for TraceBuffer {
    fn default() -> Self {
        Self::new(1024)
    }
}
