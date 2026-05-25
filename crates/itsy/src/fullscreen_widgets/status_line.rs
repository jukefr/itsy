//! Persistent bottom status line — OMP-style colored segments separated by
//! a center-dot. Each segment renders with its own theme color
//! (`statusLineModel`, `statusLinePath`, `statusLineGitClean`, …).
//!
//! The renderer is pure: pass in a [`StatusLine`] data struct, get back a
//! ratatui `Line` ready to drop into a `Paragraph`. Truncation is
//! left-anchored (drop trailing segments first) so the most identifying
//! info (model, path) survives a narrow terminal.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::symbols::SymbolTheme;
use super::theme::Theme;

/// Git working-tree state for the git segment.
/// `None` is the default — no git status if no repo / not configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitState {
    #[default]
    None,
    Clean,
    Dirty,
    Untracked,
}

/// All optional segments the status line can render. `None` / `0` fields
/// are skipped so an empty status line just renders the model name (or
/// nothing at all).
#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: Option<String>,
    pub path: Option<String>,
    pub git: GitState,
    /// Tokens used / window (e.g. `12k/32k`).
    pub context_used: u64,
    pub context_window: u64,
    /// Cost in USD ($0.0123).
    pub cost_usd: f64,
    /// Tool-call / turn counter to right-anchor.
    pub turn_count: u32,
    pub tool_count: u32,
    /// Running spinner frame (`tick` from the render loop). `None` → no spinner.
    pub spinner_tick: Option<usize>,
    pub busy_label: Option<String>,
}

impl StatusLine {
    /// Render the line at the given total `width`. If the assembled segments
    /// exceed `width`, segments are dropped from the RIGHT (least important
    /// first) so the model name + path stay visible.
    pub fn render(&self, width: u16, theme: &Theme, symbols: &SymbolTheme) -> Line<'static> {
        let sep_style = Style::default().fg(theme.sl_sep);
        let dot = format!(" {} ", symbols.dot);

        let segments = self.collect_segments(theme, symbols);
        if segments.is_empty() {
            return Line::raw("");
        }

        // Greedy: keep adding segments while they still fit. Always keep the
        // first (model) segment, even if it has to be truncated.
        let total = width as usize;
        let mut out: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;

        for (i, seg) in segments.iter().enumerate() {
            let seg_w: usize = seg.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
            let sep_w = if i == 0 { 0 } else { UnicodeWidthStr::width(dot.as_str()) };

            if used + sep_w + seg_w > total {
                if i == 0 {
                    // First segment doesn't fit even alone — truncate it.
                    let avail = total.saturating_sub(1);
                    out.extend(truncate_spans(seg.spans.clone(), avail));
                    out.push(Span::raw("…"));
                    used = total;
                }
                break;
            }
            if i > 0 {
                out.push(Span::styled(dot.clone(), sep_style));
                used += sep_w;
            }
            out.extend(seg.spans.clone());
            used += seg_w;
        }

        Line::from(out)
    }

    /// Build the ordered segment list (left → right priority).
    fn collect_segments(&self, theme: &Theme, symbols: &SymbolTheme) -> Vec<Line<'static>> {
        let mut segs: Vec<Line<'static>> = Vec::new();

        // 1. Spinner + busy label (most ephemeral but most "alive" indicator).
        if let Some(tick) = self.spinner_tick {
            let frame = symbols.spinner_frame(tick);
            let label = self.busy_label.clone().unwrap_or_else(|| String::from("working"));
            segs.push(Line::from(vec![
                Span::styled(format!("{} ", frame), Style::default().fg(theme.accent)),
                Span::styled(label, Style::default().fg(theme.muted)),
            ]));
        }

        // 2. Model name.
        if let Some(model) = &self.model {
            segs.push(Line::from(vec![Span::styled(
                model.clone(),
                Style::default().fg(theme.sl_model),
            )]));
        }

        // 3. Path (display only).
        if let Some(path) = &self.path {
            segs.push(Line::from(vec![Span::styled(
                path.clone(),
                Style::default().fg(theme.sl_path),
            )]));
        }

        // 4. Git state badge.
        match self.git {
            GitState::Clean => segs.push(badge(symbols.icons.success, theme.sl_git_clean)),
            GitState::Dirty => segs.push(badge("✱", theme.sl_git_dirty)),
            GitState::Untracked => segs.push(badge("?", theme.sl_untracked)),
            GitState::None => {}
        }

        // 5. Context usage (12k/32k).
        if self.context_used > 0 || self.context_window > 0 {
            let used = humanize_tokens(self.context_used);
            let win = humanize_tokens(self.context_window);
            let s = if self.context_window > 0 {
                format!("{used}/{win}")
            } else {
                used
            };
            segs.push(Line::from(vec![Span::styled(s, Style::default().fg(theme.sl_context))]));
        }

        // 6. Cost (right-most heavy info).
        if self.cost_usd > 0.0 {
            let s = format!("${:.3}", self.cost_usd);
            segs.push(Line::from(vec![Span::styled(s, Style::default().fg(theme.sl_cost))]));
        }

        // 7. Turn / tool counters (least important).
        if self.turn_count > 0 || self.tool_count > 0 {
            let s = format!("{}t/{}c", self.turn_count, self.tool_count);
            segs.push(Line::from(vec![Span::styled(s, Style::default().fg(theme.sl_subagents))]));
        }

        segs
    }
}

fn badge(text: &str, color: Color) -> Line<'static> {
    Line::from(vec![Span::styled(text.to_string(), Style::default().fg(color))])
}

