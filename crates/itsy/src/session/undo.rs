//! LIFO stack of reversible operations.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub enum UndoOp {
    Write { path: String, prev: Option<String> },
    Delete { path: String, prev: Option<String> },
    Rename { from: String, to: String },
}

pub struct UndoStack {
    stack: VecDeque<UndoOp>,
    capacity: usize,
}

impl UndoStack {
    pub fn new() -> Self {
        Self { stack: VecDeque::new(), capacity: 100 }
    }

    pub fn push(&mut self, op: UndoOp) {
        if self.stack.len() == self.capacity {
            self.stack.pop_front();
        }
        self.stack.push_back(op);
    }

    pub fn pop(&mut self) -> Option<UndoOp> {
        self.stack.pop_back()
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new()
    }
}
