//! Canonical filesystem layout for itsy state.
//!
//! All runtime state lives under the user's config directory; nothing is
//! written into the project working tree. The layout is:
//!
//! ```text
//! ~/.config/itsy/
//!   config.toml              global config (version + migrations)
//!   plugins/                 user-installed plugins
//!   skills/                  user-installed skills
//!   projects/                per-project state, keyed by a hash of cwd
//!     <slug>-<hash>/
//!       memory.db            SQLite + FTS5 project memory
//!       codegraph.db         SQLite + FTS5 symbol index
//!       sessions/            conversation persistence
//!       traces/              agent execution traces
//!       snapshots/           auto-rollback checkpoints
//!       tool_scores.json     governor's per-tool confidence
//! ```
//!
//! `ITSY_HOME` overrides the root entirely; `XDG_CONFIG_HOME` is honored
//! when set; otherwise `$HOME/.config/itsy/` is used.

use std::path::{Path, PathBuf};


/// Shared lock for tests that need to mutate `ITSY_HOME` or `XDG_CONFIG_HOME`.
/// Multiple test modules mutate these globals; without a single shared lock
/// they race and corrupt each other's tempdirs. Use as:
///
/// ```ignore
/// let _g = crate::paths::env_lock();
/// ```
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    }
}

/// Root of all itsy state.
pub fn config_dir() -> PathBuf {
    if let Ok(over) = std::env::var("ITSY_HOME") {
        if !over.is_empty() {
            return PathBuf::from(over);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("itsy");
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("itsy")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn plugins_dir() -> PathBuf {
    config_dir().join("plugins")
}

pub fn skills_dir() -> PathBuf {
    config_dir().join("skills")
}

/// Project-keyed state root for `cwd`.
pub fn project_dir(cwd: &Path) -> PathBuf {
    config_dir().join("projects").join(project_id(cwd))
}

pub fn memory_db(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("memory.db")
}

pub fn codegraph_db(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("codegraph.db")
}

pub fn sessions_dir(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("sessions")
}

pub fn traces_dir(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("traces")
}

pub fn snapshots_dir(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("snapshots")
}

pub fn tool_scores(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("tool_scores.json")
}

/// Derive a stable, human-readable project id from a working directory.
///
/// The id is the canonical absolute path with `/` → `-`, leading dash
/// preserved (mirrors Claude Code's project-slug convention). Examples:
///
/// * `/home/user/Documents/src/openclaw/life`
///   → `-home-user-Documents-src-openclaw-life`
/// * `/workspace/itsy`
///   → `-workspace-itsy`
///
/// Characters that aren't ASCII alphanumeric, `-`, `_`, or `.` are
/// also flattened to `-` so paths with spaces or unicode round-trip
/// safely. Multiple consecutive `-` are collapsed.
pub fn project_id(cwd: &Path) -> String {
    let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = canon.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        let mapped = if c == '/' || c == '\\' {
            '-'
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            c
        } else {
            '-'
        };
        if mapped == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(mapped);
    }
    if out.is_empty() {
        return "project".into();
    }
    // Strip a single trailing `-` (e.g. when cwd was "/") but keep a
    // leading `-` so absolute paths stay distinct from relative ones.
    if out.ends_with('-') && out.len() > 1 {
        out.pop();
    }
    out
}

/// Eagerly create every directory the project will need. Safe to call on
/// every launch — `create_dir_all` is idempotent.
pub fn ensure_project_dirs(cwd: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(project_dir(cwd))?;
    std::fs::create_dir_all(sessions_dir(cwd))?;
    std::fs::create_dir_all(traces_dir(cwd))?;
    std::fs::create_dir_all(snapshots_dir(cwd))?;
    Ok(())
}

/// Eagerly create the global config dirs. Same idempotence guarantee.
pub fn ensure_config_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(plugins_dir())?;
    std::fs::create_dir_all(skills_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_is_stable_for_same_cwd() {
        let p = std::env::temp_dir();
        let a = project_id(&p);
        let b = project_id(&p);
        assert_eq!(a, b);
    }

    #[test]
    fn project_id_changes_with_cwd() {
        let p1 = std::env::temp_dir();
        let p2 = std::env::temp_dir().join("subdir-that-may-not-exist");
        // Doesn't need to exist — canonicalize falls back to the literal path.
        assert_ne!(project_id(&p1), project_id(&p2));
    }

    #[test]
    fn project_id_is_path_with_slashes_as_dashes() {
        let p = PathBuf::from("/home/user/Documents/src/openclaw/life");
        // canonicalize on a non-existent path falls back to the literal,
        // so we get a deterministic string here.
        assert_eq!(project_id(&p), "-home-user-Documents-src-openclaw-life");
    }

    #[test]
    fn project_id_collapses_consecutive_dashes_and_handles_spaces() {
        let p = PathBuf::from("/path with spaces/and//doubles");
        assert_eq!(project_id(&p), "-path-with-spaces-and-doubles");
    }

    #[test]
    fn itsy_home_override_takes_priority() {
        let original = std::env::var("ITSY_HOME").ok();
        // SAFETY: tests in this module are not run in parallel with the rest
        // of the crate's env-mutating tests.
        unsafe { std::env::set_var("ITSY_HOME", "/tmp/itsy-test-home") };
        assert_eq!(config_dir(), PathBuf::from("/tmp/itsy-test-home"));
        match original {
            Some(v) => unsafe { std::env::set_var("ITSY_HOME", v) },
            None => unsafe { std::env::remove_var("ITSY_HOME") },
        }
    }
}
