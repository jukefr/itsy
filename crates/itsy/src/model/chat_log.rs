//! Raw per-call chat-completion logging.
//!
//! Captures the full request body + full response JSON for every chat
//! call. The response includes whatever the upstream model emits
//! verbatim — `reasoning_content`, `<think>...</think>` blocks inside
//! `content`, finish_reason, usage stats — so we can actually inspect
//! what the model was doing instead of guessing from the parsed summary.
//!
//! Output goes to the always-on session log via `session_log::log`. One
//! `[chat] {compact-json}` line per call. Same file as everything else
//! (tool events, errors, etc.), so a single grep over the session log
//! gives you the full picture.
//!
//! Disable with `[traces].disable = true`.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Persist one request/response pair as a single JSON line in the session
/// log. Best-effort — failure to serialise is logged once to stderr and
/// otherwise ignored.
pub fn record(request: &Value, response: &Value, attempt: u32) {
    if crate::settings::get().traces_disable {
        return;
    }
    let n = CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let body = json!({
        "n": n,
        "timestamp": ts,
        "attempt": attempt,
        "request": request,
        "response": response,
    });
    match serde_json::to_string(&body) {
        Ok(line) => crate::session_log::log(&format!("[chat] {line}")),
        Err(e) => {
            static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            if WARNED.set(()).is_ok() {
                eprintln!("[chat-log] failed to serialise call #{n}: {e}");
            }
        }
    }
}
