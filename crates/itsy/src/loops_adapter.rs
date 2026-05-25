//! Bounded loop adapter — wraps the compiled loop runtime
//! ([`crate::runtime::cognition::loops`]) for use in the agent loop.
//! Mirrors `bin/loops_adapter.js`.

use std::future::Future;
use std::pin::Pin;

use crate::runtime::cognition::loops::{run_with_retry, RetryConfig};

#[derive(Debug, Clone)]
pub struct ValidationOutcome {
    pub passed: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BoundedValidationResult {
    pub passed: bool,
    pub attempts: u32,
    pub last_errors: Vec<String>,
    pub exhausted: bool,
}

/// Run a bounded validation loop. The validator runs at most
/// `max_iterations` times; we return the final outcome plus a flag
/// indicating whether the retry budget was exhausted. Mirrors
/// `runBoundedValidation` in `bin/loops_adapter.js`.
pub async fn run_bounded_validation<F, Fut>(
    mut validate_fn: F,
    file_path: &str,
    max_iterations: u32,
) -> BoundedValidationResult
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Option<ValidationOutcome>>,
{
    let cfg = RetryConfig {
        max_attempts: max_iterations.max(1),
        backoff_ms: 0,
        jitter_ms: 1,
    };
    let mut attempts: u32 = 0;
    let mut last_errors: Vec<String> = Vec::new();
    let res: Result<(), Vec<String>> = run_with_retry(cfg, |_| {
        let path = file_path.to_string();
        attempts += 1;
        let fut = validate_fn(path);
        async move {
            match fut.await {
                None => Ok(()),
                Some(o) if o.passed => Ok(()),
                Some(o) => Err(o.errors),
            }
        }
    })
    .await;
    if let Err(ref errs) = res {
        last_errors = errs.clone();
    }
    let exhausted = attempts >= max_iterations && res.is_err();
    BoundedValidationResult {
        passed: res.is_ok(),
        attempts,
        last_errors,
        exhausted,
    }
}

/// One step of a flow: an `action` future plus an optional `compensate`
/// future. Both are dyn-boxed so a heterogeneous list can be passed to
/// [`execute_flow`].
pub type StepFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
pub type StepFn<C> = Box<dyn for<'a> FnMut(&'a mut C) -> StepFuture<'a> + Send>;

pub struct FlowStep<C> {
    pub name: String,
    pub action: StepFn<C>,
    pub compensate: Option<StepFn<C>>,
}

#[derive(Debug, Clone)]
pub struct FlowResult {
    pub ok: bool,
    pub failed_step: Option<String>,
    pub compensated: Vec<String>,
    pub error: Option<String>,
}

