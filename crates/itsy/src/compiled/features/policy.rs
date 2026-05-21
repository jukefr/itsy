//! Returns enabled-feature gates
//! based on environment configuration.

use std::env;

#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub feature: &'static str,
    pub enabled: bool,
    pub reason: &'static str,
}

pub fn is_enabled(feature: &str) -> PolicyDecision {
    let env_var = match feature {
        "diff_context" => "ITSY_DIFF_CONTEXT",
        "write_guard" => "ITSY_WRITE_GUARD",
        "shell_persist" => "ITSY_SHELL_PERSIST",
        "rtk" => "ITSY_RTK",
        "web_browse" => "ITSY_WEB_BROWSE",
        "auto_approve" => "ITSY_AUTO_APPROVE",
        "auto_commit" => "ITSY_AUTO_COMMIT",
        _ => {
            return PolicyDecision {
                feature: "unknown",
                enabled: false,
                reason: "no policy registered",
            }
        }
    };
    let raw = env::var(env_var).unwrap_or_default();
    let enabled = match feature {
        // Default-on features
        "write_guard" | "shell_persist" | "rtk" => raw != "false",
        // Default-off features
        _ => raw == "true",
    };
    PolicyDecision {
        feature: "feature",
        enabled,
        reason: "env-controlled",
    }
}
