//! Rich classic TUI rendering: markdown-lite,
//! syntax-highlighted code blocks, status bar, welcome banner, diff display.


const C_RESET: &str = "\x1b[0m";
const C_BOLD: &str = "\x1b[1m";
const C_RED: &str = "\x1b[31m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_MAGENTA: &str = "\x1b[35m";
const C_CYAN: &str = "\x1b[36m";
const C_GRAY: &str = "\x1b[90m";
const C_WHITE_B: &str = "\x1b[97m";

pub fn paint(color: &str, s: &str) -> String {
    format!("{color}{s}{C_RESET}")
}

pub fn bold(s: &str) -> String {
    format!("{C_BOLD}{s}{C_RESET}")
}

pub fn render_markdown(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") && !in_code {
            in_code = true;
            code_lang = trimmed[3..].trim().to_string();
            code_buf.clear();
            continue;
        }
        if trimmed == "```" && in_code {
            in_code = false;
            output.push_str(&render_code_block(&code_buf, &code_lang));
            continue;
        }
        if in_code {
            if !code_buf.is_empty() {
                code_buf.push('\n');
            }
            code_buf.push_str(line);
            continue;
        }
        if let Some(rest) = line.strip_prefix("### ") {
            output.push_str(&paint(&format!("{C_BOLD}{C_CYAN}"), rest));
            output.push('\n');
        } else if let Some(rest) = line.strip_prefix("## ") {
            output.push_str(&bold(rest));
            output.push('\n');
        } else if let Some(rest) = line.strip_prefix("# ") {
            output.push_str(&paint(&format!("{C_BOLD}{C_WHITE_B}"), rest));
            output.push('\n');
        } else if line.contains("**") {
            output.push_str(&render_bold(line));
            output.push('\n');
        } else if line.contains('`') {
            output.push_str(&render_inline_code(line));
            output.push('\n');
        } else if line.trim_start().starts_with("- ") || line.trim_start().starts_with("* ") {
            output.push_str("  ");
            output.push_str(&paint(C_GRAY, "•"));
            output.push(' ');
            let rest = line.trim_start();
            output.push_str(&rest[2..]);
            output.push('\n');
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if in_code {
        output.push_str(&render_code_block(&code_buf, &code_lang));
    }
    output
}

fn render_bold(line: &str) -> String {
    let re = regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();
    re.replace_all(line, |c: &regex::Captures| bold(&c[1])).into_owned()
}

fn render_inline_code(line: &str) -> String {
    let re = regex::Regex::new(r"`([^`]+)`").unwrap();
    re.replace_all(line, |c: &regex::Captures| paint(C_YELLOW, &c[1])).into_owned()
}

pub fn render_code_block(code: &str, lang: &str) -> String {
    let border = paint(C_GRAY, &format!("  ┌{}", "─".repeat(60)));
    let footer = paint(C_GRAY, &format!("  └{}", "─".repeat(60)));
    let lang_tag = if lang.is_empty() { String::new() } else { paint(C_GRAY, &format!(" {lang}")) };
    let lines: String = code
        .split('\n')
        .map(|l| format!("{} {}", paint(C_GRAY, "  │"), highlight_line(l, lang)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{border}{lang_tag}\n{lines}\n{footer}\n")
}

fn highlight_line(line: &str, lang: &str) -> String {
    let lang_key = match lang {
        "typescript" => "ts",
        "javascript" => "js",
        other => other,
    };
    let kws: &[&str] = match lang_key {
        "js" => &["const","let","var","function","return","if","else","for","while","class","import","export","from","async","await","new","this","true","false","null","undefined"],
        "ts" => &["const","let","var","function","return","if","else","for","while","class","import","export","from","async","await","new","this","true","false","null","undefined","interface","type","enum","extends","implements"],
        "python" => &["def","class","return","if","else","elif","for","while","import","from","as","True","False","None","with","try","except","raise","yield","async","await","self"],
        "rust" => &["fn","let","mut","struct","enum","impl","pub","use","mod","if","else","for","while","match","return","self","true","false","Some","None","Ok","Err"],
        _ => &["const","let","var","function","return","if","else","for","while","class","import","export","from","async","await","new","this","true","false","null","undefined","interface","type","enum","extends","implements"],
    };
    let mut h = line.to_string();
    // The `regex` crate has no look-around / back-references, so each
    // quote style gets its own (non-capturing) pattern. Escaped quotes
    // (\", \', \`) are handled inside the alternation so embedded
    // escapes don't terminate the string early.
    static STR_RES: once_cell::sync::Lazy<[regex::Regex; 3]> = once_cell::sync::Lazy::new(|| {
        [
            regex::Regex::new(r#""(?:\\.|[^"\\])*""#).unwrap(),
            regex::Regex::new(r#"'(?:\\.|[^'\\])*'"#).unwrap(),
            regex::Regex::new(r#"`(?:\\.|[^`\\])*`"#).unwrap(),
        ]
    });
    for str_re in STR_RES.iter() {
        h = str_re
            .replace_all(&h, |c: &regex::Captures| paint(C_GREEN, &c[0]))
            .into_owned();
    }
    static LINE_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"//.*$").unwrap());
    h = LINE_RE.replace_all(&h, |c: &regex::Captures| paint(C_GRAY, &c[0])).into_owned();
    static PY_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"#.*$").unwrap());
    h = PY_RE.replace_all(&h, |c: &regex::Captures| paint(C_GRAY, &c[0])).into_owned();
    for kw in kws {
        let re = regex::Regex::new(&format!(r"\b{kw}\b")).unwrap();
        h = re.replace_all(&h, |c: &regex::Captures| paint(C_MAGENTA, &c[0])).into_owned();
    }
    static NUM_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\b(\d+)\b").unwrap());
    h = NUM_RE.replace_all(&h, |c: &regex::Captures| paint(C_CYAN, &c[0])).into_owned();
    h
}

pub fn render_status(history_len: usize) -> String {
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let short_cwd = cwd
        .rsplit(['/', '\\'])
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "  {} │ {} │ {}",
        paint(C_CYAN, &crate::settings::get().model_name),
        paint(C_GRAY, &format!("{history_len} msgs")),
        paint(C_GRAY, &short_cwd)
    )
}

pub fn render_welcome(graph_ok: bool) -> String {
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let lines = [
        "".to_string(),
        format!("{}{}", paint(&format!("{C_BOLD}{C_CYAN}"), "  ⚡ itsy"), paint(C_GRAY, &format!(" v{}", env!("CARGO_PKG_VERSION")))),
        "".to_string(),
        format!("  Model:    {}", crate::settings::get().model_name),
        format!("  Endpoint: {}", paint(C_GRAY, &crate::settings::get().base_url)),
        format!("  Graph:    {}", if graph_ok { paint(C_GREEN, "✓ indexed") } else { paint(C_GRAY, "disabled") }),
        format!("  Dir:      {}", paint(C_GRAY, &cwd)),
        "".to_string(),
        paint(C_GRAY, "  Type a message to chat. /help for commands. /quit to exit.").to_string(),
        "".to_string(),
    ];
    lines.join("\n")
}

pub fn tool_start(name: &str) -> String {
    format!("  {} {} ", paint(C_CYAN, "⚙"), paint(C_CYAN, name))
}

pub fn tool_success(msg: &str, ms: u64) -> String {
    format!("{} {} {}", paint(C_GREEN, "✓"), msg, paint(C_GRAY, &format!("{ms}ms")))
}

pub fn tool_error(msg: &str) -> String {
    format!("{} {}", paint(C_RED, "✗"), msg)
}

pub fn tool_edited(path: &str, line: u32, ms: u64) -> String {
    format!("{} Edited {}:{} {}", paint(C_YELLOW, "✓"), path, line, paint(C_GRAY, &format!("{ms}ms")))
}

pub fn tool_created(path: &str, lines: u32, ms: u64) -> String {
    format!("{} Created {} ({} lines) {}", paint(C_GREEN, "✓"), bold(path), lines, paint(C_GRAY, &format!("{ms}ms")))
}

pub fn tool_updated(path: &str, lines: u32, ms: u64) -> String {
    format!("{} Updated {} ({} lines) {}", paint(C_GREEN, "✓"), bold(path), lines, paint(C_GRAY, &format!("{ms}ms")))
}

pub fn tool_bash(cmd: &str, ms: u64) -> String {
    format!("{} {} {}", paint(C_GRAY, "$"), paint(C_GRAY, cmd), paint(C_GRAY, &format!("{ms}ms")))
}

pub fn improvement_loop(errors: &[String], attempt: u32, max: u32) -> String {
    let header = paint(C_YELLOW, &format!("⟳ {} error(s) — fix attempt {attempt}/{max}", errors.len()));
    let err_lines = errors
        .iter()
        .take(3)
        .map(|e| format!("    {}", paint(C_RED, e)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("  {header}\n{err_lines}")
}

pub fn improvement_fixed(path: &str, attempts: u32) -> String {
    format!("  {} {} — {}", paint(C_GREEN, "✓"), path, paint(C_GREEN, &format!("fixed after {attempts} attempt(s)")))
}

pub fn improvement_gave_up(path: &str, max: u32) -> String {
    format!("  {} {}: giving up after {max} fix attempts", paint(C_RED, "⚠"), path)
}

pub fn turn_summary(calls: u32) -> String {
    paint(C_GRAY, &format!("  ─── {calls} tool calls this turn ───"))
}

/// Compact one-block rendering of the active contract — title,
/// counts, and a per-assertion checkbox grid.
pub fn render_contract(c: &crate::session::contract::Contract) -> String {
    use crate::session::contract::{AssertionState, ContractStatus};
    let counts = c.counts();
    let status_color = match c.status {
        ContractStatus::Completed => C_GREEN,
        ContractStatus::Active => C_CYAN,
        ContractStatus::Aborted => C_RED,
        ContractStatus::Draft => C_GRAY,
    };
    let mut out = String::new();
    out.push_str(&paint(C_GRAY, "\n  ── contract ──"));
    out.push('\n');
    out.push_str(&format!(
        "  {} {} {}\n",
        paint(status_color, &format!("[{}]", c.status.as_str())),
        paint(&format!("{C_BOLD}"), &c.title),
        paint(
            C_GRAY,
            &format!(
                "({}/{}/{}/{})",
                counts.passed, counts.failed, counts.pending, counts.skipped
            )
        ),
    ));
    for a in &c.assertions {
        let (badge_color, badge) = match a.state {
            AssertionState::Passed => (C_GREEN, "✓"),
            AssertionState::Failed => (C_RED, "✗"),
            AssertionState::Skipped => (C_GRAY, "~"),
            AssertionState::Pending => (C_YELLOW, "·"),
        };
        out.push_str(&format!(
            "  {} {}  {}\n",
            paint(badge_color, badge),
            paint(C_GRAY, &a.id),
            a.text,
        ));
    }
    out
}

pub fn compacted(removed: u32) -> String {
    paint(C_GRAY, &format!("  [compacted {removed} old messages]"))
}

pub fn render_diff(path: &str, old_str: &str, new_str: &str, line_num: u32) -> String {
    let old_lines: Vec<&str> = old_str.split('\n').collect();
    let new_lines: Vec<&str> = new_str.split('\n').collect();
    if old_lines.len() > 8 && new_lines.len() > 8 {
        return String::new();
    }
    let mut out = paint(C_GRAY, &format!("    ┌─ {path}:{line_num}\n"));
    for line in old_lines.iter().take(5) {
        out.push_str(&paint(C_RED, &format!("    │ - {line}\n")));
    }
    if old_lines.len() > 5 {
        out.push_str(&paint(C_RED, &format!("    │ ... ({} more)\n", old_lines.len() - 5)));
    }
    for line in new_lines.iter().take(5) {
        out.push_str(&paint(C_GREEN, &format!("    │ + {line}\n")));
    }
    if new_lines.len() > 5 {
        out.push_str(&paint(C_GREEN, &format!("    │ ... ({} more)\n", new_lines.len() - 5)));
    }
    out.push_str(&paint(C_GRAY, "    └─"));
    out
}