/// Execute a saga-style flow. Each step runs sequentially; if any step
/// returns an error, prior steps are compensated in reverse order.
pub async fn execute_flow<C>(_name: &str, mut steps: Vec<FlowStep<C>>, ctx: &mut C) -> FlowResult {
    let mut completed: Vec<(String, Option<StepFn<C>>)> = Vec::new();
    for step in &mut steps {
        let name = step.name.clone();
        let action_res = (step.action)(ctx).await;
        match action_res {
            Ok(()) => {
                completed.push((name, step.compensate.take()));
            }
            Err(e) => {
                let mut compensated_names = Vec::new();
                while let Some((cname, cmp)) = completed.pop() {
                    if let Some(mut cfn) = cmp {
                        if (cfn)(ctx).await.is_ok() {
                            compensated_names.push(cname);
                        }
                    }
                }
                return FlowResult {
                    ok: false,
                    failed_step: Some(name),
                    compensated: compensated_names,
                    error: Some(e.to_string()),
                };
            }
        }
    }
    FlowResult {
        ok: true,
        failed_step: None,
        compensated: Vec::new(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// `run_bounded_validation` returns passed=true on first-try success
    /// without spinning more attempts.
    #[tokio::test]
    async fn bounded_validation_stops_on_pass() {
        let attempts = Arc::new(AtomicU32::new(0));
        let a2 = attempts.clone();
        let r = run_bounded_validation(
            move |_path| {
                let c = a2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Some(ValidationOutcome { passed: true, errors: vec![] })
                }
            },
            "x.rs",
            5,
        ).await;
        assert!(r.passed);
        assert_eq!(r.attempts, 1, "must stop on first pass; spent {} attempts", r.attempts);
        assert!(!r.exhausted);
    }

    /// `run_bounded_validation` keeps retrying until the budget runs out.
    #[tokio::test]
    async fn bounded_validation_exhausts_when_never_passes() {
        let attempts = Arc::new(AtomicU32::new(0));
        let a2 = attempts.clone();
        let r = run_bounded_validation(
            move |_p| {
                let c = a2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Some(ValidationOutcome { passed: false, errors: vec!["err".into()] })
                }
            },
            "x.rs",
            3,
        ).await;
        assert!(!r.passed);
        assert_eq!(r.attempts, 3, "must use full budget");
        assert!(r.exhausted);
        assert_eq!(r.last_errors, vec!["err".to_string()],
            "last_errors must reflect the final failure");
    }

    /// `run_bounded_validation` with `None` outcome treats it as success
    /// (validator reported no signal to test).
    #[tokio::test]
    async fn bounded_validation_none_outcome_is_pass() {
        let r = run_bounded_validation(
            |_| async { None },
            "x.rs", 3,
        ).await;
        assert!(r.passed, "None outcome must be treated as pass (no signal)");
        assert_eq!(r.attempts, 1);
    }

    /// `execute_flow` runs all steps in order and reports ok=true on success.
    #[tokio::test]
    async fn flow_runs_all_steps_in_order_on_success() {
        let order: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        fn step(label: &str, order: Arc<std::sync::Mutex<Vec<String>>>) -> FlowStep<()> {
            let label_owned = label.to_string();
            FlowStep {
                name: label.to_string(),
                action: Box::new(move |_ctx| {
                    let l = label_owned.clone();
                    let o = order.clone();
                    Box::pin(async move {
                        o.lock().unwrap().push(l);
                        Ok::<(), anyhow::Error>(())
                    })
                }),
                compensate: None,
            }
        }

        let steps = vec![
            step("a", order.clone()),
            step("b", order.clone()),
            step("c", order.clone()),
        ];
        let mut ctx = ();
        let r = execute_flow("test", steps, &mut ctx).await;
        assert!(r.ok);
        assert!(r.failed_step.is_none());
        assert_eq!(*order.lock().unwrap(), vec!["a", "b", "c"]);
    }

    /// When a step fails, prior steps are compensated in REVERSE order.
    #[tokio::test]
    async fn flow_compensates_in_reverse_on_failure() {
        let compensated: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        fn ok_step(name: &str, compensated: Arc<std::sync::Mutex<Vec<String>>>) -> FlowStep<()> {
            let n = name.to_string();
            FlowStep {
                name: name.to_string(),
                action: Box::new(|_ctx| Box::pin(async { Ok(()) })),
                compensate: Some(Box::new(move |_ctx| {
                    let n = n.clone();
                    let c = compensated.clone();
                    Box::pin(async move {
                        c.lock().unwrap().push(n);
                        Ok::<(), anyhow::Error>(())
                    })
                })),
            }
        }

        fn bad_step(name: &str) -> FlowStep<()> {
            let n = name.to_string();
            FlowStep {
                name: n.clone(),
                action: Box::new(move |_ctx| {
                    let err = format!("{n}-failed");
                    Box::pin(async move { Err(anyhow::anyhow!(err)) })
                }),
                compensate: None,
            }
        }

        let steps = vec![
            ok_step("step1", compensated.clone()),
            ok_step("step2", compensated.clone()),
            bad_step("step3"),
        ];
        let mut ctx = ();
        let r = execute_flow("test", steps, &mut ctx).await;
        assert!(!r.ok);
        assert_eq!(r.failed_step.as_deref(), Some("step3"));
        // compensated in reverse: step2 first, then step1.
        assert_eq!(*compensated.lock().unwrap(), vec!["step2", "step1"]);
        assert!(r.error.unwrap().contains("step3-failed"));
    }

    /// Empty flow trivially succeeds.
    #[tokio::test]
    async fn empty_flow_returns_ok() {
        let mut ctx = ();
        let r: FlowResult = execute_flow("nothing", Vec::<FlowStep<()>>::new(), &mut ctx).await;
        assert!(r.ok);
    }
}