fn humanize_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Trim a styled span list to a target display width (char-boundary safe).
fn truncate_spans(spans: Vec<Span<'static>>, target: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut remaining = target;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if w <= remaining {
            remaining -= w;
            out.push(span);
        } else {
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
            out.push(Span::styled(acc, span.style));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen_widgets::symbols::UNICODE;

    fn t() -> Theme { Theme::dark() }
    fn line_text(l: &Line<'static>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    /// Empty status returns an empty line — anti-regression for a stray `·` dot.
    #[test]
    fn empty_status_is_empty_line() {
        let sl = StatusLine::default();
        let line = sl.render(80, &t(), &UNICODE);
        assert!(line.spans.is_empty() || line_text(&line).is_empty());
    }

    /// Model + path render as two dot-separated segments.
    #[test]
    fn model_and_path_dot_separated() {
        let sl = StatusLine {
            model: Some("qwen3".into()),
            path: Some("~/itsy".into()),
            ..Default::default()
        };
        let s = line_text(&sl.render(80, &t(), &UNICODE));
        assert!(s.contains("qwen3"));
        assert!(s.contains("~/itsy"));
        assert!(s.contains('·'), "must include dot separator; got {s:?}");
    }

    /// Spinner appears LEFT-most (most "alive" indicator) and uses busy_label.
    #[test]
    fn spinner_appears_first_and_uses_label() {
        let sl = StatusLine {
            model: Some("m".into()),
            spinner_tick: Some(0),
            busy_label: Some("running tool".into()),
            ..Default::default()
        };
        let s = line_text(&sl.render(80, &t(), &UNICODE));
        assert!(s.contains("running tool"));
        // Model name comes AFTER the spinner segment.
        let m_idx = s.find('m').unwrap();
        let label_idx = s.find("running tool").unwrap();
        assert!(label_idx < m_idx, "busy label must precede model; got: {s}");
    }

    /// Cost segment uses `$0.123` format.
    #[test]
    fn cost_renders_as_dollar_amount() {
        let sl = StatusLine { model: Some("m".into()), cost_usd: 0.1234, ..Default::default() };
        let s = line_text(&sl.render(80, &t(), &UNICODE));
        assert!(s.contains("$0.123"), "expected $0.123 in output; got {s}");
    }

    /// Token humanizer: 1234 → 1.2k, 24000 → 24k, 1500000 → 1.5m.
    #[test]
    fn token_humanizer_thresholds() {
        assert_eq!(humanize_tokens(500), "500");
        assert_eq!(humanize_tokens(1500), "1.5k");
        assert_eq!(humanize_tokens(24_000), "24k");
        assert_eq!(humanize_tokens(1_500_000), "1.5m");
    }

    /// Context segment uses used/window when both present.
    #[test]
    fn context_segment_used_over_window() {
        let sl = StatusLine {
            model: Some("m".into()),
            context_used: 12_000,
            context_window: 32_000,
            ..Default::default()
        };
        let s = line_text(&sl.render(80, &t(), &UNICODE));
        assert!(s.contains("12k/32k"), "expected '12k/32k'; got {s}");
    }

    /// Git clean state renders the success icon in success color.
    #[test]
    fn git_clean_renders_success_icon() {
        let sl = StatusLine { model: Some("m".into()), git: GitState::Clean, ..Default::default() };
        let line = sl.render(80, &t(), &UNICODE);
        let s = line_text(&line);
        assert!(s.contains('✓') || s.contains("[+]"), "expected git-clean icon; got {s}");
    }

    /// When width is tight, less-important segments are dropped from the right.
    #[test]
    fn narrow_width_drops_right_segments_first() {
        let sl = StatusLine {
            model: Some("verylongmodelname".into()),
            path: Some("/some/very/long/working/path".into()),
            context_used: 12000,
            context_window: 32000,
            cost_usd: 5.0,
            turn_count: 12,
            tool_count: 34,
            ..Default::default()
        };
        // Wide enough for everything: cost should appear.
        let wide = line_text(&sl.render(120, &t(), &UNICODE));
        assert!(wide.contains("$5"));
        // Narrow: cost & turn counters drop, model survives.
        let narrow = line_text(&sl.render(25, &t(), &UNICODE));
        assert!(narrow.contains("verylongmodelname")
                || narrow.contains("…"),
            "model must survive narrow width (truncated OK); got: {narrow}");
        assert!(!narrow.contains("$5"), "cost must be dropped at narrow width; got: {narrow}");
    }

    /// Render output never exceeds the requested width.
    #[test]
    fn render_respects_max_width() {
        let sl = StatusLine {
            model: Some("m".repeat(10).into()),
            path: Some("p".repeat(10).into()),
            cost_usd: 0.123,
            ..Default::default()
        };
        for w in [15u16, 30, 60, 80] {
            let line = sl.render(w, &t(), &UNICODE);
            let used: usize = line.spans.iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(used <= w as usize, "width {w}: rendered {used} cols");
        }
    }

    /// When the very first segment doesn't even fit, it's truncated with `…`.
    #[test]
    fn first_segment_truncates_with_ellipsis_on_tiny_width() {
        let sl = StatusLine { model: Some("verylongname".into()), ..Default::default() };
        let s = line_text(&sl.render(5, &t(), &UNICODE));
        assert!(s.ends_with('…') || s.is_empty(), "expected ellipsis or empty; got: {s:?}");
    }
}
