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

    // Diagnostic SIGTSTP listener (Unix only). Doesn't suppress the
    // default suspend behaviour — Ctrl+Z still works. But before the
    // suspend lands, we log a stderr breadcrumb so the user can confirm
    // whether suspension came from their keyboard / kernel / a misbehaved
    // child process. Toggle with `ITSY_DEBUG_SIGTSTP=true`.
    #[cfg(unix)]
    if std::env::var("ITSY_DEBUG_SIGTSTP").as_deref() == Ok("true") {
        tokio::spawn(async {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut stream) = signal(SignalKind::from_raw(libc_sigtstp())) else {
                return;
            };
            while stream.recv().await.is_some() {
                eprintln!("\n  [itsy] received SIGTSTP — process will suspend. Resume with `fg`.");
            }
        });
    }
}

#[cfg(unix)]
fn libc_sigtstp() -> i32 {
    // Standard POSIX signal number for SIGTSTP (terminal stop) is 20 on
    // Linux/Darwin and 18 on some BSDs — but we use libc's constant via
    // the `nix` crate which is already in the dep set.
    nix::sys::signal::Signal::SIGTSTP as i32
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
