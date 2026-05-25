//! OMP-style theme: a rich color-token palette plus presets.
//!
//! Mirrors the token set documented in
//! [`oh-my-pi/oh-my-pi`](https://github.com/can1357/oh-my-pi/blob/main/docs/theme.md)
//! — core text/borders, message backgrounds, markdown styling, syntax
//! highlighting, tool-diff colors, and status-line segments. Each token is
//! a ratatui `Color` so widgets can reach in and paint with `Style::default().fg(theme.accent)`.
//!
//! Three presets ship: `dark` (default), `light`, `minimal` (single-foreground
//! grayscale). Users can pick via `ITSY_THEME=light` (etc.) — that lookup
//! lives in `crate::fullscreen::Theme::from_env`. This module is the data only.

use ratatui::style::Color;

/// Rich color palette. Every widget pulls its colors from this struct so a
/// theme swap is one assignment.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // ── Core text + borders ────────────────────────────────────────────
    pub accent: Color,
    pub border: Color,
    pub border_accent: Color,
    pub border_muted: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub muted: Color,
    pub dim: Color,
    pub text: Color,
    pub thinking_text: Color,

    // ── Backgrounds (for chips / message blocks) ───────────────────────
    pub bg: Color,
    pub selected_bg: Color,
    pub user_message_bg: Color,
    pub custom_message_bg: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    pub status_line_bg: Color,

    // ── Message / tool text ─────────────────────────────────────────────
    pub user_message_text: Color,
    pub custom_message_text: Color,
    pub custom_message_label: Color,
    pub tool_title: Color,
    pub tool_output: Color,

    // ── Markdown ────────────────────────────────────────────────────────
    pub md_heading: Color,
    pub md_link: Color,
    pub md_link_url: Color,
    pub md_code: Color,
    pub md_code_block: Color,
    pub md_code_block_border: Color,
    pub md_quote: Color,
    pub md_quote_border: Color,
    pub md_hr: Color,
    pub md_list_bullet: Color,

    // ── Tool diff + syntax highlighting ─────────────────────────────────
    pub tool_diff_added: Color,
    pub tool_diff_removed: Color,
    pub tool_diff_context: Color,
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,

    // ── Status-line segments ────────────────────────────────────────────
    pub sl_sep: Color,
    pub sl_model: Color,
    pub sl_path: Color,
    pub sl_git_clean: Color,
    pub sl_git_dirty: Color,
    pub sl_context: Color,
    pub sl_spend: Color,
    pub sl_staged: Color,
    pub sl_dirty: Color,
    pub sl_untracked: Color,
    pub sl_output: Color,
    pub sl_cost: Color,
    pub sl_subagents: Color,
}

