//! Adaptive Retry Temperature.
//!
//! When the improvement loop retries a failed edit, the default temperature
//! (usually 0.3–0.7) is used for every attempt. This causes the model to
//! produce nearly identical outputs: same strategy, same mistake.
//!
//! We nudge temperature differently per attempt:
//!   - attempt 1 (initial fail)  → lower: become more deterministic
//!   - attempt 2 (still failing) → higher: explore a different approach
//!   - attempt 3+  → back to original: the deterministic retry might finally work
//!
//! Applied as a DELTA on top of the configured temperature, never as an
//! absolute value, so user config remains the anchor.
//!
//! Configuration via environment:
//!   - `ITSY_TEMP_ADAPT=false`  disable entirely
//!   - `ITSY_TEMP_DELTA=0.15`   shift per attempt (default 0.15)
//!   - `ITSY_TEMP_MAX=1.0`      upper clamp (default 1.0)
//!   - `ITSY_TEMP_MIN=0.0`      lower clamp (default 0.0)

use std::env;

use once_cell::sync::Lazy;
use serde_json::{Map, Value};

fn parse_env_f64(key: &str, default: f64) -> f64 {
    match env::var(key) {
        Ok(s) => s.parse::<f64>().unwrap_or(default),
        Err(_) => default,
    }
}

pub static DELTA: Lazy<f64> = Lazy::new(|| parse_env_f64("ITSY_TEMP_DELTA", 0.15));
pub static MAX_T: Lazy<f64> = Lazy::new(|| parse_env_f64("ITSY_TEMP_MAX", 1.0));
pub static MIN_T: Lazy<f64> = Lazy::new(|| parse_env_f64("ITSY_TEMP_MIN", 0.0));

/// Optional knobs for [`adapt_temperature`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AdaptOptions {
    /// True if this temperature is being adapted for a validation-repair
    /// attempt. Repair attempts cycle: low → high → original.
    pub is_repair: bool,
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Return an adjusted temperature for a retry attempt.
///
/// * `base_temp` — the model's configured temperature
/// * `attempt`   — 1-indexed retry number (`0` means first call, no adjustment)
pub fn adapt_temperature(base_temp: f64, attempt: u32, options: AdaptOptions) -> f64 {
    if env::var("ITSY_TEMP_ADAPT").ok().as_deref() == Some("false") {
        return base_temp;
    }
    if base_temp.is_nan() {
        return base_temp;
    }
    if attempt == 0 {
        return base_temp;
    }

    let delta = if options.is_repair {
        // Repair attempts: low → high → original → low → high ...
        match attempt % 3 {
            1 => -*DELTA, // attempt 1: go deterministic
            2 => *DELTA,  // attempt 2: explore
            _ => 0.0,     // attempt 3: back to base
        }
    } else {
        // Non-repair: gentle linear nudge up to DELTA.
        ((attempt as f64) * 0.05).min(*DELTA)
    };

    let adjusted = round3(base_temp + delta);
    MAX_T.min(MIN_T.max(adjusted))
}

