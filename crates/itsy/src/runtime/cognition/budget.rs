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

#[cfg(test)]
mod tests {
    use super::*;

    /// Charging accumulates tokens; USD increases proportionally with cost class.
    #[test]
    fn charge_accumulates_tokens_and_usd() {
        let mut b = Budget::new("t");
        b.charge(1000, CostClass::Small);
        assert_eq!(b.tokens(), 1000);
        // 1000 tokens × (1/1000) × 0.001 floor = 0.001 USD
        assert!((b.usd() - 0.001).abs() < 1e-9, "got {}", b.usd());

        b.charge(2000, CostClass::Small);
        assert_eq!(b.tokens(), 3000);
        assert!((b.usd() - 0.003).abs() < 1e-9);
    }

    /// Charging zero tokens is a no-op (no rounding artifacts on usd).
    #[test]
    fn charge_zero_is_noop() {
        let mut b = Budget::new("t");
        b.charge(0, CostClass::Large);
        assert_eq!(b.tokens(), 0);
        assert_eq!(b.usd(), 0.0);
    }

    /// Refunding more than charged saturates at zero (never goes negative).
    /// Anti-regression for "budget went negative after refund" bugs.
    #[test]
    fn refund_saturates_at_zero() {
        let mut b = Budget::new("t");
        b.charge(500, CostClass::Tiny);
        b.refund(10_000, CostClass::Tiny);
        assert_eq!(b.tokens(), 0, "tokens must saturate at zero");
        assert_eq!(b.usd(), 0.0, "usd must saturate at zero");
    }

    /// Cost class affects USD rate proportionally: Large >> Medium >> Small >> Tiny.
    #[test]
    fn cost_class_ordering_is_monotonic() {
        let mut tiny = Budget::new("t");
        let mut small = Budget::new("s");
        let mut medium = Budget::new("m");
        let mut large = Budget::new("l");
        tiny.charge(1000, CostClass::Tiny);
        small.charge(1000, CostClass::Small);
        medium.charge(1000, CostClass::Medium);
        large.charge(1000, CostClass::Large);
        assert!(tiny.usd() < small.usd());
        assert!(small.usd() < medium.usd());
        assert!(medium.usd() < large.usd());
    }

    /// `assert_can_spend` returns Ok when no env limits are set (the common
    /// case in production where budget enforcement is opt-in).
    /// This test asserts `Ok` only when env vars are unset — otherwise
    /// it would race with parallel tests that might set them.
    #[test]
    fn assert_can_spend_ok_when_no_limits_configured() {
        if read_limit("LLM_BUDGET_TOKENS_PER_TRACE").is_some()
            || read_limit("LLM_BUDGET_USD_PER_TRACE").is_some() {
            return; // env contaminated; skip
        }
        let b = Budget::new("t");
        assert!(b.assert_can_spend(1_000_000, CostClass::Large).is_ok(),
            "with no limits, any spend must be allowed");
    }

    /// `projected_usd` is pure arithmetic — same input, same output.
    #[test]
    fn projected_usd_is_deterministic() {
        let b = Budget::new("t");
        let p1 = b.projected_usd(1000, CostClass::Medium);
        let p2 = b.projected_usd(1000, CostClass::Medium);
        assert_eq!(p1, p2);
        // 1000 tokens × (1/1000) × 0.01 floor = 0.01 USD
        assert!((p1 - 0.01).abs() < 1e-9);
    }
}
