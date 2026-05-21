//! Cooperative SIGINT handling.
//!
//! The agent loop checks [`pending`] / [`take`] between tool calls and
//! aborts the current turn when an interrupt is observed, returning
//! control to the REPL prompt instead of letting the model spawn another
//! tool.
//!
//! * Ctrl+C during a turn → the in-flight turn ends after the current
//!   tool finishes with an "Interrupted" message.
//! * Ctrl+C at the REPL prompt with no input pending → quit.
//! * Ctrl+C during the [`crate::init_wizard`] → exit the process
//!   immediately (the wizard's synchronous stdin reads block in the
//!   kernel, so cooperative cancellation can't reach them).
//!
//! Without this module a Ctrl+C only killed the foreground child process
//! (bash, cargo, …) — the agent loop kept happily firing the next tool.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static PRESSED: AtomicU32 = AtomicU32::new(0);
static WIZARD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Spawn the SIGINT listener. Idempotent — calling twice is a no-op
/// because both tasks share the same atomic counter.
///
/// Must be called from inside a running tokio runtime.
pub fn install() {
    tokio::spawn(async {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                // Platform without SIGINT — bail out of the loop.
                break;
            }
            if WIZARD_ACTIVE.load(Ordering::SeqCst) {
                // Synchronous stdin reads inside the wizard never observe
                // a cooperative counter — just exit cleanly.
                eprintln!("\n  setup cancelled");
                std::process::exit(130);
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

/// RAII guard that flips wizard-mode on creation and restores on drop.
/// While held, SIGINT exits the process via the listener installed by
/// [`install`].
///
/// ```ignore
/// fn run_wizard() {
///     let _g = WizardGuard::enter();
///     // ... synchronous read_line() calls; Ctrl+C exits process ...
/// }
/// ```
pub struct WizardGuard {
    prev: bool,
}

impl WizardGuard {
    pub fn enter() -> Self {
        let prev = WIZARD_ACTIVE.swap(true, Ordering::SeqCst);
        WizardGuard { prev }
    }
}

impl Drop for WizardGuard {
    fn drop(&mut self) {
        WIZARD_ACTIVE.store(self.prev, Ordering::SeqCst);
    }
}
