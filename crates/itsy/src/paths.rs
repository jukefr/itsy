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

use sha2::{Digest, Sha256};

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
/// The id is `<slug>-<hash10>`: the slug is the canonical basename with
/// non-alphanumerics replaced by `-`, the hash is the first 10 hex chars of
/// sha256 of the canonical absolute path.
pub fn project_id(cwd: &Path) -> String {
    let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut h = Sha256::new();
    h.update(canon.to_string_lossy().as_bytes());
    let hex = format!("{:x}", h.finalize());
    let short = &hex[..10];
    let slug: String = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let slug = if slug.is_empty() { "project".into() } else { slug };
    format!("{slug}-{short}")
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