/// Apply the adapted temperature directly to a chat-completion request body.
/// Returns the (possibly mutated) body. No-op if no `temperature` field exists.
pub fn apply_adaptive_temperature(
    body: &mut Map<String, Value>,
    attempt: u32,
    options: AdaptOptions,
) {
    if env::var("ITSY_TEMP_ADAPT").ok().as_deref() == Some("false") {
        return;
    }
    let Some(t) = body.get("temperature").and_then(|v| v.as_f64()) else {
        return;
    };
    let adjusted = adapt_temperature(t, attempt, options);
    body.insert(
        "temperature".into(),
        serde_json::Number::from_f64(adjusted)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
}

// ---------------------------------------------------------------------------
// Backwards-compatible helper kept for existing callers.
// ---------------------------------------------------------------------------

/// Pick a sampling temperature given task type and recent failure pressure.
///
/// Existing callers (`cognition_adapter`) use this signature; preserved here
/// for ABI parity.
pub fn adaptive_temperature(task_type: &str, recent_failures: u32) -> f64 {
    let base = match task_type {
        "coding" | "editing" | "backend" => 0.1,
        "search" | "explanation" => 0.2,
        _ => 0.15,
    };
    (base + (recent_failures as f64) * 0.05).min(0.7)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Attempt 0 (first call) returns the base unchanged.
    #[test]
    fn attempt_zero_returns_base_unchanged() {
        assert_eq!(adapt_temperature(0.5, 0, AdaptOptions::default()), 0.5);
    }

    /// NaN input is preserved (don't try to compute on NaN).
    #[test]
    fn nan_input_returns_nan() {
        let r = adapt_temperature(f64::NAN, 1, AdaptOptions::default());
        assert!(r.is_nan(), "NaN input must propagate, got {r}");
    }

    /// Repair cycle: attempt 1 goes deterministic (lower), 2 explores (higher),
    /// 3 returns to base. Pins the 3-step cycle contract.
    #[test]
    fn repair_cycle_lowers_then_raises_then_resets() {
        let opts = AdaptOptions { is_repair: true };
        let base = 0.5;
        let a1 = adapt_temperature(base, 1, opts);
        let a2 = adapt_temperature(base, 2, opts);
        let a3 = adapt_temperature(base, 3, opts);
        assert!(a1 < base, "attempt 1 should go deterministic (lower); got {a1} vs base {base}");
        assert!(a2 > base, "attempt 2 should explore (higher); got {a2} vs base {base}");
        assert_eq!(a3, base, "attempt 3 should reset to base; got {a3}");
    }

    /// Non-repair attempts nudge up linearly, capped by DELTA.
    #[test]
    fn non_repair_attempts_nudge_up_linearly() {
        let base = 0.3;
        let a1 = adapt_temperature(base, 1, AdaptOptions::default());
        let a2 = adapt_temperature(base, 2, AdaptOptions::default());
        assert!(a1 > base);
        assert!(a2 >= a1, "later attempts must not lower temp; a1={a1} a2={a2}");
    }

    /// Result is clamped within [MIN_T, MAX_T].
    #[test]
    fn output_is_clamped() {
        // High base + repair attempt 2 should still cap at MAX_T (1.0 default).
        let r = adapt_temperature(0.99, 2, AdaptOptions { is_repair: true });
        assert!(r <= 1.0 + 1e-9, "must clamp to MAX_T; got {r}");
        // Low base + repair attempt 1 (going down) should not go below MIN_T (0.0 default).
        let r = adapt_temperature(0.01, 1, AdaptOptions { is_repair: true });
        assert!(r >= 0.0 - 1e-9, "must clamp to MIN_T; got {r}");
    }

    /// `apply_adaptive_temperature` mutates the body's temperature field
    /// when present, and leaves the body alone when absent.
    #[test]
    fn apply_mutates_body_temperature() {
        let mut body = json!({"temperature": 0.5, "model": "x"}).as_object().unwrap().clone();
        apply_adaptive_temperature(&mut body, 1, AdaptOptions { is_repair: true });
        let new_t = body.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!(new_t < 0.5, "repair attempt 1 must lower temperature; got {new_t}");
    }

    #[test]
    fn apply_noop_when_no_temperature_field() {
        let mut body = json!({"model": "x"}).as_object().unwrap().clone();
        apply_adaptive_temperature(&mut body, 1, AdaptOptions::default());
        assert!(body.get("temperature").is_none(),
            "must not invent temperature when not present");
    }

    /// Coding/editing task types start at 0.1 base.
    /// Anti-regression: code-generation must default to low temperature.
    #[test]
    fn adaptive_temperature_coding_starts_low() {
        assert_eq!(adaptive_temperature("coding", 0), 0.1);
        assert_eq!(adaptive_temperature("editing", 0), 0.1);
        assert_eq!(adaptive_temperature("backend", 0), 0.1);
    }

    /// Recent failures bump temperature up to a cap of 0.7.
    #[test]
    fn adaptive_temperature_caps_at_07() {
        let r = adaptive_temperature("coding", 100); // pile of failures
        assert!(r <= 0.7, "must cap at 0.7; got {r}");
    }

    /// Explanation tasks start at 0.2 (slightly higher creativity).
    #[test]
    fn adaptive_temperature_explanation_starts_at_02() {
        assert_eq!(adaptive_temperature("explanation", 0), 0.2);
    }
}
