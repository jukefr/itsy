//! Agent loop state machine — `TurnState` + guard methods.
//!
//! Extracted from `bin/itsy.rs::handle_turn` during Phase B. The struct
//! encapsulates all per-turn mutable state; guard methods return
//! [`GuardAction`] to drive the loop.

use std::collections::{HashMap, HashSet};

/// Action a guard can take when it fires.
pub enum GuardAction {
    /// Inject a system message and continue the inner loop.
    Inject { message: String },
    /// Break out of the inner tool-batch loop.
    Break,
    /// Break out of the outer turn loop.
    Stop,
}

/// Per-turn mutable state.
///
/// Created at the start of every [`handle_turn`] call and dropped at the end.
/// Session-scoped counters (e.g. `tool_repeat_counts`, `readonly_turn_count`)
/// stay on [`AgentSession`].
pub struct TurnState {
    // ── config ──────────────────────────────────────────────────
    pub user_msg: String,
    pub augmented: String,
    pub task_type: &'static str,
    pub current_category: Option<String>,

    // ── counters ────────────────────────────────────────────────
    pub tool_calls_this_turn: u32,
    pub max_tool_calls: u32,
    pub consecutive_blocks: u32,
    pub last_prompt_tokens: u64,
    pub force_disable_thinking: bool,
    pub had_mutating_call: bool,
    pub empty_retry_injected: bool,

    // ── tracking ────────────────────────────────────────────────
    pub edited_files: Vec<String>,
    pub improvement_attempts: HashMap<String, u32>,
    pub per_turn_repeats: HashMap<String, u32>,
    pub per_turn_write_seen: HashSet<String>,
    pub recent_tool_failures: HashMap<String, u32>,
}

impl TurnState {
    /// Create initial state for a new turn.
    pub fn new(user_msg: String, task_type: &'static str, stage2_category: Option<String>, max_tool_calls: u32) -> Self {
        Self {
            augmented: user_msg.clone(),
            user_msg,
            task_type,
            current_category: stage2_category,
            tool_calls_this_turn: 0,
            max_tool_calls,
            consecutive_blocks: 0,
            last_prompt_tokens: 0,
            force_disable_thinking: false,
            had_mutating_call: false,
            empty_retry_injected: false,
            edited_files: Vec::new(),
            improvement_attempts: HashMap::new(),
            per_turn_repeats: HashMap::new(),
            per_turn_write_seen: HashSet::new(),
            recent_tool_failures: HashMap::new(),
        }
    }

    /// Check whether the outer turn loop should stop.
    pub fn should_stop(&self) -> bool {
        self.tool_calls_this_turn >= self.max_tool_calls
    }
}
