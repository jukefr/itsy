//! Saga runtime — implements flow declarations with backward compensation.
//!
//! On step failure, executes compensations for ALL completed steps in reverse
//! order. Two variants live here: a synchronous [`execute_flow`] used by the
//! cognition layer (registry-driven, no async runtime needed), and an
//! `async` [`execute_async_flow`] used by the agent loop for I/O-bound steps.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Flow registry — named lookup that mirrors the JS `executeFlow` registry.
// ---------------------------------------------------------------------------

pub type StepActionFn<C, E> = Box<dyn FnMut(&mut C) -> Result<(), E> + Send>;

pub struct Step<C, E> {
    pub name: String,
    pub action: StepActionFn<C, E>,
    pub compensation: Option<StepActionFn<C, E>>,
}

impl<C, E> Step<C, E> {
    pub fn new(
        name: impl Into<String>,
        action: impl FnMut(&mut C) -> Result<(), E> + Send + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            action: Box::new(action),
            compensation: None,
        }
    }

    pub fn with_compensation(
        mut self,
        compensation: impl FnMut(&mut C) -> Result<(), E> + Send + 'static,
    ) -> Self {
        self.compensation = Some(Box::new(compensation));
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowOutcome {
    pub ok: bool,
    pub failed_step: Option<String>,
    pub compensated: Vec<String>,
    pub error: Option<String>,
}

/// Execute a saga: run each step sequentially, compensating in reverse on
/// failure. Compensation errors are logged and skipped so a single broken
/// compensation does not block subsequent ones.
pub fn execute_flow<C, E: std::fmt::Display>(
    name: &str,
    mut steps: Vec<Step<C, E>>,
    ctx: &mut C,
) -> FlowOutcome {
    let mut completed: Vec<(String, Option<StepActionFn<C, E>>)> = Vec::new();

    for step in &mut steps {
        let step_name = step.name.clone();
        crate::runtime::logger::info(
            "flow_step_started",
            &format!("{name}.{step_name}"),
            None,
        );
        let res = (step.action)(ctx);
        match res {
            Ok(()) => {
                completed.push((step_name, step.compensation.take()));
            }
            Err(e) => {
                let err_msg = e.to_string();
                crate::runtime::logger::error(
                    "flow_step_failed",
                    &format!("{name}.{step_name}"),
                    Some(&err_msg),
                );
                let mut compensated: Vec<String> = Vec::new();
                while let Some((cname, cmp)) = completed.pop() {
                    if let Some(mut cfn) = cmp {
                        match cfn(ctx) {
                            Ok(()) => {
                                compensated.push(cname.clone());
                                crate::runtime::logger::info(
                                    "flow_compensated",
                                    &format!("{name}.{cname}"),
                                    None,
                                );
                            }
                            Err(ce) => {
                                crate::runtime::logger::error(
                                    "flow_compensation_failed",
                                    &format!("{name}.{cname}"),
                                    Some(&ce.to_string()),
                                );
                            }
                        }
                    }
                }
                return FlowOutcome {
                    ok: false,
                    failed_step: Some(step_name),
                    compensated,
                    error: Some(err_msg),
                };
            }
        }
    }
    FlowOutcome {
        ok: true,
        failed_step: None,
        compensated: Vec::new(),
        error: None,
    }
}

/// Trivial public registry kept for callers that imported the old type.
pub type FlowStep = serde_json::Value;

#[derive(Default)]
pub struct FlowRegistry {
    pub flows: HashMap<String, Vec<FlowStep>>,
}

impl FlowRegistry {
    pub fn new() -> Self {
        Self {
            flows: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, steps: Vec<FlowStep>) {
        self.flows.insert(name.into(), steps);
    }

    pub fn get(&self, name: &str) -> Option<&Vec<FlowStep>> {
        self.flows.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// All steps run in order on success path; FlowOutcome reports ok=true.
    #[test]
    fn flow_runs_steps_in_order_on_success() {
        let order: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let o1 = order.clone();
        let o2 = order.clone();
        let o3 = order.clone();
        let steps = vec![
            Step::<(), String>::new("a", move |_| { o1.lock().unwrap().push("a"); Ok(()) }),
            Step::<(), String>::new("b", move |_| { o2.lock().unwrap().push("b"); Ok(()) }),
            Step::<(), String>::new("c", move |_| { o3.lock().unwrap().push("c"); Ok(()) }),
        ];
        let mut ctx = ();
        let out = execute_flow("test", steps, &mut ctx);
        assert!(out.ok);
        assert!(out.failed_step.is_none());
        assert_eq!(*order.lock().unwrap(), vec!["a", "b", "c"]);
    }

    /// Failure compensates prior steps in REVERSE order.
    #[test]
    fn flow_compensates_in_reverse_on_failure() {
        let compensated: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let c1 = compensated.clone();
        let c2 = compensated.clone();
        let steps = vec![
            Step::<(), String>::new("a", |_| Ok(()))
                .with_compensation(move |_| { c1.lock().unwrap().push("a"); Ok(()) }),
            Step::<(), String>::new("b", |_| Ok(()))
                .with_compensation(move |_| { c2.lock().unwrap().push("b"); Ok(()) }),
            Step::<(), String>::new("c", |_| Err("boom".to_string())),
        ];
        let mut ctx = ();
        let out = execute_flow("test", steps, &mut ctx);
        assert!(!out.ok);
        assert_eq!(out.failed_step.as_deref(), Some("c"));
        // Compensations run in reverse: b before a.
        assert_eq!(*compensated.lock().unwrap(), vec!["b", "a"]);
        // Outcome lists them in compensation order too.
        assert_eq!(out.compensated, vec!["b", "a"]);
        assert_eq!(out.error.as_deref(), Some("boom"));
    }

    /// A broken compensation does NOT block subsequent ones.
    #[test]
    fn compensation_error_does_not_block_others() {
        let counter = Arc::new(AtomicU32::new(0));
        let c1 = counter.clone();
        let c2 = counter.clone();
        let steps = vec![
            Step::<(), String>::new("a", |_| Ok(()))
                .with_compensation(move |_| { c1.fetch_add(1, Ordering::SeqCst); Ok(()) }),
            Step::<(), String>::new("b", |_| Ok(()))
                .with_compensation(move |_| { c2.fetch_add(1, Ordering::SeqCst); Err("compensation failed".to_string()) }),
            Step::<(), String>::new("c", |_| Err("boom".to_string())),
        ];
        let mut ctx = ();
        let out = execute_flow("test", steps, &mut ctx);
        assert!(!out.ok);
        // Both compensation closures must have been invoked.
        assert_eq!(counter.load(Ordering::SeqCst), 2,
            "broken compensation must NOT short-circuit the chain");
        // Failed compensation is NOT included in `out.compensated`.
        assert_eq!(out.compensated, vec!["a"]);
    }

    /// Steps without compensation skip cleanly.
    #[test]
    fn missing_compensation_does_not_panic() {
        let steps = vec![
            Step::<(), String>::new("a", |_| Ok(())), // no compensation
            Step::<(), String>::new("b", |_| Err("boom".to_string())),
        ];
        let mut ctx = ();
        let out = execute_flow("test", steps, &mut ctx);
        assert!(!out.ok);
        assert!(out.compensated.is_empty());
    }

    /// Empty step list returns ok=true.
    #[test]
    fn empty_flow_returns_ok() {
        let mut ctx = ();
        let out = execute_flow("empty", Vec::<Step<(), String>>::new(), &mut ctx);
        assert!(out.ok);
        assert!(out.compensated.is_empty());
    }

    /// FlowRegistry stores and retrieves named flows.
    #[test]
    fn registry_register_and_get() {
        let mut r = FlowRegistry::new();
        let steps = vec![serde_json::json!({"name":"step1"})];
        r.register("my_flow", steps.clone());
        assert_eq!(r.get("my_flow"), Some(&steps));
        assert!(r.get("unknown").is_none());
    }
}

// ============================================================================
// Async saga — one I/O-bound step at a time, compensate on failure in reverse.
// ============================================================================

use std::future::Future;
use std::pin::Pin;

/// One step of an async flow: an `action` future plus an optional
/// `compensate` future. Both are dyn-boxed so a heterogeneous list can be
/// passed to [`execute_flow`].
pub type StepFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
pub type StepFn<C> = Box<dyn for<'a> FnMut(&'a mut C) -> StepFuture<'a> + Send>;

pub struct AsyncFlowStep<C> {
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
pub async fn execute_async_flow<C>(_name: &str, mut steps: Vec<AsyncFlowStep<C>>, ctx: &mut C) -> FlowResult {
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
mod async_tests {
    use super::*;
    use std::sync::Arc;

    /// `execute_flow` runs all steps in order and reports ok=true on success.
    #[tokio::test]
    async fn flow_runs_all_steps_in_order_on_success() {
        let order: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        fn step(label: &str, order: Arc<std::sync::Mutex<Vec<String>>>) -> AsyncFlowStep<()> {
            let label_owned = label.to_string();
            AsyncFlowStep {
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
        let r = execute_async_flow("test", steps, &mut ctx).await;
        assert!(r.ok);
        assert!(r.failed_step.is_none());
        assert_eq!(*order.lock().unwrap(), vec!["a", "b", "c"]);
    }

    /// When a step fails, prior steps are compensated in REVERSE order.
    #[tokio::test]
    async fn flow_compensates_in_reverse_on_failure() {
        let compensated: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        fn ok_step(name: &str, compensated: Arc<std::sync::Mutex<Vec<String>>>) -> AsyncFlowStep<()> {
            let n = name.to_string();
            AsyncFlowStep {
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

        fn bad_step(name: &str) -> AsyncFlowStep<()> {
            let n = name.to_string();
            AsyncFlowStep {
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
        let r = execute_async_flow("test", steps, &mut ctx).await;
        assert!(!r.ok);
        assert_eq!(r.failed_step.as_deref(), Some("step3"));
        assert_eq!(*compensated.lock().unwrap(), vec!["step2", "step1"]);
        assert!(r.error.unwrap().contains("step3-failed"));
    }

    /// Empty flow trivially succeeds.
    #[tokio::test]
    async fn empty_flow_returns_ok() {
        let mut ctx = ();
        let r: FlowResult = execute_async_flow("nothing", Vec::<AsyncFlowStep<()>>::new(), &mut ctx).await;
        assert!(r.ok);
    }
}
