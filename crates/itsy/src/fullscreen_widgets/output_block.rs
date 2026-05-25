//! Bordered output container — the visual primitive every OMP tool-result
//! and code-cell renders inside.
//!
//! Renders to `Vec<Line<'static>>` so it can be dropped into any ratatui
//! `Paragraph` or chat list. Frame shape:
//!
//! ```text
//! ┌──[ header · meta ]──────────┐
//! │ first section line          │
//! │ second section line         │
//! ├──[ Output ]─────────────────┤
//! │ output line                 │
//! └─────────────────────────────┘
//! ```
//!
//! Border color is picked from [`BlockState`] via the active [`Theme`]
//! (accent for running/pending, dim for success, error for error, etc.).

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::symbols::SymbolTheme;
use super::theme::{BlockState, Theme};

/// One labelled section inside a block. `lines` are already-styled
/// `Vec<Line<'static>>` so callers can do their own highlighting (syntax,
/// markdown, etc.) before passing them in.
#[derive(Debug, Clone)]
pub struct Section {
    /// Optional label rendered into the section separator bar:
    /// `├──[ label ]──┤`. Pass `None` for an unlabelled block.
    pub label: Option<String>,
    pub lines: Vec<Line<'static>>,
}

/// Inputs to [`render_output_block`].
#[derive(Debug, Clone)]
pub struct OutputBlock<'a> {
    pub header: Option<String>,
    /// Right-aligned-ish meta dot-separated into the header bar.
    pub header_meta: Option<String>,
    pub state: BlockState,
    pub sections: Vec<Section>,
    pub width: u16,
    pub symbols: &'a SymbolTheme,
    pub theme: &'a Theme,
}

/// Render the block into a list of ratatui `Line`s — one per row.
///
/// `width` is the available terminal column width; the block fills it
/// exactly. Lines too long for the inner content area are truncated with a
/// trailing `…`.
pub fn render_output_block(opts: OutputBlock<'_>) -> Vec<Line<'static>> {
    let OutputBlock { header, header_meta, state, sections, width, symbols, theme } = opts;
    let border_color = state.border_color(theme);
    let border_style = Style::default().fg(border_color);
    let box_ = &symbols.box_sharp;

    // Inner content width = total - "│ " prefix (2) - "│" suffix (1).
    let outer_width = width.max(4) as usize;
    let inner_width = outer_width.saturating_sub(3);

    let mut out: Vec<Line<'static>> = Vec::new();

    // ── Top bar ────────────────────────────────────────────────────────
    out.push(bar_line(
        box_.top_left,
        box_.top_right,
        header.as_deref(),
        header_meta.as_deref(),
        outer_width,
        border_style,
        symbols.dot,
        box_.horizontal,
    ));

    // ── Sections ───────────────────────────────────────────────────────
    let normalised: Vec<Section> = if sections.is_empty() {
        vec![Section { label: None, lines: Vec::new() }]
    } else {
        sections
    };

    let last_idx = normalised.len().saturating_sub(1);
    for (i, section) in normalised.into_iter().enumerate() {
        // Section separator bar (skip for the first section — its header already
        // sits at the top).
        if i > 0 {
            out.push(bar_line(
                box_.tee_right,
                box_.tee_left,
                section.label.as_deref(),
                None,
                outer_width,
                border_style,
                symbols.dot,
                box_.horizontal,
            ));
        }

        for line in section.lines {
            out.push(wrap_content_line(line, inner_width, outer_width, border_style, box_.vertical));
        }

        let _ = i; // silence unused for debug builds
        let _ = last_idx;
    }

    // ── Bottom cap ─────────────────────────────────────────────────────
    out.push(bar_line(
        box_.bottom_left,
        box_.bottom_right,
        None,
        None,
        outer_width,
        border_style,
        symbols.dot,
        box_.horizontal,
    ));

    out
}

/// Render a single bar (`┌──[ label · meta ]──┐` / `├──[ label ]──┤` / `└──┘`).
fn bar_line(
    left_corner: char,
    right_corner: char,
    label: Option<&str>,
    meta: Option<&str>,
    width: usize,
    style: Style,
    dot: char,
    horizontal: char,
) -> Line<'static> {
    // Left side: corner + "──[ " prefix when there's a label, else just "──".
    let cap = format!("{}{}", left_corner, repeat_char(horizontal, 3));
    let label_text: Option<String> = match (label, meta) {
        (Some(l), Some(m)) if !l.is_empty() && !m.is_empty() => Some(format!(" {} {} {} ", l, dot, m)),
        (Some(l), _) if !l.is_empty() => Some(format!(" {} ", l)),
        (_, Some(m)) if !m.is_empty() => Some(format!(" {} ", m)),
        _ => None,
    };
    let right_cap = String::from(right_corner);

    let left_w = cap.chars().count();
    let right_w = right_cap.chars().count();
    let label_w = label_text.as_deref().map(UnicodeWidthStr::width).unwrap_or(0);
    let fill_w = width.saturating_sub(left_w + label_w + right_w);
    let fill: String = std::iter::repeat(horizontal).take(fill_w).collect();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(cap, style));
    if let Some(l) = label_text {
        spans.push(Span::styled(l, style.add_modifier(ratatui::style::Modifier::BOLD)));
    }
    spans.push(Span::styled(fill, style));
    spans.push(Span::styled(right_cap, style));
    Line::from(spans)
}