impl Theme {
    /// The shipped default — dark background, OMP-leaning accent palette.
    pub fn dark() -> Self {
        Self {
            accent: Color::Rgb(0x88, 0xc0, 0xd0),
            border: Color::Rgb(0x4c, 0x56, 0x6a),
            border_accent: Color::Rgb(0x88, 0xc0, 0xd0),
            border_muted: Color::Rgb(0x3b, 0x42, 0x52),
            success: Color::Rgb(0xa3, 0xbe, 0x8c),
            error: Color::Rgb(0xbf, 0x61, 0x6a),
            warning: Color::Rgb(0xeb, 0xcb, 0x8b),
            muted: Color::Rgb(0x81, 0x8b, 0xa0),
            dim: Color::Rgb(0x4c, 0x56, 0x6a),
            text: Color::Rgb(0xd8, 0xde, 0xe9),
            thinking_text: Color::Rgb(0x81, 0x8b, 0xa0),

            bg: Color::Rgb(0x2e, 0x34, 0x40),
            selected_bg: Color::Rgb(0x3b, 0x42, 0x52),
            user_message_bg: Color::Rgb(0x3b, 0x42, 0x52),
            custom_message_bg: Color::Rgb(0x43, 0x4c, 0x5e),
            tool_pending_bg: Color::Rgb(0x3b, 0x42, 0x52),
            tool_success_bg: Color::Rgb(0x36, 0x40, 0x3a),
            tool_error_bg: Color::Rgb(0x4a, 0x35, 0x39),
            status_line_bg: Color::Rgb(0x3b, 0x42, 0x52),

            user_message_text: Color::Rgb(0xe5, 0xe9, 0xf0),
            custom_message_text: Color::Rgb(0xe5, 0xe9, 0xf0),
            custom_message_label: Color::Rgb(0xb4, 0x8e, 0xad),
            tool_title: Color::Rgb(0x88, 0xc0, 0xd0),
            tool_output: Color::Rgb(0xd8, 0xde, 0xe9),

            md_heading: Color::Rgb(0xeb, 0xcb, 0x8b),
            md_link: Color::Rgb(0x88, 0xc0, 0xd0),
            md_link_url: Color::Rgb(0x81, 0xa1, 0xc1),
            md_code: Color::Rgb(0xb4, 0x8e, 0xad),
            md_code_block: Color::Rgb(0xd8, 0xde, 0xe9),
            md_code_block_border: Color::Rgb(0x4c, 0x56, 0x6a),
            md_quote: Color::Rgb(0xd8, 0xde, 0xe9),
            md_quote_border: Color::Rgb(0x88, 0xc0, 0xd0),
            md_hr: Color::Rgb(0x4c, 0x56, 0x6a),
            md_list_bullet: Color::Rgb(0x88, 0xc0, 0xd0),

            tool_diff_added: Color::Rgb(0xa3, 0xbe, 0x8c),
            tool_diff_removed: Color::Rgb(0xbf, 0x61, 0x6a),
            tool_diff_context: Color::Rgb(0x81, 0x8b, 0xa0),
            syntax_comment: Color::Rgb(0x81, 0x8b, 0xa0),
            syntax_keyword: Color::Rgb(0x81, 0xa1, 0xc1),
            syntax_function: Color::Rgb(0x88, 0xc0, 0xd0),
            syntax_variable: Color::Rgb(0xd8, 0xde, 0xe9),
            syntax_string: Color::Rgb(0xa3, 0xbe, 0x8c),
            syntax_number: Color::Rgb(0xb4, 0x8e, 0xad),
            syntax_type: Color::Rgb(0x8f, 0xbc, 0xbb),
            syntax_operator: Color::Rgb(0x81, 0xa1, 0xc1),
            syntax_punctuation: Color::Rgb(0xd8, 0xde, 0xe9),

            sl_sep: Color::Rgb(0x4c, 0x56, 0x6a),
            sl_model: Color::Rgb(0x88, 0xc0, 0xd0),
            sl_path: Color::Rgb(0x81, 0xa1, 0xc1),
            sl_git_clean: Color::Rgb(0xa3, 0xbe, 0x8c),
            sl_git_dirty: Color::Rgb(0xeb, 0xcb, 0x8b),
            sl_context: Color::Rgb(0xb4, 0x8e, 0xad),
            sl_spend: Color::Rgb(0xeb, 0xcb, 0x8b),
            sl_staged: Color::Rgb(0xa3, 0xbe, 0x8c),
            sl_dirty: Color::Rgb(0xeb, 0xcb, 0x8b),
            sl_untracked: Color::Rgb(0x81, 0x8b, 0xa0),
            sl_output: Color::Rgb(0x88, 0xc0, 0xd0),
            sl_cost: Color::Rgb(0xeb, 0xcb, 0x8b),
            sl_subagents: Color::Rgb(0xb4, 0x8e, 0xad),
        }
    }

