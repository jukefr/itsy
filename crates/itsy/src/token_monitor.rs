//! Token usage monitor — tracks per-turn and total token cost across an
//! agent session and renders the `/tokens` slash-command output.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u32,
}

#[derive(Debug, Default, Clone)]
pub struct CallMetadata {
    pub new_turn: bool,
    pub is_tool_call: bool,
}

#[derive(Debug, Default)]
pub struct TokenMonitor {
    pub turns: Vec<TurnUsage>,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub total_calls: u64,
    pub compactions: u64,
    pub evictions: u64,
    next_call_is_new_turn: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenMetrics {
    pub total_calls: u64,
    pub total_tokens: u64,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub avg_prompt_per_call: u64,
    pub avg_completion_per_call: u64,
    pub efficiency: String,
    pub turns: usize,
    pub compactions: u64,
    pub evictions: u64,
}

impl TokenMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_call(&mut self, prompt: u64, completion: u64, meta: CallMetadata) {
        self.total_prompt += prompt;
        self.total_completion += completion;
        self.total_calls += 1;

        if self.turns.is_empty() || meta.new_turn || self.next_call_is_new_turn {
            self.turns.push(TurnUsage::default());
            self.next_call_is_new_turn = false;
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.calls += 1;
            turn.prompt_tokens += prompt;
            turn.completion_tokens += completion;
            if meta.is_tool_call {
                turn.tool_calls += 1;
            }
        }
    }

    pub fn mark_next_call_new_turn(&mut self) {
        self.next_call_is_new_turn = true;
    }

    pub fn record_compaction(&mut self) {
        self.compactions += 1;
    }

    pub fn record_eviction(&mut self) {
        self.evictions += 1;
    }

    pub fn get_metrics(&self) -> TokenMetrics {
        let total_tokens = self.total_prompt + self.total_completion;
        let avg_prompt = self.total_prompt.checked_div(self.total_calls).unwrap_or(0);
        let avg_completion = self.total_completion.checked_div(self.total_calls).unwrap_or(0);
        let efficiency_pct = if self.total_prompt > 0 {
            (self.total_completion as f64 / self.total_prompt as f64) * 100.0
        } else {
            0.0
        };
        TokenMetrics {
            total_calls: self.total_calls,
            total_tokens,
            total_prompt: self.total_prompt,
            total_completion: self.total_completion,
            avg_prompt_per_call: avg_prompt,
            avg_completion_per_call: avg_completion,
            efficiency: format!("{:.1}%", efficiency_pct),
            turns: self.turns.len(),
            compactions: self.compactions,
            evictions: self.evictions,
        }
    }

    pub fn format_short(&self) -> String {
        let m = self.get_metrics();
        format!("{} tok ({} calls, {} eff)", m.total_tokens, m.total_calls, m.efficiency)
    }

    pub fn format_full(&self) -> String {
        let m = self.get_metrics();
        format!(
            "Token Usage Report\n  Total: {} tokens ({} prompt + {} completion)\n  Calls: {} ({} turns)\n  Avg/call: {} prompt, {} completion\n  Efficiency: {} (completion / prompt ratio)\n  Compactions: {} | Evictions: {}",
            m.total_tokens,
            m.total_prompt,
            m.total_completion,
            m.total_calls,
            m.turns,
            m.avg_prompt_per_call,
            m.avg_completion_per_call,
            m.efficiency,
            m.compactions,
            m.evictions,
        )
    }
}
