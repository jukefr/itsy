//! Identical tool-call dedup: suppress repeated
//! calls with the same args within a small window.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ToolDedup {
    window: Duration,
    capacity: usize,
    seen: VecDeque<(String, String, Instant)>,
}

impl ToolDedup {
    pub fn new() -> Self {
        Self { window: Duration::from_secs(30), capacity: 32, seen: VecDeque::new() }
    }

    pub fn is_duplicate(&mut self, name: &str, args_json: &str) -> bool {
        let now = Instant::now();
        while let Some((_, _, t)) = self.seen.front() {
            if now.duration_since(*t) > self.window {
                self.seen.pop_front();
            } else {
                break;
            }
        }
        let dup = self.seen.iter().any(|(n, a, _)| n == name && a == args_json);
        if !dup {
            if self.seen.len() == self.capacity {
                self.seen.pop_front();
            }
            self.seen.push_back((name.to_string(), args_json.to_string(), now));
        }
        dup
    }
}

impl Default for ToolDedup {
    fn default() -> Self {
        Self::new()
    }
}
