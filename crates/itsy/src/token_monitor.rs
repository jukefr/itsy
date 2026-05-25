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

#[cfg(test)]
mod tests {
    use super::*;

    /// First call creates a new turn implicitly.
    #[test]
    fn first_call_creates_turn() {
        let mut m = TokenMonitor::new();
        m.record_call(10, 20, CallMetadata::default());
        assert_eq!(m.turns.len(), 1);
        assert_eq!(m.turns[0].calls, 1);
        assert_eq!(m.turns[0].prompt_tokens, 10);
        assert_eq!(m.turns[0].completion_tokens, 20);
    }

    /// `new_turn=true` starts a new turn each time.
    #[test]
    fn new_turn_flag_starts_fresh_turn() {
        let mut m = TokenMonitor::new();
        m.record_call(10, 5, CallMetadata::default());
        m.record_call(20, 10, CallMetadata { new_turn: true, is_tool_call: false });
        assert_eq!(m.turns.len(), 2);
        assert_eq!(m.turns[0].calls, 1);
        assert_eq!(m.turns[1].calls, 1);
        assert_eq!(m.turns[1].prompt_tokens, 20);
    }

    /// `mark_next_call_new_turn` triggers a turn boundary on the next call.
    #[test]
    fn mark_next_call_new_turn_takes_effect_once() {
        let mut m = TokenMonitor::new();
        m.record_call(1, 1, CallMetadata::default());
        m.mark_next_call_new_turn();
        m.record_call(2, 2, CallMetadata::default());
        m.record_call(3, 3, CallMetadata::default());
        // First → turn 0, second → turn 1 (mark), third → still turn 1 (mark consumed).
        assert_eq!(m.turns.len(), 2);
        assert_eq!(m.turns[1].calls, 2);
    }

    /// `is_tool_call=true` increments the per-turn tool-call counter.
    #[test]
    fn is_tool_call_bumps_tool_counter() {
        let mut m = TokenMonitor::new();
        m.record_call(10, 5, CallMetadata { new_turn: false, is_tool_call: true });
        m.record_call(10, 5, CallMetadata { new_turn: false, is_tool_call: false });
        m.record_call(10, 5, CallMetadata { new_turn: false, is_tool_call: true });
        assert_eq!(m.turns[0].tool_calls, 2);
        assert_eq!(m.turns[0].calls, 3);
    }

    /// Totals accumulate across all calls.
    #[test]
    fn totals_accumulate() {
        let mut m = TokenMonitor::new();
        m.record_call(100, 50, CallMetadata::default());
        m.record_call(200, 75, CallMetadata::default());
        assert_eq!(m.total_prompt, 300);
        assert_eq!(m.total_completion, 125);
        assert_eq!(m.total_calls, 2);
    }

    /// `record_compaction` / `record_eviction` increment dedicated counters.
    #[test]
    fn compaction_and_eviction_counters() {
        let mut m = TokenMonitor::new();
        m.record_compaction();
        m.record_compaction();
        m.record_eviction();
        assert_eq!(m.compactions, 2);
        assert_eq!(m.evictions, 1);
    }

    /// `get_metrics` on empty monitor returns zeros without divide-by-zero panic.
    #[test]
    fn metrics_on_empty_no_div_by_zero() {
        let m = TokenMonitor::new();
        let metrics = m.get_metrics();
        assert_eq!(metrics.total_calls, 0);
        assert_eq!(metrics.avg_prompt_per_call, 0);
        assert_eq!(metrics.avg_completion_per_call, 0);
        assert_eq!(metrics.efficiency, "0.0%");
    }

    /// Efficiency is correctly computed as completion / prompt ratio.
    #[test]
    fn efficiency_ratio_correct() {
        let mut m = TokenMonitor::new();
        m.record_call(1000, 500, CallMetadata::default());
        let metrics = m.get_metrics();
        assert_eq!(metrics.efficiency, "50.0%");
    }

    /// `format_short` summarises totals.
    #[test]
    fn format_short_summarises() {
        let mut m = TokenMonitor::new();
        m.record_call(100, 50, CallMetadata::default());
        let s = m.format_short();
        assert!(s.contains("150"), "total tokens must appear; got {s}");
        assert!(s.contains("1"), "call count must appear; got {s}");
    }

    /// `format_full` includes all metric labels.
    #[test]
    fn format_full_includes_labels() {
        let mut m = TokenMonitor::new();
        m.record_call(100, 50, CallMetadata::default());
        m.record_compaction();
        let s = m.format_full();
        for label in ["Token Usage Report", "Total:", "Calls:", "Avg/call:",
                      "Efficiency:", "Compactions:", "Evictions:"] {
            assert!(s.contains(label), "{label} missing from full report; got: {s}");
        }
    }
}