/// Wrap one already-styled inner content line into "│ content │", padded
/// to inner_width.
fn wrap_content_line(
    content: Line<'static>,
    inner_width: usize,
    outer_width: usize,
    border_style: Style,
    vertical: char,
) -> Line<'static> {
    let content_width: usize = content
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(content.spans.len() + 3);
    spans.push(Span::styled(format!("{} ", vertical), border_style));

    if content_width <= inner_width {
        spans.extend(content.spans);
        let pad = inner_width.saturating_sub(content_width);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
    } else {
        // Truncate at inner_width - 1 then append "…" — preserves color where
        // possible by walking spans.
        let mut remaining = inner_width.saturating_sub(1);
        for span in content.spans {
            let w = UnicodeWidthStr::width(span.content.as_ref());
            if w == 0 {
                continue;
            }
            if w <= remaining {
                remaining -= w;
                spans.push(span);
            } else {
                // Truncate this span char-by-char so we land on a char boundary.
                let mut acc = String::new();
                let mut acc_w = 0usize;
                for ch in span.content.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if acc_w + cw > remaining {
                        break;
                    }
                    acc.push(ch);
                    acc_w += cw;
                }
                spans.push(Span::styled(acc, span.style));
                break;
            }
        }
        spans.push(Span::raw("…"));
    }

    let _ = outer_width; // future use: trailing padding before right bar
    spans.push(Span::styled(String::from(vertical), border_style));
    Line::from(spans)
}

