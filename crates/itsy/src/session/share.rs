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

    /// JSON export round-trips through serde — the structure is preserved.
    #[test]
    fn json_export_preserves_structure() {
        let s = json!({
            "title": "T", "model": "m", "messages": [{"role":"user","content":"x"}]
        });
        let json_str = export_json(&s);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["title"], "T");
        assert_eq!(parsed["model"], "m");
        assert_eq!(parsed["messages"][0]["role"], "user");
    }

    /// `export_json` redacts sensitive-looking strings (e.g. API keys).
    /// Anti-regression: shared sessions must never leak secrets.
    #[test]
    fn json_export_redacts_secrets() {
        let s = json!({
            "title": "T",
            "messages": [{"role":"user","content":"my key is sk-proj-abc123def456ghi789jkl012mno345pqr678"}]
        });
        let out = export_json(&s);
        // Either the secret is redacted, or this test alerts us if the redactor regresses.
        assert!(!out.contains("sk-proj-abc123def456ghi789jkl012mno345pqr678"),
            "session export must redact OpenAI-style keys; got: {out}");
    }

    /// Empty messages array produces well-formed Markdown (no panic).
    #[test]
    fn markdown_export_handles_empty_messages() {
        let s = json!({"title":"empty","messages":[]});
        let md = export_markdown(&s);
        assert!(md.contains("itsy Session: empty"));
        assert!(md.contains("Messages: 0") || md.contains("**Messages:** 0"));
    }

    /// HTML export sets a content-type charset and includes UTF-8 safe content.
    #[test]
    fn html_export_is_utf8_self_describing() {
        let s = json!({"title":"héllo","messages":[]});
        let h = export_html(&s);
        assert!(h.contains("charset=\"utf-8\""));
        assert!(h.contains("héllo"));
        assert!(h.starts_with("<!doctype html>") || h.starts_with("<!DOCTYPE html>"));
    }

    /// `export` dispatches to the right renderer.
    #[test]
    fn export_dispatches_by_format() {
        let s = json!({"title":"t","messages":[]});
        let md = export(&s, ShareFormat::Markdown);
        let js = export(&s, ShareFormat::Json);
        let h  = export(&s, ShareFormat::Html);
        assert!(md.contains("itsy Session"));
        assert!(js.contains("\"title\""));
        assert!(h.contains("<html") || h.contains("<HTML"));
    }

    /// `export_to_file` writes to disk and sets `0600` permissions on Unix.
    #[test]
    fn export_to_file_writes_with_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.md");
        let s = json!({"title":"t","messages":[]});
        let written = export_to_file(&s, &p, ShareFormat::Markdown).unwrap();
        assert_eq!(written, p);
        assert!(p.exists());
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("itsy Session"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "shared session must be 0600; got {:o}", mode);
        }
    }

    /// `html_escape` escapes the dangerous five characters (&, <, >, ", ').
    #[test]
    fn html_escape_covers_dangerous_chars() {
        assert_eq!(html_escape("<&>"), "&lt;&amp;&gt;");
        assert_eq!(html_escape("\""), "&quot;");
        assert_eq!(html_escape("'"), "&#39;");
        // Plain ASCII passes through.
        assert_eq!(html_escape("hello world"), "hello world");
    }

    /// `export_messages_markdown` formats the message stream. Currently it
    /// reuses the session renderer with a synthetic envelope; pin that the
    /// content makes it through and the role marker (## You / ## AI) appears.
    #[test]
    fn messages_markdown_renders_role_blocks() {
        let messages = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"hello"}),
        ];
        let md = export_messages_markdown(&messages);
        assert!(md.contains("## You"));
        assert!(md.contains("## AI"));
        assert!(md.contains("hi"));
        assert!(md.contains("hello"));
    }

    /// Tool messages render with the truncated-snippet prefix.
    #[test]
    fn markdown_tool_messages_are_truncated() {
        let s = json!({
            "title":"t",
            "messages":[{"role":"tool", "content": "X".repeat(500)}]
        });
        let md = export_markdown(&s);
        // Tool block uses "> Tool: " prefix.
        assert!(md.contains("> Tool:"));
        // Snippet is capped at 200 chars in the source — output should not
        // contain a 500-X run.
        let max_x_run = md.chars().fold((0usize, 0usize), |(cur, max), c| {
            if c == 'X' { let n = cur + 1; (n, max.max(n)) } else { (0, max) }
        }).1;
        assert!(max_x_run <= 200, "tool snippet must be truncated; got {max_x_run}");
    }
}
