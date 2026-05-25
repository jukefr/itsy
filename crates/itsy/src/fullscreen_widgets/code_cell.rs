//! Code-cell widget — a syntax-highlighted code block wrapped in an
//! [`output_block`] with an optional "Output" section. Mirrors OMP's
//! `code-cell.ts` rendering: bordered frame with state colors, language tag
//! in the header, line truncation with `... (N more lines)` hint, and an
//! optional Output section for the result of running the code.
//!
//! Syntax highlighting here is intentionally minimal — a token-pattern
//! matcher with a small keyword set per language. The full agent loop has
//! `crate::tui::render_code_block` which uses regex-based highlighting; this
//! reuses the same patterns through a simpler API and produces ratatui
//! `Span`s ready to drop into the output_block.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::output_block::{render_output_block, OutputBlock, Section};
use super::symbols::SymbolTheme;
use super::theme::{BlockState, Theme};

/// Inputs to [`render_code_cell`].
#[derive(Debug, Clone)]
pub struct CodeCell<'a> {
    pub code: &'a str,
    pub language: Option<&'a str>,
    /// Optional header title (e.g. "edit foo.rs" or "shell").
    pub title: Option<String>,
    pub state: BlockState,
    /// Captured output (stdout/stderr / tool result). `None` → no Output section.
    pub output: Option<&'a str>,
    /// Cap code lines at `code_max_lines` (collapse to "X more lines"). 0 = no cap.
    pub code_max_lines: usize,
    /// Cap output lines similarly.
    pub output_max_lines: usize,
    /// Right-side meta in the header bar (e.g. "12ms").
    pub meta: Option<String>,
    pub width: u16,
    pub theme: &'a Theme,
    pub symbols: &'a SymbolTheme,
}

/// Render the cell. Returns the styled lines ready to push into the chat.
pub fn render_code_cell(opts: CodeCell<'_>) -> Vec<Line<'static>> {
    let CodeCell {
        code, language, title, state, output,
        code_max_lines, output_max_lines, meta, width, theme, symbols,
    } = opts;

    let language_owned: Option<String> = language.map(|s| s.to_string());

    // Header: optional title + language tag.
    let header = build_header(title.as_deref(), language_owned.as_deref(), &state, symbols);

    // Code section: split into lines, truncate, syntax-highlight.
    let code_lines = highlight_lines(code, language_owned.as_deref(), theme);
    let code_section = cap_with_more_hint(code_lines, code_max_lines, theme, "lines");

    let mut sections = vec![Section { label: None, lines: code_section }];

    if let Some(out) = output {
        let trimmed = out.trim_end();
        if !trimmed.is_empty() {
            let out_lines: Vec<Line<'static>> = trimmed
                .lines()
                .map(|l| Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme.tool_output),
                )))
                .collect();
            let out_section = cap_with_more_hint(out_lines, output_max_lines, theme, "lines");
            sections.push(Section {
                label: Some(String::from("Output")),
                lines: out_section,
            });
        }
    }

    render_output_block(OutputBlock {
        header: Some(header),
        header_meta: meta,
        state,
        sections,
        width,
        symbols,
        theme,
    })
}

fn build_header(
    title: Option<&str>,
    language: Option<&str>,
    _state: &BlockState,
    _symbols: &SymbolTheme,
) -> String {
    match (title, language) {
        (Some(t), Some(l)) if !l.is_empty() => format!("{} {}", t, l),
        (Some(t), _) => t.to_string(),
        (None, Some(l)) if !l.is_empty() => format!("code · {}", l),
        _ => String::from("code"),
    }
}

/// Cap a list of lines at `max`. If anything trimmed, append a dim
/// `... (N more <unit>)` line.
fn cap_with_more_hint(
    mut lines: Vec<Line<'static>>,
    max: usize,
    theme: &Theme,
    unit: &str,
) -> Vec<Line<'static>> {
    if max == 0 || lines.len() <= max {
        return lines;
    }
    let remaining = lines.len() - max;
    lines.truncate(max);
    lines.push(Line::from(vec![Span::styled(
        format!("... ({} more {unit})", remaining),
        Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC),
    )]));
    lines
}

