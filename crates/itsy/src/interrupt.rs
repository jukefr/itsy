//! Cooperative SIGINT handling. The agent loop checks
//! [`pending`]/[`take`] between tool calls and aborts the current turn
//! when an interrupt is observed, returning control to the REPL prompt
//! instead of letting the model spawn another tool.
//!
//! Press Ctrl+C **once** during a turn → the in-flight turn ends as soon
//! as the current tool finishes, with an "Interrupted" message.
//! Press Ctrl+C **once** at the empty REPL prompt → quit.
//!
//! Without this module a Ctrl+C only killed the foreground child process
//! (bash, cargo, …) — the agent loop kept happily firing the next tool.

use std::sync::atomic::{AtomicU32, Ordering};

static PRESSED: AtomicU32 = AtomicU32::new(0);

/// Spawn the SIGINT listener. Idempotent — calling twice is a no-op
/// because both tasks share the same atomic counter.
///
/// Must be called from inside a running tokio runtime.
pub fn install() {
    tokio::spawn(async {
        loop {
            // `ctrl_c()` resolves on every SIGINT (it re-arms internally).
            if tokio::signal::ctrl_c().await.is_err() {
                // Platform without SIGINT — bail.
                break;
            }
            PRESSED.fetch_add(1, Ordering::SeqCst);
        }
    });
}

/// How many Ctrl+C presses have been received since the last [`take`].
pub fn pending() -> u32 {
    PRESSED.load(Ordering::SeqCst)
}

/// Read-and-clear the pending count. Returns the number of presses since
/// the previous `take` / `reset`.
pub fn take() -> u32 {
    PRESSED.swap(0, Ordering::SeqCst)
}

/// Reset to zero without observing the prior count.
pub fn reset() {
    PRESSED.store(0, Ordering::SeqCst);
}
