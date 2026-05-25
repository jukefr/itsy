//! Saga runtime — implements flow declarations with backward compensation.
//!
//! On step failure, executes compensations for ALL completed steps in reverse
//! order. The synchronous (non-`async`) variant lives here so the cognition
//! layer can register flows without dragging an executor into scope; the async
//! sibling lives in `loops_adapter::execute_flow`.

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