fn repeat_char(c: char, n: usize) -> String {
    std::iter::repeat(c).take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen_widgets::symbols::UNICODE;

    fn t() -> Theme { Theme::dark() }

    fn collect_text(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect::<Vec<_>>().join("\n")
    }

    /// Minimum block: no header, no sections, just frame.
    #[test]
    fn empty_block_has_top_and_bottom_only() {
        let block = render_output_block(OutputBlock {
            header: None,
            header_meta: None,
            state: BlockState::Success,
            sections: vec![],
            width: 20,
            symbols: &UNICODE,
            theme: &t(),
        });
        // 2 lines: top bar + bottom cap.
        assert_eq!(block.len(), 2);
        let text = collect_text(&block);
        assert!(text.starts_with('┌'));
        assert!(text.contains('└'));
    }

    /// Header lands inside the top bar with bracket-style framing.
    #[test]
    fn header_is_rendered_in_top_bar() {
        let block = render_output_block(OutputBlock {
            header: Some("bash".into()),
            header_meta: Some("12ms".into()),
            state: BlockState::Success,
            sections: vec![Section { label: None, lines: vec![Line::raw("hello".to_string())] }],
            width: 40,
            symbols: &UNICODE,
            theme: &t(),
        });
        let text = collect_text(&block);
        let first = text.lines().next().unwrap();
        assert!(first.contains("bash"), "header must appear; got: {first}");
        assert!(first.contains("12ms"), "meta must appear; got: {first}");
        // dot separator between header + meta.
        assert!(first.contains('·'));
    }

    /// Sections after the first get an `├──[ label ]──┤` separator.
    #[test]
    fn multi_section_inserts_tee_bar() {
        let block = render_output_block(OutputBlock {
            header: None,
            header_meta: None,
            state: BlockState::Success,
            sections: vec![
                Section { label: None, lines: vec![Line::raw("code".to_string())] },
                Section { label: Some("Output".into()), lines: vec![Line::raw("hi".to_string())] },
            ],
            width: 30,
            symbols: &UNICODE,
            theme: &t(),
        });
        let text = collect_text(&block);
        assert!(text.contains('├'), "tee_right separator expected");
        assert!(text.contains("Output"), "section label must render");
    }

    /// Content lines are padded out to inner width — every row visually fills.
    #[test]
    fn content_line_is_padded_to_full_width() {
        let block = render_output_block(OutputBlock {
            header: None,
            header_meta: None,
            state: BlockState::Pending,
            sections: vec![Section { label: None, lines: vec![Line::raw("a".to_string())] }],
            width: 20,
            symbols: &UNICODE,
            theme: &t(),
        });
        // Content row width should be exactly the requested outer width.
        let content_line = &block[1]; // [0]=top bar, [1]=content
        let total: usize = content_line.spans.iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(total, 20, "content row must be {} wide; got {total}", 20);
    }

    /// Overlong content gets truncated with a `…` suffix.
    #[test]
    fn overlong_content_truncates_with_ellipsis() {
        let block = render_output_block(OutputBlock {
            header: None,
            header_meta: None,
            state: BlockState::Running,
            sections: vec![Section {
                label: None,
                lines: vec![Line::raw("X".repeat(200))]
            }],
            width: 30,
            symbols: &UNICODE,
            theme: &t(),
        });
        let text = collect_text(&block);
        assert!(text.contains('…'), "overlong row must end with ellipsis");
        // No line should exceed the requested width (counted by chars not bytes).
        for line in text.lines() {
            let w = UnicodeWidthStr::width(line);
            assert!(w <= 30, "row exceeds width 30: {w} chars");
        }
    }

    /// Border color tracks block state: error uses theme.error.
    #[test]
    fn border_color_matches_state() {
        let theme = t();
        let block = render_output_block(OutputBlock {
            header: Some("h".into()),
            header_meta: None,
            state: BlockState::Error,
            sections: vec![],
            width: 20,
            symbols: &UNICODE,
            theme: &theme,
        });
        let top = &block[0];
        let first_span = &top.spans[0];
        assert_eq!(first_span.style.fg, Some(theme.error),
            "top corner must use error color when state is Error");
    }

    /// Header-only (no meta) renders without a dot separator.
    #[test]
    fn header_only_omits_dot_separator() {
        let block = render_output_block(OutputBlock {
            header: Some("bash".into()),
            header_meta: None,
            state: BlockState::Success,
            sections: vec![],
            width: 30,
            symbols: &UNICODE,
            theme: &t(),
        });
        let text = collect_text(&block);
        let first = text.lines().next().unwrap();
        assert!(first.contains("bash"));
        // Dot only appears between header + meta; with no meta there should be none.
        assert!(!first.contains('·'),
            "header-only bar must not show '·' separator; got: {first}");
    }

    /// ASCII preset renders cleanly (no Unicode chars).
    #[test]
    fn ascii_preset_renders_ascii_only() {
        use crate::fullscreen_widgets::symbols::ASCII;
        let block = render_output_block(OutputBlock {
            header: Some("tool".into()),
            header_meta: None,
            state: BlockState::Success,
            sections: vec![Section { label: None, lines: vec![Line::raw("ok".to_string())] }],
            width: 20,
            symbols: &ASCII,
            theme: &t(),
        });
        let text = collect_text(&block);
        for ch in text.chars() {
            assert!(ch.is_ascii() || ch == '\n',
                "ASCII preset rendered non-ASCII char {ch:?}");
        }
    }

    /// `…` is char-boundary safe — multibyte content gets truncated without panic.
    #[test]
    fn multibyte_content_truncates_safely() {
        let block = render_output_block(OutputBlock {
            header: None,
            header_meta: None,
            state: BlockState::Pending,
            sections: vec![Section {
                label: None,
                lines: vec![Line::raw("héllo wörld ".repeat(20))]
            }],
            width: 25,
            symbols: &UNICODE,
            theme: &t(),
        });
        // Just must not panic and must include ellipsis somewhere.
        let text = collect_text(&block);
        assert!(text.contains('…'));
    }
}