/// Lightweight syntax tokenizer. Recognises a small per-language keyword
/// set, strings (`"…"`), numbers, comments (`//…` or `#…`). Everything else
/// is text-colored.
fn highlight_lines(code: &str, language: Option<&str>, theme: &Theme) -> Vec<Line<'static>> {
    let language = language.unwrap_or("").to_lowercase();
    let keywords: &[&str] = match language.as_str() {
        "rust" | "rs" => &[
            "fn", "let", "mut", "pub", "use", "mod", "impl", "trait", "struct", "enum",
            "match", "if", "else", "for", "while", "loop", "return", "break", "continue",
            "as", "in", "ref", "move", "where", "self", "Self", "async", "await", "dyn",
            "const", "static", "type", "crate", "super", "extern", "unsafe",
        ],
        "python" | "py" => &[
            "def", "class", "import", "from", "as", "if", "elif", "else", "for", "while",
            "return", "yield", "with", "try", "except", "finally", "raise", "pass",
            "lambda", "in", "is", "not", "and", "or", "None", "True", "False", "self",
            "async", "await",
        ],
        "ts" | "typescript" | "js" | "javascript" => &[
            "function", "const", "let", "var", "if", "else", "for", "while", "do",
            "return", "break", "continue", "switch", "case", "default", "class",
            "extends", "implements", "interface", "type", "enum", "import", "export",
            "from", "as", "async", "await", "new", "this", "super", "typeof", "instanceof",
            "void", "null", "undefined", "true", "false",
        ],
        "go" => &[
            "func", "var", "const", "type", "struct", "interface", "package", "import",
            "if", "else", "for", "range", "switch", "case", "default", "return", "break",
            "continue", "go", "defer", "chan", "select", "nil", "true", "false",
        ],
        "bash" | "sh" | "shell" => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while",
            "case", "esac", "function", "return", "break", "continue", "export",
            "local", "readonly", "echo", "exit",
        ],
        _ => &[],
    };

    let comment_prefix: &[&str] = match language.as_str() {
        "rust" | "rs" | "ts" | "typescript" | "js" | "javascript" | "go" => &["//"],
        "python" | "py" | "bash" | "sh" | "shell" => &["#"],
        _ => &[],
    };

    code.lines().map(|line| highlight_line(line, keywords, comment_prefix, theme)).collect()
}

