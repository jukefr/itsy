//! Raw per-call chat-completion logging.
//!
//! Captures the full request body + full response JSON for every chat
//! call. The response includes whatever the upstream model emits
//! verbatim — `reasoning_content`, `<think>...</think>` blocks inside
//! `content`, finish_reason, usage stats — so we can actually inspect
//! what the model was doing instead of guessing from the parsed
//! summary.
//!
//! Files land at one of two roots, in priority order:
//!
//!   1. `/logs/agent/chats/` if `/logs/agent` exists (bench / harbor
//!      runs — that path gets captured as a trial artifact)
//!   2. `<traces_dir>/chats/` otherwise (local runs)
//!
//! Disable entirely with `[traces].disable = true` or
//! `--set traces.disable=true`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn chats_root() -> Option<PathBuf> {
    let bench = PathBuf::from("/logs/agent");
    if bench.is_dir() {
        let p = bench.join("chats");
        std::fs::create_dir_all(&p).ok()?;
        return Some(p);
    }
    let cwd = std::env::current_dir().ok()?;
    let p = crate::paths::traces_dir(&cwd).join("chats");
    std::fs::create_dir_all(&p).ok()?;
    Some(p)
}

/// Persist one request/response pair. Best-effort — failure to write
/// is logged once to stderr and otherwise ignored so logging never
/// breaks the agent loop.
pub fn record(request: &Value, response: &Value, attempt: u32) {
    if crate::settings::get().traces_disable {
        return;
    }
    let Some(root) = chats_root() else { return };
    let n = CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let path = root.join(format!("{n:06}-{ts}.json"));
    let body = json!({
        "n": n,
        "timestamp": ts,
        "attempt": attempt,
        "request": request,
        "response": response,
    });
    let serialised = match serde_json::to_vec_pretty(&body) {
        Ok(b) => b,
        Err(_) => return,
    };
    if let Err(e) = std::fs::write(&path, &serialised) {
        // Print once per failure — don't spam the agent log.
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        let _ = WARNED.set(());
        eprintln!(
            "  \x1b[90m[chat-log] failed to write {}: {e}\x1b[0m",
            path.display()
        );
    }
}
