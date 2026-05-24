//! Tool-call utilities: idempotent-write key and bash read-only classifier.
//!
//! The pure-tool result cache (ToolDedup) was removed because caching
//! read results across mutations caused stale-content bugs worse than
//! any spiral benefit it prevented. Repeat-call loop detection is
//! handled by the `tool_repeat_counts` guard in `bin/itsy.rs`.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Stable hash key for idempotent-write dedup (memory_remember / memory_forget).
/// Same turn, same args → skip re-execution.
pub fn idempotent_write_key(name: &str, args: &Value) -> String {
    let norm = sorted_keys_json(args);
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b"|");
    h.update(norm.as_bytes());
    let hex = format!("{:x}", h.finalize());
    hex.chars().take(16).collect()
}

/// Returns true when a bash command has no observable side effects.
/// Used by the contract gate to distinguish read-only from mutating bash.
pub fn bash_is_read_only(args: &Value) -> bool {
    let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    if cmd.contains('>') {
        return false;
    }
    const READ_ONLY: &[&str] = &[
        "ls", "cat", "pwd", "find", "grep", "rg", "echo", "stat", "wc",
        "head", "tail", "file", "which", "type", "du", "df", "tree",
        "sort", "uniq", "awk", "sed", "cut", "tr", "diff", "git",
        "printenv", "env", "uname", "whoami", "date", "hostname",
        "true", "false", "test", "[",
    ];
    const FORBID: &[&str] = &[
        "rm", "mv", "cp", "mkdir", "rmdir", "touch", "chmod", "chown",
        "ln", "install", "dd", "sync", "kill", "killall", "pkill",
        "cd", "export", "unset", "source", ".", "exec", "eval",
        "tee", "shred",
    ];
    for segment in cmd.split(['|', ';', '&']) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        let head = trimmed.split_whitespace().next().unwrap_or("");
        let head = head.trim_start_matches('(').trim_start_matches('$');
        if head == "cd" {
            let rest: Vec<&str> = trimmed.split_whitespace().collect();
            let cd_safe = rest.len() == 2
                && !rest[1].contains('*')
                && !rest[1].contains('$')
                && !rest[1].contains('`');
            if cd_safe {
                continue;
            }
            return false;
        }
        if FORBID.contains(&head) {
            return false;
        }
        if (head == "sed" || head == "awk") && trimmed.contains("-i") {
            return false;
        }
        if head == "git" {
            let sub = trimmed.split_whitespace().nth(1).unwrap_or("");
            const GIT_READONLY: &[&str] = &[
                "status", "diff", "log", "show", "branch", "remote",
                "blame", "tag", "config", "ls-files", "ls-tree",
                "rev-parse", "describe", "shortlog", "stash",
                "reflog", "cherry", "for-each-ref", "cat-file",
                "rev-list", "name-rev", "whatchanged", "fsck",
                "verify-commit", "verify-tag", "count-objects",
                "ls-remote", "show-ref", "symbolic-ref",
                "check-ignore", "check-attr", "grep",
            ];
            if !GIT_READONLY.contains(&sub) {
                return false;
            }
        }
        if !READ_ONLY.contains(&head) {
            return false;
        }
    }
    true
}

fn sorted_keys_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&k.replace('"', "\\\""));
                out.push_str("\":");
                out.push_str(&serde_json::to_string(&map[*k]).unwrap_or_default());
            }
            out.push('}');
            out
        }
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b(cmd: &str) -> Value {
        json!({ "command": cmd })
    }

    #[test]
    fn read_only_commands_are_pure() {
        assert!(bash_is_read_only(&b("ls /tmp")));
        assert!(bash_is_read_only(&b("cat /etc/passwd")));
        assert!(bash_is_read_only(&b("git status")));
        assert!(bash_is_read_only(&b("git log --oneline")));
    }

    #[test]
    fn mutating_commands_are_impure() {
        assert!(!bash_is_read_only(&b("rm -rf /tmp/x")));
        assert!(!bash_is_read_only(&b("echo hi > /tmp/x")));
        assert!(!bash_is_read_only(&b("git commit -m foo")));
        assert!(!bash_is_read_only(&b("sed -i s/a/b/ /tmp/x")));
    }

    #[test]
    fn cd_then_readonly_is_pure() {
        assert!(bash_is_read_only(&b("cd /app && ls")));
        assert!(bash_is_read_only(&b("cd /app && git log -5")));
    }

    #[test]
    fn cd_with_glob_or_flags_is_impure() {
        assert!(!bash_is_read_only(&b("cd /app/*/src && ls")));
        assert!(!bash_is_read_only(&b("cd $HOME && ls")));
    }

    #[test]
    fn cd_then_mutating_is_impure() {
        assert!(!bash_is_read_only(&b("cd /app && rm foo")));
    }
}
