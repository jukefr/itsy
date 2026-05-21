//! `edit_with_approval` flow checkpoint store — in-memory implementation for
//! CLI use (no DB).
//!
//! Two surfaces are exposed:
//!
//! 1. The simple `CheckpointStore` retained for existing call sites: a
//!    string-keyed map of recorded checkpoint payloads.
//!
//! 2. The pending-decision registry that mirrors the JS `awaitDecision` /
//!    `submitDecision` / `buildShowsPayload` triad. The async checkpoint flow
//!    suspends until either a decision arrives (via `submit_decision`) or the
//!    optional timeout elapses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Legacy CheckpointStore (still used by other crates)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct CheckpointStore {
    pub points: Mutex<HashMap<String, Value>>,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, name: &str, value: Value) {
        self.points.lock().insert(name.to_string(), value);
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.points.lock().get(name).cloned()
    }
}

// ---------------------------------------------------------------------------
// Approval flow
// ---------------------------------------------------------------------------

/// What the user (or system) decided when prompted at a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Reject,
    Cancel,
}

impl Decision {
    pub fn parse(s: &str) -> Decision {
        match s {
            "approve" => Decision::Approve,
            "cancel" => Decision::Cancel,
            _ => Decision::Reject,
        }
    }
}

/// What [`await_decision`] resolves to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResult {
    pub decision: Decision,
    pub edited_payload: Option<Value>,
    pub actor_id: String,
    pub timed_out: bool,
}

/// Behaviour on timeout.
#[derive(Debug, Clone, Copy)]
pub enum OnTimeout {
    Reject,
    Cancel,
}

fn pending_key(flow_run_id: &str, checkpoint: &str) -> String {
    format!("{flow_run_id}|{checkpoint}")
}

/// Approval-handler shape: the TUI registers a closure that runs y/n prompts
/// and returns a `Decision`.
pub type ApprovalHandler = Arc<dyn Fn(&str, &str) -> Decision + Send + Sync>;

#[derive(Default)]
struct ApprovalState {
    pending: HashMap<String, oneshot::Sender<DecisionResult>>,
    handler: Option<ApprovalHandler>,
}

static STATE: once_cell::sync::Lazy<Mutex<ApprovalState>> =
    once_cell::sync::Lazy::new(|| Mutex::new(ApprovalState::default()));

/// Register an approval handler. Replaces any previously registered handler.
pub fn set_approval_handler<F>(handler: F)
where
    F: Fn(&str, &str) -> Decision + Send + Sync + 'static,
{
    STATE.lock().handler = Some(Arc::new(handler));
}

/// Clear the registered approval handler.
pub fn clear_approval_handler() {
    STATE.lock().handler = None;
}

/// Suspend until either a decision arrives or the timeout fires.
///
/// If an approval handler is registered, it is invoked synchronously to
/// produce a decision, which is then routed back through [`submit_decision`]
/// like any other.
pub async fn await_decision(
    flow_run_id: &str,
    checkpoint_name: &str,
    timeout_ms: Option<u64>,
    on_timeout: OnTimeout,
) -> DecisionResult {
    let (tx, rx) = oneshot::channel::<DecisionResult>();
    let key = pending_key(flow_run_id, checkpoint_name);

    let handler_clone = {
        let mut s = STATE.lock();
        s.pending.insert(key.clone(), tx);
        s.handler.clone()
    };

    // If an approval handler is registered, run it now and submit the result.
    if let Some(handler) = handler_clone {
        let flow_run_id = flow_run_id.to_string();
        let checkpoint_name = checkpoint_name.to_string();
        std::thread::spawn(move || {
            let decision = (handler)(&flow_run_id, &checkpoint_name);
            let _ = submit_decision(
                &flow_run_id,
                &checkpoint_name,
                decision,
                None,
                "user",
            );
        });
    }

    if let Some(ms) = timeout_ms.filter(|m| *m > 0) {
        match timeout(Duration::from_millis(ms), rx).await {
            Ok(Ok(d)) => d,
            _ => {
                // Drop the still-pending entry so a late submit_decision is
                // a clean no-op.
                STATE.lock().pending.remove(&key);
                DecisionResult {
                    decision: match on_timeout {
                        OnTimeout::Cancel => Decision::Cancel,
                        OnTimeout::Reject => Decision::Reject,
                    },
                    edited_payload: None,
                    actor_id: "system:timeout".into(),
                    timed_out: true,
                }
            }
        }
    } else {
        rx.await.unwrap_or(DecisionResult {
            decision: Decision::Reject,
            edited_payload: None,
            actor_id: "system:dropped".into(),
            timed_out: false,
        })
    }
}

/// Outcome of [`submit_decision`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResult {
    pub ok: bool,
    pub reason: Option<String>,
}

/// Submit a decision for a pending checkpoint. No-ops if the checkpoint isn't
/// pending (e.g. has already been resolved or timed out).
pub fn submit_decision(
    flow_run_id: &str,
    checkpoint_name: &str,
    decision: Decision,
    edited_payload: Option<Value>,
    actor_id: &str,
) -> SubmitResult {
    let key = pending_key(flow_run_id, checkpoint_name);
    let sender = STATE.lock().pending.remove(&key);
    let Some(tx) = sender else {
        return SubmitResult {
            ok: false,
            reason: Some("no pending checkpoint".into()),
        };
    };
    let _ = tx.send(DecisionResult {
        decision,
        edited_payload,
        actor_id: actor_id.into(),
        timed_out: false,
    });
    SubmitResult { ok: true, reason: None }
}

/// Resolve a dotted set of paths against `ctx`, returning a map of
/// `{path → value}`. Missing paths resolve to `null`.
pub fn build_shows_payload(ctx: &Value, paths: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for path in paths {
        let mut cur: Option<&Value> = Some(ctx);
        for seg in path.split('.') {
            cur = match cur {
                Some(Value::Object(map)) => map.get(seg),
                _ => None,
            };
            if cur.is_none() {
                break;
            }
        }
        out.insert(path.clone(), cur.cloned().unwrap_or(Value::Null));
    }
    Value::Object(out)
}
