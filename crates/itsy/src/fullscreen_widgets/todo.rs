//! Plan / TODO tracker widget — a compact list shown above the status line.
//!
//! Sits between the chat area and the input. Renders a single header line
//! (collapsed) or the full item list (expanded). Each item shows a state
//! icon, its label, and a faint per-item meta (duration / count) when set.
//!
//! State semantics mirror what an agent plan-mode would expose:
//! - `Pending`   ☐  — not started
//! - `InProgress` ▶ — actively working
//! - `Complete`  ✓ — finished
//! - `Blocked`   ⊘ — failed / waiting on something external
//! - `Cancelled` ✗ — explicitly skipped

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::symbols::SymbolTheme;
use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TodoState {
    #[default]
    Pending,
    InProgress,
    Complete,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, Default)]
pub struct TodoItem {
    pub label: String,
    pub state: TodoState,
    /// Optional dim-rendered meta (e.g. "12 files", "3.2s").
    pub meta: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TodoWidget {
    pub items: Vec<TodoItem>,
    /// Title rendered into the first line (e.g. "Plan", "TODO").
    /// Empty → just shows the count.
    pub title: String,
    pub expanded: bool,
}

impl TodoWidget {
    pub fn new(title: impl Into<String>) -> Self {
        Self { items: Vec::new(), title: title.into(), expanded: true }
    }

    pub fn push(&mut self, label: impl Into<String>, state: TodoState) {
        self.items.push(TodoItem { label: label.into(), state, meta: None });
    }

    /// Render the widget. When collapsed, just one header line:
    /// `▶ Plan · 2/5 complete`. When expanded, header + one line per item.
    pub fn render(&self, theme: &Theme, symbols: &SymbolTheme) -> Vec<Line<'static>> {
        if self.items.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(self.header_line(theme, symbols));
        if self.expanded {
            for item in &self.items {
                out.push(item_line(item, theme, symbols));
            }
        }
        out
    }

    fn header_line(&self, theme: &Theme, symbols: &SymbolTheme) -> Line<'static> {
        let complete = self.items.iter().filter(|i| i.state == TodoState::Complete).count();
        let in_progress = self.items.iter().filter(|i| i.state == TodoState::InProgress).count();
        let total = self.items.len();

        let icon = if in_progress > 0 {
            (symbols.icons.running, theme.accent)
        } else if complete == total {
            (symbols.icons.success, theme.success)
        } else {
            (symbols.icons.pending, theme.muted)
        };

        let title = if self.title.is_empty() { "Plan" } else { self.title.as_str() };
        let mut spans = vec![
            Span::styled(format!("{} ", icon.0), Style::default().fg(icon.1)),
            Span::styled(
                title.to_string(),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {} ", symbols.dot), Style::default().fg(theme.sl_sep)),
            Span::styled(
                format!("{}/{} complete", complete, total),
                Style::default().fg(theme.muted),
            ),
        ];
        if in_progress > 0 {
            spans.push(Span::styled(
                format!(" {} {} in progress", symbols.dot, in_progress),
                Style::default().fg(theme.warning),
            ));
        }
        Line::from(spans)
    }
}

