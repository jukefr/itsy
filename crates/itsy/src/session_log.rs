//! Always-on session log writer. Opens one file at startup; every mode
//! (bench, classic, fullscreen TUI) appends to it. Gives us a grep-able
//! record of every model call, tool dispatch, chat event, and error
//! regardless of how the binary was invoked.
//!
//! Path priority:
//!   1. `/logs/agent/itsy.log` if `/logs/agent` exists (bench / harbor)
//!   2. `<traces_dir>/itsy-<UTC-timestamp>.log` otherwise (local runs)
//!
//! Disable with `[traces].disable = true`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn pick_path() -> Option<PathBuf> {
    let bench = PathBuf::from("/logs/agent");
    if bench.is_dir() {
        return Some(bench.join("itsy.log"));
    }
    let cwd = std::env::current_dir().ok()?;
    let dir = crate::paths::traces_dir(&cwd);
    std::fs::create_dir_all(&dir).ok()?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    Some(dir.join(format!("itsy-{ts}.log")))
}

/// Initialise the session log. Idempotent — first caller wins; later
/// calls are no-ops. Best-effort: silent failure if the path can't be
/// opened.
pub fn init() {
    LOG_FILE.get_or_init(|| {
        if crate::settings::get().traces_disable {
            return None;
        }
        let path = pick_path()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new)
    });
}

/// Append one timestamped line to the session log. Multi-line messages
/// get one timestamp per line. Failures are swallowed — logging must
/// never break the agent loop.
pub fn log(msg: &str) {
    let Some(Some(file)) = LOG_FILE.get() else { return };
    let Ok(mut guard) = file.lock() else { return };
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
    for line in msg.split_inclusive('\n') {
        let _ = write!(guard, "[{now}] {line}");
        if !line.ends_with('\n') {
            let _ = writeln!(guard);
        }
    }
    let _ = guard.flush();
}
