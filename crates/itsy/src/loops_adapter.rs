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