    /// Inverted preset — light background, darker accents.
    pub fn light() -> Self {
        Self {
            accent: Color::Rgb(0x00, 0x5c, 0x91),
            border: Color::Rgb(0x9b, 0xa3, 0xb0),
            border_accent: Color::Rgb(0x00, 0x5c, 0x91),
            border_muted: Color::Rgb(0xd1, 0xd5, 0xdc),
            success: Color::Rgb(0x4f, 0x7a, 0x28),
            error: Color::Rgb(0xb0, 0x2a, 0x37),
            warning: Color::Rgb(0xa8, 0x6c, 0x0e),
            muted: Color::Rgb(0x68, 0x6f, 0x7d),
            dim: Color::Rgb(0xa1, 0xa8, 0xb3),
            text: Color::Rgb(0x1b, 0x1e, 0x25),
            thinking_text: Color::Rgb(0x68, 0x6f, 0x7d),

            bg: Color::Rgb(0xfa, 0xfa, 0xfa),
            selected_bg: Color::Rgb(0xe5, 0xe9, 0xf0),
            user_message_bg: Color::Rgb(0xe8, 0xee, 0xf6),
            custom_message_bg: Color::Rgb(0xf3, 0xed, 0xfb),
            tool_pending_bg: Color::Rgb(0xee, 0xf2, 0xf7),
            tool_success_bg: Color::Rgb(0xe9, 0xf4, 0xe4),
            tool_error_bg: Color::Rgb(0xfa, 0xe6, 0xe8),
            status_line_bg: Color::Rgb(0xe5, 0xe9, 0xf0),

            user_message_text: Color::Rgb(0x1b, 0x1e, 0x25),
            custom_message_text: Color::Rgb(0x1b, 0x1e, 0x25),
            custom_message_label: Color::Rgb(0x70, 0x3d, 0x6a),
            tool_title: Color::Rgb(0x00, 0x5c, 0x91),
            tool_output: Color::Rgb(0x1b, 0x1e, 0x25),

            md_heading: Color::Rgb(0xa8, 0x6c, 0x0e),
            md_link: Color::Rgb(0x00, 0x5c, 0x91),
            md_link_url: Color::Rgb(0x2d, 0x5f, 0x9e),
            md_code: Color::Rgb(0x70, 0x3d, 0x6a),
            md_code_block: Color::Rgb(0x1b, 0x1e, 0x25),
            md_code_block_border: Color::Rgb(0xc4, 0xcb, 0xd6),
            md_quote: Color::Rgb(0x1b, 0x1e, 0x25),
            md_quote_border: Color::Rgb(0x00, 0x5c, 0x91),
            md_hr: Color::Rgb(0xc4, 0xcb, 0xd6),
            md_list_bullet: Color::Rgb(0x00, 0x5c, 0x91),

            tool_diff_added: Color::Rgb(0x4f, 0x7a, 0x28),
            tool_diff_removed: Color::Rgb(0xb0, 0x2a, 0x37),
            tool_diff_context: Color::Rgb(0x68, 0x6f, 0x7d),
            syntax_comment: Color::Rgb(0x68, 0x6f, 0x7d),
            syntax_keyword: Color::Rgb(0x2d, 0x5f, 0x9e),
            syntax_function: Color::Rgb(0x00, 0x5c, 0x91),
            syntax_variable: Color::Rgb(0x1b, 0x1e, 0x25),
            syntax_string: Color::Rgb(0x4f, 0x7a, 0x28),
            syntax_number: Color::Rgb(0x70, 0x3d, 0x6a),
            syntax_type: Color::Rgb(0x0e, 0x70, 0x6f),
            syntax_operator: Color::Rgb(0x2d, 0x5f, 0x9e),
            syntax_punctuation: Color::Rgb(0x1b, 0x1e, 0x25),

            sl_sep: Color::Rgb(0xa1, 0xa8, 0xb3),
            sl_model: Color::Rgb(0x00, 0x5c, 0x91),
            sl_path: Color::Rgb(0x2d, 0x5f, 0x9e),
            sl_git_clean: Color::Rgb(0x4f, 0x7a, 0x28),
            sl_git_dirty: Color::Rgb(0xa8, 0x6c, 0x0e),
            sl_context: Color::Rgb(0x70, 0x3d, 0x6a),
            sl_spend: Color::Rgb(0xa8, 0x6c, 0x0e),
            sl_staged: Color::Rgb(0x4f, 0x7a, 0x28),
            sl_dirty: Color::Rgb(0xa8, 0x6c, 0x0e),
            sl_untracked: Color::Rgb(0x68, 0x6f, 0x7d),
            sl_output: Color::Rgb(0x00, 0x5c, 0x91),
            sl_cost: Color::Rgb(0xa8, 0x6c, 0x0e),
            sl_subagents: Color::Rgb(0x70, 0x3d, 0x6a),
        }
    }

