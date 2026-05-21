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

    let len = steps.len();
    for i in 0..len {
        let step_name = steps[i].name.clone();
        crate::compiled::logger::info(
            "flow_step_started",
            &format!("{name}.{step_name}"),
            None,
        );
        let res = (steps[i].action)(ctx);
        match res {
            Ok(()) => {
                let comp = steps[i].compensation.take();
                completed.push((step_name, comp));
            }
            Err(e) => {
                let err_msg = e.to_string();
                crate::compiled::logger::error(
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
                                crate::compiled::logger::info(
                                    "flow_compensated",
                                    &format!("{name}.{cname}"),
                                    None,
                                );
                            }
                            Err(ce) => {
                                crate::compiled::logger::error(
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
