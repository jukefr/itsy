//! Export a session as a shareable transcript.
//!
//! Supports markdown, JSON, and HTML output, plus a one-shot
//! upload to a GitHub Gist via the `gh` CLI. All output passes
//! through [`crate::security::redact_value`] / [`crate::security::redact_string`]
//! to strip secrets that may appear in messages.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{redact_string, redact_value};

/// Output format for [`export`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareFormat {
    Markdown,
    Json,
    Html,
}

impl ShareFormat {
    pub fn from_path(p: &Path) -> Self {
        match p.extension().and_then(|s| s.to_str()) {
            Some("json") => ShareFormat::Json,
            Some("html") | Some("htm") => ShareFormat::Html,
            _ => ShareFormat::Markdown,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ShareFormat::Markdown => "md",
            ShareFormat::Json => "json",
            ShareFormat::Html => "html",
        }
    }
}

/// Result of uploading a session to GitHub Gist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GistResult {
    pub success: bool,
    pub url: Option<String>,
    pub error: Option<String>,
}

/// Render a session as a markdown transcript (with redaction).
pub fn export_markdown(session: &Value) -> String {
    let safe = redact_value(session);
    let title = safe.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
    let model = safe.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let created = safe
        .get("createdAt")
        .or_else(|| safe.get("created_at"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let messages = safe.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut md = String::new();
    md.push_str(&format!("# itsy Session: {}\n\n", title));
    md.push_str(&format!("**Model:** {}\n", model));
    md.push_str(&format!("**Date:** {}\n", created));
    md.push_str(&format!("**Messages:** {}\n\n", messages.len()));
    md.push_str("---\n\n");

    for msg in &messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        match role {
            "user" => md.push_str(&format!("## You\n\n{}\n\n", content)),
            "assistant" => md.push_str(&format!("## AI\n\n{}\n\n", content)),
            "tool" => {
                let snippet: String = content.chars().take(200).collect();
                md.push_str(&format!("> Tool: {}\n\n", snippet));
            }
            other => md.push_str(&format!("### {}\n\n{}\n\n", other, content)),
        }
    }
    md
}

/// Render a session as redacted, pretty-printed JSON.
pub fn export_json(session: &Value) -> String {
    let safe = redact_value(session);
    serde_json::to_string_pretty(&safe).unwrap_or_else(|_| "{}".into())
}

/// Render a session as a minimal standalone HTML document.
pub fn export_html(session: &Value) -> String {
    let safe = redact_value(session);
    let title = safe.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
    let model = safe.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let created = safe
        .get("createdAt")
        .or_else(|| safe.get("created_at"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let messages = safe.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut html = String::new();
    html.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
    html.push_str(&format!("<title>itsy session: {}</title>\n", html_escape(title)));
    html.push_str("<style>body{font-family:system-ui,sans-serif;max-width:48rem;margin:2rem auto;padding:0 1rem;line-height:1.5}");
    html.push_str(".msg{margin:1rem 0;padding:.75rem 1rem;border-radius:.5rem}.user{background:#eef}.assistant{background:#efe}.tool{background:#f5f5f5;font-family:ui-monospace,monospace;font-size:.85em}pre{white-space:pre-wrap;word-break:break-word}</style>\n");
    html.push_str("</head><body>\n");
    html.push_str(&format!("<h1>itsy session: {}</h1>\n", html_escape(title)));
    html.push_str(&format!(
        "<p><strong>Model:</strong> {} &middot; <strong>Date:</strong> {} &middot; <strong>Messages:</strong> {}</p>\n",
        html_escape(model),
        html_escape(created),
        messages.len()
    ));
    for msg in &messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let class = match role {
            "user" => "user",
            "assistant" => "assistant",
            "tool" => "tool",
            _ => "msg",
        };
        html.push_str(&format!(
            "<div class=\"msg {}\"><strong>{}</strong><pre>{}</pre></div>\n",
            class,
            html_escape(role),
            html_escape(&content)
        ));
    }
    html.push_str("</body></html>\n");
    html
}

/// Render in the requested format.
pub fn export(session: &Value, format: ShareFormat) -> String {
    match format {
        ShareFormat::Markdown => export_markdown(session),
        ShareFormat::Json => export_json(session),
        ShareFormat::Html => export_html(session),
    }
}

/// Write a rendered transcript to `output_path` with `0600` permissions.
pub fn export_to_file(session: &Value, output_path: &Path, format: ShareFormat) -> std::io::Result<PathBuf> {
    let body = export(session, format);
    let mut f = fs::File::create(output_path)?;
    f.write_all(body.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(output_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(output_path, perms)?;
    }
    Ok(output_path.to_path_buf())
}

/// Backward-compatible helper used by older callers: take a message array
/// (not a full session object) and render markdown.
pub fn export_messages_markdown(messages: &[Value]) -> String {
    let session = serde_json::json!({ "messages": messages });
    export_markdown(&session)
}

/// Upload a session to a GitHub Gist using the `gh` CLI. Requires the `gh`
/// binary to be on PATH and authenticated.
pub fn export_to_gist(session: &Value) -> GistResult {
    let safe_id = sanitize_id(session.get("id").and_then(|v| v.as_str()).unwrap_or(""));
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    let tmp_file = std::env::temp_dir().join(format!("itsy-session-{}-{}.md", safe_id, now_ms));

    if let Err(e) = export_to_file(session, &tmp_file, ShareFormat::Markdown) {
        return GistResult { success: false, url: None, error: Some(redact_string(&e.to_string())) };
    }

    let raw_title = session.get("title").and_then(|v| v.as_str()).unwrap_or("untitled");
    let title_truncated: String = raw_title.chars().take(80).collect();
    let desc = format!("itsy session: {}", title_truncated);

    let output = Command::new("gh")
        .args([
            "gist",
            "create",
            tmp_file.to_string_lossy().as_ref(),
            "--desc",
            &desc,
            "--public",
        ])
        .output();

    let _ = fs::remove_file(&tmp_file);

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let url = stdout.trim().lines().last().map(|s| s.to_string());
            GistResult { success: true, url, error: None }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            GistResult { success: false, url: None, error: Some(redact_string(&err)) }
        }
        Err(e) => GistResult { success: false, url: None, error: Some(redact_string(&e.to_string())) },
    }
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn markdown_contains_title_and_messages() {
        let s = json!({
            "title": "Hello",
            "model": "m",
            "createdAt": "2024",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
            ]
        });
        let md = export_markdown(&s);
        assert!(md.contains("# itsy Session: Hello"));
        assert!(md.contains("## You\n\nhi"));
        assert!(md.contains("## AI\n\nhello"));
    }

    #[test]
    fn html_escapes_content() {
        let s = json!({"title": "<x>", "messages": [{"role":"user","content":"a&b"}]});
        let h = export_html(&s);
        assert!(h.contains("&lt;x&gt;"));
        assert!(h.contains("a&amp;b"));
    }

    #[test]
    fn sanitize_id_strips_unsafe_chars() {
        assert_eq!(sanitize_id("abc-123_xyz; rm -rf /"), "abc-123_xyzrm-rf");
    }
}