    /// Monochrome preset — single foreground for terminals without good color.
    pub fn minimal() -> Self {
        // Use Color::Reset to fall back to terminal default, and color a few
        // semantic accents (success/error) so feedback still reads.
        let fg = Color::Reset;
        let muted = Color::DarkGray;
        Self {
            accent: fg,
            border: muted,
            border_accent: fg,
            border_muted: muted,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            muted,
            dim: muted,
            text: fg,
            thinking_text: muted,

            bg: Color::Reset,
            selected_bg: Color::DarkGray,
            user_message_bg: Color::Reset,
            custom_message_bg: Color::Reset,
            tool_pending_bg: Color::Reset,
            tool_success_bg: Color::Reset,
            tool_error_bg: Color::Reset,
            status_line_bg: Color::Reset,

            user_message_text: fg,
            custom_message_text: fg,
            custom_message_label: fg,
            tool_title: fg,
            tool_output: fg,

            md_heading: fg,
            md_link: Color::Blue,
            md_link_url: muted,
            md_code: fg,
            md_code_block: fg,
            md_code_block_border: muted,
            md_quote: fg,
            md_quote_border: muted,
            md_hr: muted,
            md_list_bullet: fg,

            tool_diff_added: Color::Green,
            tool_diff_removed: Color::Red,
            tool_diff_context: muted,
            syntax_comment: muted,
            syntax_keyword: fg,
            syntax_function: fg,
            syntax_variable: fg,
            syntax_string: fg,
            syntax_number: fg,
            syntax_type: fg,
            syntax_operator: fg,
            syntax_punctuation: fg,

            sl_sep: muted,
            sl_model: fg,
            sl_path: fg,
            sl_git_clean: Color::Green,
            sl_git_dirty: Color::Yellow,
            sl_context: fg,
            sl_spend: Color::Yellow,
            sl_staged: Color::Green,
            sl_dirty: Color::Yellow,
            sl_untracked: muted,
            sl_output: fg,
            sl_cost: Color::Yellow,
            sl_subagents: fg,
        }
    }

    /// Pick a theme by name: `"light"`, `"minimal"`, anything else → dark.
    pub fn from_name(name: &str) -> Self {
        match name {
            "light" => Self::light(),
            "minimal" => Self::minimal(),
            _ => Self::dark(),
        }
    }
}

/// Map a tool/status state to its semantic border color in the active theme.
/// Used by `output_block` and `code_cell` to color their frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Running,
    Pending,
    Success,
    Warning,
    Error,
}

impl BlockState {
    pub fn border_color(self, t: &Theme) -> Color {
        match self {
            BlockState::Running | BlockState::Pending => t.accent,
            BlockState::Success => t.dim,
            BlockState::Warning => t.warning,
            BlockState::Error => t.error,
        }
    }

    pub fn bg_color(self, t: &Theme) -> Color {
        match self {
            BlockState::Running | BlockState::Pending => t.tool_pending_bg,
            BlockState::Success => t.tool_success_bg,
            BlockState::Warning => t.warning,
            BlockState::Error => t.tool_error_bg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_known_presets() {
        // Dark fallback.
        let dark = Theme::from_name("dark");
        let unknown = Theme::from_name("nonsense");
        assert_eq!(dark.text, unknown.text, "unknown name must fall back to dark");

        // Light differs in at least bg/text.
        let light = Theme::from_name("light");
        assert_ne!(dark.bg, light.bg);
        assert_ne!(dark.text, light.text);

        // Minimal uses Color::Reset for text — never collides with dark/light.
        let minimal = Theme::from_name("minimal");
        assert_eq!(minimal.text, Color::Reset);
    }

    #[test]
    fn block_state_picks_running_in_accent() {
        let t = Theme::dark();
        assert_eq!(BlockState::Running.border_color(&t), t.accent);
        assert_eq!(BlockState::Pending.border_color(&t), t.accent);
    }

    #[test]
    fn block_state_picks_dim_for_success() {
        // Anti-regression: a completed tool block must NOT keep the accent
        // border — it should fade to dim so the eye flows to the current
        // running block instead.
        let t = Theme::dark();
        assert_eq!(BlockState::Success.border_color(&t), t.dim);
    }

    #[test]
    fn block_state_picks_error_color() {
        let t = Theme::dark();
        assert_eq!(BlockState::Error.border_color(&t), t.error);
        assert_eq!(BlockState::Warning.border_color(&t), t.warning);
    }
}
