//! Centralised redaction, ANSI stripping,
//! shell escaping, and path safety utilities.

use std::path::{Component, Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

pub struct SecretPattern {
    pub name: &'static str,
    pub re: Regex,
}

pub static SECRET_PATTERNS: Lazy<Vec<SecretPattern>> = Lazy::new(|| {
    vec![
        SecretPattern { name: "openai_key", re: Regex::new(r"\bsk-(?:proj-|ant-|or-)?[A-Za-z0-9_-]{20,}\b").unwrap() },
        SecretPattern { name: "anthropic_key", re: Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b").unwrap() },
        SecretPattern { name: "bearer", re: Regex::new(r"\b[Bb]earer\s+[A-Za-z0-9_\-.=:+/]{16,}").unwrap() },
        SecretPattern { name: "github_pat", re: Regex::new(r"\bghp_[A-Za-z0-9]{30,}\b").unwrap() },
        SecretPattern { name: "github_oauth", re: Regex::new(r"\bgho_[A-Za-z0-9]{30,}\b").unwrap() },
        SecretPattern { name: "google_api", re: Regex::new(r"\bAIza[0-9A-Za-z_-]{30,}\b").unwrap() },
        SecretPattern { name: "aws_key", re: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap() },
        SecretPattern { name: "jwt", re: Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b").unwrap() },
        SecretPattern { name: "slack", re: Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap() },
        SecretPattern {
            name: "env_api_key",
            re: Regex::new(r#"\b([A-Z][A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|PWD|API)[A-Z0-9_]*)\s*=\s*["']?([^\s"'\n]{8,})["']?"#).unwrap(),
        },
        // Private key blocks are normally handled by an offline tool; the
        // multi-line form does not work with the regex crate's default DOTALL
        // semantics so we approximate with the (?s) flag.
        SecretPattern { name: "private_key", re: Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----").unwrap() },
    ]
});

pub static ALWAYS_REDACT_KEYS: &[&str] = &[
    "password", "passwd", "pwd", "secret", "token", "api_key", "apikey",
    "authorization", "auth", "bearer", "cookie", "session_token", "refresh_token",
    "access_token", "private_key", "client_secret", "webhook_secret",
    "openai_api_key", "anthropic_api_key", "deepseek_api_key",
];

/// Redact secrets from a string. Returns the string with sensitive
/// substrings replaced by `[REDACTED:<kind>]`.
pub fn redact_string(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = input.to_string();
    for sp in SECRET_PATTERNS.iter() {
        if sp.name == "env_api_key" {
            out = sp.re.replace_all(&out, "$1=[REDACTED:env_value]").into_owned();
        } else {
            out = sp.re.replace_all(&out, format!("[REDACTED:{}]", sp.name).as_str()).into_owned();
        }
    }
    out
}

/// Recursively redact a JSON-like value.
pub fn redact_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact_string(s)),
        serde_json::Value::Array(arr) => serde_json::Value::Array(arr.iter().map(redact_value).collect()),
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                if ALWAYS_REDACT_KEYS.contains(&k.to_lowercase().as_str()) {
                    out.insert(k.clone(), serde_json::Value::String("[REDACTED]".into()));
                } else {
                    out.insert(k.clone(), redact_value(v));
                }
            }
            serde_json::Value::Object(out)
        }
        _ => value.clone(),
    }
}

static SENSITIVE_PATH_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)[/\\]\.ssh[/\\]").unwrap(),
        Regex::new(r"(?i)[/\\]\.aws[/\\]credentials").unwrap(),
        Regex::new(r"(?i)[/\\]\.gnupg[/\\]").unwrap(),
        Regex::new(r"(?i)[/\\]\.netrc$").unwrap(),
        Regex::new(r"(?i)[/\\]etc[/\\](shadow|gshadow|sudoers)").unwrap(),
        Regex::new(r"(?i)[/\\]\.password-store[/\\]").unwrap(),
        Regex::new(r"(?i)[/\\]\.docker[/\\]config\.json$").unwrap(),
        Regex::new(r"(?i)[/\\]\.kube[/\\]config$").unwrap(),
    ]
});

