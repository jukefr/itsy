//! Adaptive Retry Temperature.
//!
//! When the improvement loop retries a failed edit, the default temperature
//! (usually 0.3–0.7) is used for every attempt. This causes the model to
//! produce nearly identical outputs: same strategy, same mistake.
//!
//! We nudge temperature differently per attempt:
//!   - attempt 1 (initial fail)  → lower: become more deterministic
//!   - attempt 2 (still failing) → higher: explore a different approach
//!   - attempt 3+                → back to original: the deterministic retry
//!                                  might finally work
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