fn highlight_line(
    line: &str,
    keywords: &[&str],
    comment_prefix: &[&str],
    theme: &Theme,
) -> Line<'static> {
    // Whole-line comment?
    let trimmed_start = line.trim_start();
    let leading = line.len() - trimmed_start.len();
    for p in comment_prefix {
        if trimmed_start.starts_with(p) {
            return Line::from(vec![
                Span::raw(" ".repeat(leading)),
                Span::styled(
                    trimmed_start.to_string(),
                    Style::default().fg(theme.syntax_comment).add_modifier(Modifier::ITALIC),
                ),
            ]);
        }
    }

    let kw_style = Style::default().fg(theme.syntax_keyword);
    let str_style = Style::default().fg(theme.syntax_string);
    let num_style = Style::default().fg(theme.syntax_number);
    let text_style = Style::default().fg(theme.text);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = line.char_indices().peekable();
    let bytes = line.as_bytes();

    while let Some(&(i, ch)) = chars.peek() {
        if ch == '"' || ch == '\'' {
            // Walk forward until the matching quote (or EOL).
            let quote = ch;
            chars.next();
            let start = i;
            let mut end = i + ch.len_utf8();
            while let Some(&(j, c)) = chars.peek() {
                end = j + c.len_utf8();
                chars.next();
                if c == quote { break; }
                if c == '\\' {
                    if let Some(&(k, esc)) = chars.peek() {
                        end = k + esc.len_utf8();
                        chars.next();
                    }
                }
            }
            spans.push(Span::styled(line[start..end].to_string(), str_style));
            continue;
        }
        if ch.is_ascii_digit() {
            let start = i;
            let mut end = i + ch.len_utf8();
            chars.next();
            while let Some(&(j, c)) = chars.peek() {
                if c.is_ascii_digit() || c == '.' || c == '_' {
                    end = j + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            spans.push(Span::styled(line[start..end].to_string(), num_style));
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            let mut end = i + ch.len_utf8();
            chars.next();
            while let Some(&(j, c)) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    end = j + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let word = &line[start..end];
            if keywords.contains(&word) {
                spans.push(Span::styled(word.to_string(), kw_style));
            } else {
                spans.push(Span::styled(word.to_string(), text_style));
            }
            continue;
        }
        // Single non-special char.
        chars.next();
        spans.push(Span::styled(line[i..i + ch.len_utf8()].to_string(), text_style));
    }

    let _ = bytes; // suppress lint
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen_widgets::symbols::UNICODE;

    fn t() -> Theme { Theme::dark() }
    fn text(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>().join("\n")
    }

    /// A code cell with no language still renders cleanly.
    #[test]
    fn no_language_renders_generic_header() {
        let lines = render_code_cell(CodeCell {
            code: "hello",
            language: None,
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &t(),
            symbols: &UNICODE,
        });
        let s = text(&lines);
        assert!(s.contains("code"));
        assert!(s.contains("hello"));
    }

    /// Language tag appears in the header.
    #[test]
    fn language_tag_appears_in_header() {
        let lines = render_code_cell(CodeCell {
            code: "fn main() {}",
            language: Some("rust"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let first = text(&lines).lines().next().unwrap().to_string();
        assert!(first.contains("rust"), "language must show in header; got {first:?}");
    }

    /// Output section gets its own labelled separator.
    #[test]
    fn output_section_has_labelled_separator() {
        let lines = render_code_cell(CodeCell {
            code: "echo hi",
            language: Some("bash"),
            title: None,
            state: BlockState::Success,
            output: Some("hi\n"),
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &t(),
            symbols: &UNICODE,
        });
        let s = text(&lines);
        assert!(s.contains("Output"), "Output label must appear; got: {s}");
        assert!(s.contains('├'), "section separator (├) must appear");
    }

    /// Code line truncation: cap at 3 lines, leave the "... (X more lines)" marker.
    #[test]
    fn long_code_truncates_with_more_marker() {
        let code: String = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let lines = render_code_cell(CodeCell {
            code: &code,
            language: None,
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 3,
            output_max_lines: 0,
            meta: None,
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let s = text(&lines);
        assert!(s.contains("more lines"), "expected truncation marker; got: {s}");
        assert!(s.contains("line 0"));
        // line 19 must NOT appear since we truncated at 3.
        assert!(!s.contains("line 19"));
    }

    /// Empty / whitespace-only output is dropped (no empty Output section).
    #[test]
    fn empty_output_is_dropped() {
        let lines = render_code_cell(CodeCell {
            code: "x",
            language: None,
            title: None,
            state: BlockState::Success,
            output: Some("   \n  \n"),
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &t(),
            symbols: &UNICODE,
        });
        let s = text(&lines);
        assert!(!s.contains("Output"),
            "whitespace-only output must not produce a section; got: {s}");
    }

    /// Comments render in italic syntax-comment color.
    /// Anti-regression: a `//` line should NOT be highlighted as code keywords.
    #[test]
    fn rust_line_comment_styled_as_comment() {
        let lines = render_code_cell(CodeCell {
            code: "// just a comment with fn keyword inside",
            language: Some("rust"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 60,
            theme: &t(),
            symbols: &UNICODE,
        });
        let theme = t();
        // Find the line containing the comment.
        let comment_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("just a comment"))
        }).expect("comment line must exist");
        let comment_span = comment_line.spans.iter()
            .find(|s| s.content.contains("comment"))
            .unwrap();
        assert_eq!(comment_span.style.fg, Some(theme.syntax_comment),
            "comment must use syntax_comment color");
        assert!(comment_span.style.add_modifier.contains(Modifier::ITALIC),
            "comment must be italic");
    }

    /// Python `#` comment also recognized.
    #[test]
    fn python_hash_comment_styled() {
        let lines = render_code_cell(CodeCell {
            code: "# a comment",
            language: Some("python"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let theme = t();
        let comment_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("a comment"))
        }).unwrap();
        let comment_span = comment_line.spans.iter()
            .find(|s| s.content.contains("comment"))
            .unwrap();
        assert_eq!(comment_span.style.fg, Some(theme.syntax_comment));
    }

    /// Keywords are colored differently from identifiers.
    #[test]
    fn rust_keywords_highlighted() {
        let lines = render_code_cell(CodeCell {
            code: "fn foo()",
            language: Some("rust"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &t(),
            symbols: &UNICODE,
        });
        let theme = t();
        // Find a span with content "fn" — it must have the keyword color.
        let code_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content == "fn")
        }).expect("'fn' span must exist");
        let fn_span = code_line.spans.iter().find(|s| s.content == "fn").unwrap();
        assert_eq!(fn_span.style.fg, Some(theme.syntax_keyword),
            "fn keyword must use syntax_keyword color");
    }

    /// String literals are colored as strings, with the quotes included.
    #[test]
    fn string_literal_styled_as_string() {
        let lines = render_code_cell(CodeCell {
            code: r#"let s = "hello""#,
            language: Some("rust"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let theme = t();
        let line = &lines[1]; // 0=top bar, 1=first code line.
        let str_span = line.spans.iter().find(|s| s.content.contains("\"hello\""));
        assert!(str_span.is_some(), "string literal must be one span");
        assert_eq!(str_span.unwrap().style.fg, Some(theme.syntax_string));
    }

    /// Number literals colored as numbers.
    #[test]
    fn number_literal_styled_as_number() {
        let lines = render_code_cell(CodeCell {
            code: "let n = 42;",
            language: Some("rust"),
            title: None,
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &t(),
            symbols: &UNICODE,
        });
        let theme = t();
        let line = &lines[1];
        let num_span = line.spans.iter().find(|s| s.content == "42");
        assert!(num_span.is_some(), "number must be its own span");
        assert_eq!(num_span.unwrap().style.fg, Some(theme.syntax_number));
    }

    /// Meta string lands in the top-bar `· meta` slot.
    #[test]
    fn meta_appears_after_header() {
        let lines = render_code_cell(CodeCell {
            code: "x",
            language: None,
            title: Some("step 1".into()),
            state: BlockState::Success,
            output: None,
            code_max_lines: 0,
            output_max_lines: 0,
            meta: Some("12ms".into()),
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let first = text(&lines).lines().next().unwrap().to_string();
        assert!(first.contains("step 1"));
        assert!(first.contains("12ms"));
        assert!(first.contains('·'));
    }

    /// Output line truncation also applies independently.
    #[test]
    fn long_output_truncates() {
        let output: String = (0..30).map(|i| format!("o{i}")).collect::<Vec<_>>().join("\n");
        let lines = render_code_cell(CodeCell {
            code: "x",
            language: None,
            title: None,
            state: BlockState::Success,
            output: Some(&output),
            code_max_lines: 0,
            output_max_lines: 5,
            meta: None,
            width: 40,
            theme: &t(),
            symbols: &UNICODE,
        });
        let s = text(&lines);
        assert!(s.contains("more lines"), "output truncation marker must appear; got: {s}");
        assert!(s.contains("o0"));
        assert!(!s.contains("o29"));
    }

    /// Error state colors the border red.
    #[test]
    fn error_state_colors_border_red() {
        let theme = t();
        let lines = render_code_cell(CodeCell {
            code: "fail",
            language: None,
            title: None,
            state: BlockState::Error,
            output: Some("boom"),
            code_max_lines: 0,
            output_max_lines: 0,
            meta: None,
            width: 30,
            theme: &theme,
            symbols: &UNICODE,
        });
        // First span of the top-bar line is the top-left corner styled with border color.
        let top = &lines[0];
        assert_eq!(top.spans[0].style.fg, Some(theme.error));
    }
}