fn item_line(item: &TodoItem, theme: &Theme, symbols: &SymbolTheme) -> Line<'static> {
    let (icon, color) = match item.state {
        TodoState::Pending => ("☐", theme.muted),
        TodoState::InProgress => (symbols.icons.running, theme.accent),
        TodoState::Complete => (symbols.icons.success, theme.success),
        TodoState::Blocked => ("⊘", theme.error),
        TodoState::Cancelled => (symbols.icons.error, theme.dim),
    };

    let label_style = match item.state {
        TodoState::Complete | TodoState::Cancelled => {
            Style::default().fg(theme.muted).add_modifier(Modifier::CROSSED_OUT)
        }
        TodoState::InProgress => Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(theme.text),
    };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(item.label.clone(), label_style),
    ];
    if let Some(meta) = &item.meta {
        spans.push(Span::styled(
            format!("  ({})", meta),
            Style::default().fg(theme.dim),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen_widgets::symbols::UNICODE;

    fn t() -> Theme { Theme::dark() }

    fn text(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect::<Vec<_>>().join("\n")
    }

    /// Empty TodoWidget renders zero lines.
    #[test]
    fn empty_widget_renders_nothing() {
        let w = TodoWidget::new("Plan");
        assert!(w.render(&t(), &UNICODE).is_empty());
    }

    /// Header line shows count + title.
    #[test]
    fn header_shows_title_and_count() {
        let mut w = TodoWidget::new("Plan");
        w.push("a", TodoState::Pending);
        w.push("b", TodoState::Complete);
        let s = text(&w.render(&t(), &UNICODE));
        assert!(s.contains("Plan"));
        assert!(s.contains("1/2 complete"), "expected '1/2 complete'; got {s:?}");
    }

    /// Empty title falls back to "Plan".
    #[test]
    fn empty_title_falls_back_to_plan() {
        let mut w = TodoWidget::new("");
        w.push("a", TodoState::Pending);
        let s = text(&w.render(&t(), &UNICODE));
        assert!(s.contains("Plan"));
    }

    /// In-progress count surfaces in the header.
    #[test]
    fn in_progress_count_surfaces() {
        let mut w = TodoWidget::new("Plan");
        w.push("a", TodoState::InProgress);
        w.push("b", TodoState::InProgress);
        w.push("c", TodoState::Pending);
        let s = text(&w.render(&t(), &UNICODE));
        assert!(s.contains("2 in progress"), "expected in-progress count; got: {s}");
    }

    /// All-complete: success icon + success-color header icon.
    #[test]
    fn all_complete_uses_success_icon() {
        let mut w = TodoWidget::new("Plan");
        w.push("a", TodoState::Complete);
        w.push("b", TodoState::Complete);
        let lines = w.render(&t(), &UNICODE);
        let head_first_span = &lines[0].spans[0];
        // Icon span is the success glyph.
        assert!(head_first_span.content.contains('✓') || head_first_span.content.contains("[+]"),
            "all-complete must show success icon; got: {:?}", head_first_span.content);
    }

    /// Expanded shows N+1 lines (header + N items).
    #[test]
    fn expanded_shows_one_line_per_item() {
        let mut w = TodoWidget::new("Plan");
        w.push("a", TodoState::Pending);
        w.push("b", TodoState::InProgress);
        w.push("c", TodoState::Complete);
        w.expanded = true;
        let lines = w.render(&t(), &UNICODE);
        assert_eq!(lines.len(), 4);
        let s = text(&lines);
        assert!(s.contains("a"));
        assert!(s.contains("b"));
        assert!(s.contains("c"));
    }

    /// Collapsed shows ONLY the header.
    #[test]
    fn collapsed_shows_only_header() {
        let mut w = TodoWidget::new("Plan");
        w.push("a", TodoState::Pending);
        w.push("b", TodoState::InProgress);
        w.expanded = false;
        let lines = w.render(&t(), &UNICODE);
        assert_eq!(lines.len(), 1, "collapsed should be one header line");
    }

    /// Completed items render with strikethrough modifier.
    /// Anti-regression: an in-progress label must NOT be strikethrough.
    #[test]
    fn completed_items_get_strikethrough() {
        let mut w = TodoWidget::new("Plan");
        w.push("done", TodoState::Complete);
        w.push("doing", TodoState::InProgress);
        let lines = w.render(&t(), &UNICODE);
        // Find the "done" line.
        let done_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("done"))
        }).unwrap();
        let done_label = done_line.spans.iter().find(|s| s.content.contains("done")).unwrap();
        assert!(done_label.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "completed label must be strikethrough");

        let doing_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("doing"))
        }).unwrap();
        let doing_label = doing_line.spans.iter().find(|s| s.content.contains("doing")).unwrap();
        assert!(!doing_label.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "in-progress label must NOT be strikethrough");
    }

    /// Item meta renders in dim parens after the label.
    #[test]
    fn item_meta_renders_after_label() {
        let mut w = TodoWidget::new("Plan");
        w.items.push(TodoItem {
            label: "build".into(),
            state: TodoState::Complete,
            meta: Some("3.2s".into()),
        });
        let s = text(&w.render(&t(), &UNICODE));
        assert!(s.contains("build"));
        assert!(s.contains("(3.2s)"), "meta must appear in parens; got: {s}");
    }

    /// Blocked state uses error color + ⊘ icon.
    #[test]
    fn blocked_state_uses_error_icon() {
        let mut w = TodoWidget::new("Plan");
        w.push("stuck", TodoState::Blocked);
        let lines = w.render(&t(), &UNICODE);
        let s = text(&lines);
        assert!(s.contains('⊘'), "blocked must use ⊘ icon; got: {s}");
    }
}
