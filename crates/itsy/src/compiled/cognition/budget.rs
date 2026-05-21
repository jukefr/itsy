//! Per-trace token + USD budget
//! tracker with environment-configured ceilings.

use std::env;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub enum CostClass {
    Tiny,
    Small,
    Medium,
    Large,
}

fn cost_floor(c: CostClass) -> f64 {
    match c {
        CostClass::Tiny => 0.0001,
        CostClass::Small => 0.0010,
        CostClass::Medium => 0.0100,
        CostClass::Large => 0.0300,
    }
}

#[derive(Debug, Error)]
#[error("BudgetExceeded: trace {trace_id} {kind} (limit {limit}, observed {observed})")]
pub struct BudgetExceededError {
    pub trace_id: String,
    pub kind: &'static str,
    pub limit: f64,
    pub observed: f64,
}

fn read_limit(name: &str) -> Option<f64> {
    let raw = env::var(name).ok()?;
    let v: f64 = raw.parse().ok()?;
    if v.is_finite() && v > 0.0 { Some(v) } else { None }
}

#[derive(Debug)]
pub struct Budget {
    pub trace_id: String,
    tokens: u64,
    usd: f64,
    token_limit: Option<u64>,
    usd_limit: Option<f64>,
}

impl Budget {
    pub fn new(trace_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            tokens: 0,
            usd: 0.0,
            token_limit: read_limit("LLM_BUDGET_TOKENS_PER_TRACE").map(|v| v as u64),
            usd_limit: read_limit("LLM_BUDGET_USD_PER_TRACE"),
        }
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    pub fn usd(&self) -> f64 {
        self.usd
    }

    pub fn projected_usd(&self, add_tokens: u64, cost: CostClass) -> f64 {
        self.usd + (add_tokens as f64 / 1000.0) * cost_floor(cost)
    }

    pub fn assert_can_spend(&self, tokens: u64, cost: CostClass) -> Result<(), BudgetExceededError> {
        if let Some(tl) = self.token_limit {
            if self.tokens + tokens > tl {
                return Err(BudgetExceededError {
                    trace_id: self.trace_id.clone(),
                    kind: "tokens",
                    limit: tl as f64,
                    observed: (self.tokens + tokens) as f64,
                });
            }
        }
        if let Some(ul) = self.usd_limit {
            let projected = self.projected_usd(tokens, cost);
            if projected > ul {
                return Err(BudgetExceededError {
                    trace_id: self.trace_id.clone(),
                    kind: "usd",
                    limit: ul,
                    observed: projected,
                });
            }
        }
        Ok(())
    }

    pub fn charge(&mut self, tokens: u64, cost: CostClass) {
        if tokens == 0 {
            return;
        }
        self.tokens += tokens;
        self.usd += (tokens as f64 / 1000.0) * cost_floor(cost);
    }

    pub fn refund(&mut self, tokens: u64, cost: CostClass) {
        if tokens == 0 {
            return;
        }
        self.tokens = self.tokens.saturating_sub(tokens);
        self.usd = (self.usd - (tokens as f64 / 1000.0) * cost_floor(cost)).max(0.0);
    }
}

pub fn budget_enforced() -> bool {
    read_limit("LLM_BUDGET_TOKENS_PER_TRACE").is_some()
        || read_limit("LLM_BUDGET_USD_PER_TRACE").is_some()
}

pub fn budget_limits() -> (Option<u64>, Option<f64>) {
    (
        read_limit("LLM_BUDGET_TOKENS_PER_TRACE").map(|v| v as u64),
        read_limit("LLM_BUDGET_USD_PER_TRACE"),
    )
}