pub struct SafePath {
    pub full_path: PathBuf,
    pub display_path: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PathOptions {
    pub allow_outside: bool,
    pub allow_home: bool,
}

impl Default for PathOptions {
    fn default() -> Self {
        // Many real-world tasks instruct the model to read or write files
        // outside the working directory (e.g. `/data/...`, `/tmp/...`,
        // `/etc/...`). Refusing them outright forces the model into
        // brittle workarounds (or fabricating the data, as IQ2-quant
        // models tend to do). Sensitive paths are still blocked by the
        // SENSITIVE_PATH_RES list further down.
        //
        // Default ON; set ITSY_ALLOW_OUTSIDE_PATHS=false to force the
        // legacy strict-confinement behavior.
        let allow_outside = match std::env::var("ITSY_ALLOW_OUTSIDE_PATHS")
            .ok()
            .as_deref()
        {
            Some("0") | Some("false") | Some("no") | Some("off") => false,
            _ => true,
        };
        Self { allow_outside, allow_home: false }
    }
}

/// Resolve a user-supplied path safely against `cwd`. Mirrors
/// `safeResolvePath` semantics.
pub fn safe_resolve_path(req_path: &str, cwd: &Path, options: PathOptions) -> Result<SafePath, String> {
    if req_path.is_empty() {
        return Err("path must be a non-empty string".into());
    }
    if req_path.contains('\0') {
        return Err("path contains NUL byte".into());
    }

    let mut candidate = req_path.to_string();
    if candidate == "~" || candidate.starts_with("~/") || candidate.starts_with("~\\") {
        if !options.allow_home {
            return Err("home-relative paths are blocked".into());
        }
        let home = dirs::home_dir().ok_or_else(|| "no home directory".to_string())?;
        let rest = &candidate[2..];
        candidate = home.join(rest).to_string_lossy().into_owned();
    }
    if let Some(stripped) = candidate.strip_prefix("./") {
        candidate = stripped.to_string();
    } else if let Some(stripped) = candidate.strip_prefix(".\\") {
        candidate = stripped.to_string();
    }

    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let candidate_path = Path::new(&candidate);
    let full = if candidate_path.is_absolute() {
        candidate_path.to_path_buf()
    } else {
        cwd.join(candidate_path)
    };
    let full = normalize(&full);

    let full_str = full.to_string_lossy();
    for re in SENSITIVE_PATH_RES.iter() {
        if re.is_match(&full_str) {
            return Err("path is sensitive (auth credentials)".into());
        }
    }

    if !options.allow_outside {
        let rel = pathdiff(&full, &cwd);
        if let Some(r) = &rel {
            if r.starts_with("..") || Path::new(r).is_absolute() {
                return Err("path resolves outside project root".into());
            }
        } else {
            return Err("path resolves outside project root".into());
        }
    }

    let display = pathdiff(&full, &cwd).unwrap_or_else(|| {
        full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| full_str.to_string())
    });

    Ok(SafePath { full_path: full, display_path: display })
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn pathdiff(target: &Path, base: &Path) -> Option<String> {
    let target_comps: Vec<_> = target.components().collect();
    let base_comps: Vec<_> = base.components().collect();
    let mut i = 0;
    while i < target_comps.len() && i < base_comps.len() && target_comps[i] == base_comps[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..base_comps.len() {
        out.push("..");
    }
    for c in &target_comps[i..] {
        out.push(c.as_os_str());
    }
    Some(out.to_string_lossy().into_owned())
}

// ─── Shell escaping ─────────────────────────────────────────────────────────

pub fn escape_shell_arg(value: &str) -> String {
    // The JS version throws on NUL; we instead strip NUL bytes so a single
    // malformed input can't silently collapse an arg into "" and shift the
    // meaning of a command. NUL is never valid in a POSIX shell argument
    // anyway.
    let cleaned: String = value.chars().filter(|c| *c != '\0').collect();
    if cfg!(windows) {
        // CMD: wrap in double quotes, double internal double-quotes.
        format!("\"{}\"", cleaned.replace('"', "\"\""))
    } else {
        // POSIX: single-quote, escape any embedded single quote with '\\''
        format!("'{}'", cleaned.replace('\'', "'\\''"))
    }
}

/// Strict variant that mirrors the JS `escapeShellArg` semantics: returns an
/// `Err` if `value` contains a NUL byte. Useful where callers want to surface
/// the input error explicitly rather than rely on the lenient strip.
pub fn try_escape_shell_arg(value: &str) -> Result<String, &'static str> {
    if value.contains('\0') {
        return Err("shell argument contains NUL byte");
    }
    Ok(escape_shell_arg(value))
}

pub fn build_command(base: &str, trusted: &[&str], user_args: &[&str]) -> String {
    let mut parts: Vec<String> = vec![base.to_string()];
    for t in trusted {
        parts.push((*t).to_string());
    }
    for u in user_args {
        parts.push(escape_shell_arg(u));
    }
    parts.join(" ")
}

// ─── ANSI / control-char stripping ──────────────────────────────────────────

static ANSI_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"\x1b\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]").unwrap(),
        Regex::new(r"\x1b\][\x20-\x7e]*?(?:\x07|\x1b\\)").unwrap(),
        Regex::new(r"\x1b[PX\^_][\x20-\x7e]*?\x1b\\").unwrap(),
        Regex::new(r"\x1b[@-_]").unwrap(),
        Regex::new(r"[\x80-\x9f]").unwrap(),
    ]
});

static C0_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());

pub fn strip_ansi(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = input.to_string();
    for re in ANSI_RES.iter() {
        out = re.replace_all(&out, "").into_owned();
    }
    C0_RE.replace_all(&out, "").into_owned()
}

pub fn sanitize_tool_output(input: &str) -> String {
    let stripped = strip_ansi(input);
    let redacted = redact_string(&stripped);
    redacted.replace("\r\n", "\n")
}
