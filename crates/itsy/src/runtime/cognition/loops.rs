//! Bounded agent loops + retry helper.
//!
//! Two complementary primitives live here:
//!
//! * [`run_with_retry`] — retry an async closure with linear-with-jitter
//!   backoff. Used by the agent loop for transient provider errors.
//! * [`run_loop`] — bounded generate → validate → retry refinement loop.
//!   Mirrors the JS `runLoop({name, max_iterations, trace_id, step, validate})`
//!   pattern from Phase 26. Always terminates; never recurses; emits a
//!   summary span to the trace store on completion.

use std::future::Future;
use std::time::{Duration, Instant};

use crate::runtime::cognition::traces::{write_span, SpanInit};

// ─── Retry helper (kept for existing callers) ───────────────────────────────

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub jitter_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_attempts: 3, backoff_ms: 250, jitter_ms: 50 }
    }
}

pub async fn run_with_retry<F, T, E, Fut>(cfg: RetryConfig, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut last: Option<E> = None;
    for attempt in 0..cfg.max_attempts {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                if attempt + 1 < cfg.max_attempts {
                    let jitter = (rand::random::<u64>() % cfg.jitter_ms.max(1)) as u64;
                    tokio::time::sleep(Duration::from_millis(cfg.backoff_ms + jitter)).await;
                }
            }
        }
    }
    Err(last.expect("no attempts ran"))
}

// ─── Bounded refinement loop ────────────────────────────────────────────────

