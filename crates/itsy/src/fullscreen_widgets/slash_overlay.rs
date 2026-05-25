//! Slash command overlay — bottom-anchored selectable list that fuzzy-filters
//! as you type. Renders inside an [`crate::fullscreen_widgets::output_block`].
//!
//! Fuzzy matching is a simple subsequence scorer:
//!   - case-insensitive subsequence match required
//!   - **contiguous** matches score higher than scattered
//!   - **left-anchored** matches (prefix) get a bonus
//!   - matched character indices are returned so the renderer can highlight
//!     them
//!
//! That's enough fidelity for command palettes; this isn't `fzf`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::output_block::{render_output_block, OutputBlock, Section};
use super::symbols::SymbolTheme;
use super::theme::{BlockState, Theme};

/// One selectable row in the overlay.
#[derive(Debug, Clone)]
pub struct SlashItem {
    /// Command name (including the leading `/`).
    pub name: String,
    /// One-line description shown dim after the name.
    pub description: String,
    /// Optional alias displayed as a chip (`/quit /q`).
    pub alias: Option<String>,
}

/// A fuzzy-matched item plus the highlight positions for the `name`.
#[derive(Debug, Clone)]
pub struct Match {
    pub item: SlashItem,
    pub score: i32,
    /// Byte offsets into `item.name` that were matched (used for bold highlight).
    pub matched: Vec<usize>,
}