/// Configuration for a single [`run_loop`] invocation.
pub struct LoopConfig<'a, T, S, V, SFut, VFut>
where
    S: FnMut(u32) -> SFut,
    V: FnMut(&T) -> VFut,
    SFut: Future<Output = Result<T, String>>,
    VFut: Future<Output = bool>,
{
    pub name: &'a str,
    pub max_iterations: u32,
    pub trace_id: &'a str,
    pub step: S,
    pub validate: V,
    // PhantomData not needed; the compiler infers `T` from `step`'s return.
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T, S, V, SFut, VFut> LoopConfig<'a, T, S, V, SFut, VFut>
where
    S: FnMut(u32) -> SFut,
    V: FnMut(&T) -> VFut,
    SFut: Future<Output = Result<T, String>>,
    VFut: Future<Output = bool>,
{
    pub fn new(name: &'a str, max_iterations: u32, trace_id: &'a str, step: S, validate: V) -> Self {
        Self {
            name,
            max_iterations,
            trace_id,
            step,
            validate,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Result of running a bounded refinement loop.
#[derive(Debug)]
pub struct LoopOutcome<T> {
    /// The final output (best-effort: last produced value, even if invalid).
    pub final_value: Option<T>,
    /// `true` when we ran out of iterations without ever validating.
    pub exhausted: bool,
    /// Number of iterations actually executed.
    pub attempts: u32,
}

/// Bounded `step → validate → retry` loop.
///
/// Equivalent to the JS `runLoop({name, max_iterations, trace_id, step,
/// validate})`. Returns as soon as `validate` returns `true`, otherwise
/// up to `max_iterations` attempts before declaring `exhausted`.
pub async fn run_loop<T, S, V, SFut, VFut>(
    mut cfg: LoopConfig<'_, T, S, V, SFut, VFut>,
) -> LoopOutcome<T>
where
    S: FnMut(u32) -> SFut,
    V: FnMut(&T) -> VFut,
    SFut: Future<Output = Result<T, String>>,
    VFut: Future<Output = bool>,
{
    let started = Instant::now();
    let mut last: Option<T> = None;
    let mut attempts: u32 = 0;
    let mut valid = false;

    for i in 0..cfg.max_iterations {
        attempts = i + 1;
        let out = match (cfg.step)(i).await {
            Ok(v) => v,
            Err(_e) => continue,
        };
        valid = (cfg.validate)(&out).await;
        last = Some(out);
        if valid {
            break;
        }
    }

    let total_ms = started.elapsed().as_millis() as u64;
    let mut metadata = serde_json::Map::new();
    metadata.insert("iterations".into(), serde_json::json!(attempts));
    metadata.insert("max_iterations".into(), serde_json::json!(cfg.max_iterations));
    metadata.insert("valid".into(), serde_json::json!(valid));
    let _ = write_span(SpanInit {
        trace_id: cfg.trace_id.to_string(),
        workflow: "loop".into(),
        step: cfg.name.to_string(),
        kind: "loop".into(),
        latency_ms: total_ms,
        status: if valid { "ok".into() } else { "exhausted".into() },
        metadata,
        ..Default::default()
    });

    LoopOutcome {
        final_value: last,
        exhausted: !valid,
        attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// `run_loop` stops as soon as `validate` returns true — doesn't waste
    /// further iterations. This is the early-exit invariant.
    #[tokio::test]
    async fn run_loop_stops_on_first_valid() {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        let cfg = LoopConfig::new(
            "test", 5, "t",
            move |i| {
                let cc = c2.clone();
                async move {
                    cc.fetch_add(1, Ordering::SeqCst);
                    Ok::<u32, String>(i)
                }
            },
            |v: &u32| { let v = *v; async move { v >= 2 } },
        );
        let out = run_loop(cfg).await;
        assert!(!out.exhausted, "must not exhaust when valid is reached");
        assert_eq!(out.attempts, 3, "must stop on the iteration where validate becomes true (i=2)");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    /// `run_loop` exhausts and returns the last value when validation
    /// never succeeds within `max_iterations`.
    #[tokio::test]
    async fn run_loop_exhausts_when_never_valid() {
        let cfg = LoopConfig::new(
            "test", 3, "t",
            |i| async move { Ok::<u32, String>(i) },
            |_| async { false },
        );
        let out = run_loop(cfg).await;
        assert!(out.exhausted);
        assert_eq!(out.attempts, 3);
        assert_eq!(out.final_value, Some(2));
    }

    /// Step errors don't abort the loop — they just skip the iteration.
    /// Anti-regression: a single transient error must not poison the loop.
    #[tokio::test]
    async fn run_loop_skips_step_errors() {
        let cfg = LoopConfig::new(
            "test", 5, "t",
            |i| async move {
                if i == 0 || i == 2 {
                    Err::<u32, String>("transient".to_string())
                } else {
                    Ok(i)
                }
            },
            |v: &u32| { let v = *v; async move { v == 1 } },
        );
        let out = run_loop(cfg).await;
        // i=0 errors (skip), i=1 succeeds and validates → loop exits
        assert!(!out.exhausted);
        assert_eq!(out.attempts, 2);
        assert_eq!(out.final_value, Some(1));
    }

    /// `run_with_retry` returns Ok on the first successful attempt.
    #[tokio::test]
    async fn run_with_retry_succeeds_first_try() {
        let cfg = RetryConfig { max_attempts: 3, backoff_ms: 1, jitter_ms: 1 };
        let result = run_with_retry(cfg, |_| async { Ok::<i32, String>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    /// `run_with_retry` retries on Err and surfaces the success.
    #[tokio::test]
    async fn run_with_retry_recovers_after_transient_failure() {
        let cfg = RetryConfig { max_attempts: 3, backoff_ms: 1, jitter_ms: 1 };
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        let result = run_with_retry(cfg, move |_| {
            let cc = c2.clone();
            async move {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                if n < 2 { Err::<i32, String>("transient".into()) } else { Ok(99) }
            }
        }).await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    /// `run_with_retry` gives up after `max_attempts` and surfaces the last error.
    #[tokio::test]
    async fn run_with_retry_returns_last_error_on_exhaustion() {
        let cfg = RetryConfig { max_attempts: 2, backoff_ms: 1, jitter_ms: 1 };
        let result: Result<i32, String> = run_with_retry(cfg, |attempt| async move {
            Err(format!("err-{attempt}"))
        }).await;
        assert_eq!(result.unwrap_err(), "err-1",
            "must surface the LAST attempt's error, not the first");
    }
}