/// Filter a list of items against a needle, returning best-first matches.
/// Empty needle returns all items in original order with `score=0`, `matched=[]`.
pub fn filter(items: &[SlashItem], needle: &str) -> Vec<Match> {
    if needle.is_empty() {
        return items.iter()
            .map(|i| Match { item: i.clone(), score: 0, matched: Vec::new() })
            .collect();
    }
    let needle = needle.trim_start_matches('/').to_lowercase();
    if needle.is_empty() {
        return items.iter()
            .map(|i| Match { item: i.clone(), score: 0, matched: Vec::new() })
            .collect();
    }

    let mut out: Vec<Match> = items.iter()
        .filter_map(|item| {
            // Try matching against the bare command (no leading /).
            let hay = item.name.trim_start_matches('/');
            score_match(hay, &needle).map(|(score, indices)| {
                // Re-anchor indices to include the leading '/'.
                let offset = item.name.len() - hay.len();
                let matched = indices.into_iter().map(|i| i + offset).collect();
                Match { item: item.clone(), score, matched }
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score)
        .then_with(|| a.item.name.len().cmp(&b.item.name.len())));
    out
}

/// Score one haystack against a (lowercased) needle. Returns
/// `(score, matched byte indices)` or `None` if not a subsequence.
///
/// Heuristic:
///   - +10 per matched char
///   - +5 per contiguous run beyond the first char
///   - +20 if the first match is at index 0 (prefix bonus)
///   - -1 per skipped char between matches
fn score_match(hay: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    let hay_lower = hay.to_lowercase();
    let mut h_iter = hay_lower.char_indices();
    let mut indices: Vec<usize> = Vec::with_capacity(needle.chars().count());
    let mut score: i32 = 0;
    let mut last_idx: Option<usize> = None;
    let mut first_match: Option<usize> = None;

    for n_ch in needle.chars() {
        // Find next char in hay.
        let mut found = None;
        for (i, h_ch) in h_iter.by_ref() {
            if h_ch == n_ch {
                found = Some(i);
                break;
            }
        }
        match found {
            Some(i) => {
                if first_match.is_none() { first_match = Some(i); }
                score += 10;
                if let Some(last) = last_idx {
                    let gap = i.saturating_sub(last).saturating_sub(1);
                    if gap == 0 {
                        score += 5; // contiguous
                    } else {
                        score -= gap as i32;
                    }
                }
                last_idx = Some(i);
                indices.push(i);
            }
            None => return None,
        }
    }

    if first_match == Some(0) {
        score += 20;
    }

    Some((score, indices))
}

/// Render the overlay into a list of `Line<'static>` ready for ratatui.
/// Wraps everything in an `output_block` so it looks like a small modal.
pub fn render_overlay(
    matches: &[Match],
    selected: usize,
    width: u16,
    max_visible: usize,
    theme: &Theme,
    symbols: &SymbolTheme,
) -> Vec<Line<'static>> {
    let lines = if matches.is_empty() {
        vec![Line::from(vec![
            Span::styled("  no matches", Style::default().fg(theme.muted)),
        ])]
    } else {
        let visible = matches.iter().take(max_visible.max(1));
        visible.enumerate().map(|(i, m)| {
            render_item(m, i == selected, theme)
        }).collect()
    };

    let header = if matches.is_empty() {
        Some(String::from("commands"))
    } else {
        Some(format!("commands · {} match", matches.len()))
    };

    render_output_block(OutputBlock {
        header,
        header_meta: None,
        state: BlockState::Pending,
        sections: vec![Section { label: None, lines }],
        width,
        symbols,
        theme,
    })
}

fn render_item(m: &Match, is_selected: bool, theme: &Theme) -> Line<'static> {
    let cursor = if is_selected { "▶ " } else { "  " };
    let cursor_style = if is_selected {
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim)
    };

    let name_style = if is_selected {
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let highlight_style = Style::default().fg(theme.warning).add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(cursor.to_string(), cursor_style)];

    // Highlight matched chars inside name.
    let name = &m.item.name;
    let highlight: std::collections::HashSet<usize> = m.matched.iter().copied().collect();
    let mut buf = String::new();
    let mut buf_highlighted = false;
    for (i, ch) in name.char_indices() {
        let now_highlight = highlight.contains(&i);
        if buf_highlighted != now_highlight && !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_highlighted { highlight_style } else { name_style },
            ));
        }
        buf.push(ch);
        buf_highlighted = now_highlight;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(
            buf,
            if buf_highlighted { highlight_style } else { name_style },
        ));
    }

    if let Some(alias) = &m.item.alias {
        spans.push(Span::styled(
            format!("  ({})", alias),
            Style::default().fg(theme.dim),
        ));
    }

    if !m.item.description.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            m.item.description.clone(),
            Style::default().fg(theme.muted),
        ));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen_widgets::symbols::UNICODE;

    fn t() -> Theme { Theme::dark() }

    fn items() -> Vec<SlashItem> {
        vec![
            SlashItem { name: "/quit".into(), description: "Exit".into(), alias: Some("/q".into()) },
            SlashItem { name: "/help".into(), description: "Help".into(), alias: None },
            SlashItem { name: "/model".into(), description: "Show model".into(), alias: None },
            SlashItem { name: "/memory".into(), description: "Memory".into(), alias: None },
        ]
    }

    /// Empty needle → all items in original order, all score=0.
    #[test]
    fn empty_needle_returns_all_unranked() {
        let r = filter(&items(), "");
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|m| m.score == 0));
        assert_eq!(r[0].item.name, "/quit");
    }

    /// Just "/" also returns all (no fuzzy needle yet).
    #[test]
    fn just_slash_returns_all() {
        let r = filter(&items(), "/");
        assert_eq!(r.len(), 4);
    }

    /// Prefix match scores higher than mid-word.
    #[test]
    fn prefix_beats_midword() {
        let r = filter(&items(), "mod");
        // Both /model and /memory contain 'm' but only /model has "mod" as subseq.
        // After scoring, /model should rank higher than /memory.
        let names: Vec<&str> = r.iter().map(|m| m.item.name.as_str()).collect();
        let model_pos = names.iter().position(|s| *s == "/model").unwrap();
        // Either /memory drops out or ranks lower.
        if let Some(mem_pos) = names.iter().position(|s| *s == "/memory") {
            assert!(model_pos < mem_pos, "/model must rank before /memory; got {names:?}");
        }
    }

    /// Contiguous-match heuristic: "hel" matches "/help" with score > 35
    /// (10*3 + 5*2 contiguous + 20 prefix = 60 — leading '/' adjustment, etc.)
    #[test]
    fn contiguous_match_scores_high() {
        let r = filter(&items(), "hel");
        let m = r.iter().find(|m| m.item.name == "/help").expect("/help matches");
        assert!(m.score > 35, "contiguous prefix match must score high; got {}", m.score);
    }

    /// Non-subsequence returns empty.
    #[test]
    fn non_match_filtered_out() {
        let r = filter(&items(), "zxq");
        assert!(r.is_empty());
    }

    /// Case-insensitive matching.
    #[test]
    fn matching_is_case_insensitive() {
        let r = filter(&items(), "HELP");
        assert!(r.iter().any(|m| m.item.name == "/help"));
    }

    /// Matched indices are returned for renderer highlighting.
    #[test]
    fn matched_indices_returned() {
        let r = filter(&items(), "q");
        let m = r.iter().find(|m| m.item.name == "/quit").unwrap();
        // /q  → 'q' at byte index 1 (the '/' is at 0).
        assert_eq!(m.matched, vec![1]);
    }

    /// Render produces a non-empty block with the header counter.
    #[test]
    fn overlay_renders_with_header_count() {
        let r = filter(&items(), "m");
        let lines = render_overlay(&r, 0, 60, 10, &t(), &UNICODE);
        let text: String = lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect::<Vec<_>>().join("\n");
        assert!(text.contains("commands"));
        assert!(text.contains("match"));
        assert!(text.contains("/model") || text.contains("/memory"));
    }

    /// Empty matches → "no matches" message in body.
    #[test]
    fn no_matches_shows_helpful_message() {
        let lines = render_overlay(&[], 0, 60, 10, &t(), &UNICODE);
        let text: String = lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect::<Vec<_>>().join("\n");
        assert!(text.contains("no matches"));
    }

    /// Selected item gets the ▶ cursor + accent color.
    #[test]
    fn selected_item_has_cursor() {
        let r = filter(&items(), "");
        let lines = render_overlay(&r, 1, 60, 10, &t(), &UNICODE);
        // Lines: 0=top bar, 1=item0, 2=item1 (selected), 3=item2, 4=item3, 5=bottom.
        // The output_block adds a "│ " border prefix as spans[0]; the cursor
        // emitted by render_item lands in spans[1].
        let row = &lines[2];
        let cursor_span = row.spans.iter().find(|s| {
            s.content.contains('▶') || s.content.contains("> ")
        });
        assert!(cursor_span.is_some(),
            "selected row must include cursor marker; spans: {:?}",
            row.spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>());
    }

    /// `max_visible` clamps the list shown — anti-regression: must not exceed
    /// the requested visible count.
    #[test]
    fn max_visible_clamps_list() {
        let r = filter(&items(), "");
        let lines = render_overlay(&r, 0, 60, 2, &t(), &UNICODE);
        // top bar + 2 items + bottom cap = 4 lines.
        assert_eq!(lines.len(), 4, "got: {} lines", lines.len());
    }

    /// Renderer never panics with selected index out of range.
    #[test]
    fn out_of_range_selected_does_not_panic() {
        let r = filter(&items(), "");
        let _ = render_overlay(&r, 99, 60, 10, &t(), &UNICODE);
    }

    /// Alias renders in dim parens after the name.
    #[test]
    fn alias_renders_in_dim_parens() {
        let r = filter(&items(), "quit");
        let lines = render_overlay(&r, 0, 60, 10, &t(), &UNICODE);
        let text: String = lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect::<Vec<_>>().join("\n");
        assert!(text.contains("(/q)"), "alias must render in parens; got: {text}");
    }
}
