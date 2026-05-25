//! Box-drawing and status symbols.
//!
//! Three presets — `unicode` (default), `rounded`, `ascii` — mirror what OMP's
//! symbol theme exposes. Status icons + spinner frames are shared across
//! presets (the icons themselves render fine in any modern terminal; ASCII
//! falls back to `[?]`, `[!]`, etc. for parity with users on very old
//! terminals).

/// Box-drawing characters for a single rendering style.
#[derive(Debug, Clone, Copy)]
pub struct BoxSymbols {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
    /// `┬` (T pointing down)
    pub tee_down: char,
    /// `┴` (T pointing up)
    pub tee_up: char,
    /// `┤` (T pointing left) — section separator on the right side.
    pub tee_left: char,
    /// `├` (T pointing right) — section separator on the left side.
    pub tee_right: char,
    /// `┼` (intersection)
    pub cross: char,
}

/// All symbol assets bundled into a theme-pickable preset.
#[derive(Debug, Clone, Copy)]
pub struct SymbolTheme {
    pub box_sharp: BoxSymbols,
    pub box_round: BoxSymbols,
    /// Status icons keyed by [`StatusIcon`].
    pub icons: StatusIcons,
    /// Spinner frames for in-progress states.
    pub spinner: &'static [&'static str],
    /// `·` separator used between status-line / header meta segments.
    pub dot: char,
    /// `→` arrow used for action / step transitions.
    pub arrow: char,
    /// `▶ ` prefix for input prompt.
    pub prompt: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct StatusIcons {
    pub pending: &'static str,
    pub running: &'static str,
    pub success: &'static str,
    pub warning: &'static str,
    pub error: &'static str,
    pub info: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusIcon {
    Pending,
    Running,
    Success,
    Warning,
    Error,
    Info,
}

impl StatusIcons {
    pub fn pick(&self, which: StatusIcon) -> &'static str {
        match which {
            StatusIcon::Pending => self.pending,
            StatusIcon::Running => self.running,
            StatusIcon::Success => self.success,
            StatusIcon::Warning => self.warning,
            StatusIcon::Error => self.error,
            StatusIcon::Info => self.info,
        }
    }
}

/// Default Unicode preset — sharp corners, rounded corners, full icon set.
pub const UNICODE: SymbolTheme = SymbolTheme {
    box_sharp: BoxSymbols {
        top_left: '┌', top_right: '┐', bottom_left: '└', bottom_right: '┘',
        horizontal: '─', vertical: '│',
        tee_down: '┬', tee_up: '┴', tee_left: '┤', tee_right: '├',
        cross: '┼',
    },
    box_round: BoxSymbols {
        top_left: '╭', top_right: '╮', bottom_left: '╰', bottom_right: '╯',
        horizontal: '─', vertical: '│',
        tee_down: '┬', tee_up: '┴', tee_left: '┤', tee_right: '├',
        cross: '┼',
    },
    icons: StatusIcons {
        pending: "⋯",
        running: "▶",
        success: "✓",
        warning: "⚠",
        error:   "✗",
        info:    "ℹ",
    },
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    dot: '·',
    arrow: '→',
    prompt: "▶ ",
};

/// ASCII-only preset for terminals that mangle box-drawing chars.
pub const ASCII: SymbolTheme = SymbolTheme {
    box_sharp: BoxSymbols {
        top_left: '+', top_right: '+', bottom_left: '+', bottom_right: '+',
        horizontal: '-', vertical: '|',
        tee_down: '+', tee_up: '+', tee_left: '+', tee_right: '+',
        cross: '+',
    },
    box_round: BoxSymbols {
        top_left: '.', top_right: '.', bottom_left: '\'', bottom_right: '\'',
        horizontal: '-', vertical: '|',
        tee_down: '+', tee_up: '+', tee_left: '+', tee_right: '+',
        cross: '+',
    },
    icons: StatusIcons {
        pending: "[*]",
        running: "[~]",
        success: "[+]",
        warning: "[!]",
        error:   "[x]",
        info:    "[i]",
    },
    spinner: &["|", "/", "-", "\\"],
    dot: '.',
    arrow: '>',
    prompt: "> ",
};

impl SymbolTheme {
    /// Pick a preset by name: `"ascii"` → [`ASCII`], else [`UNICODE`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "ascii" => ASCII,
            _ => UNICODE,
        }
    }

    /// Choose the spinner frame for a given tick. Stable on tick=0.
    pub fn spinner_frame(&self, tick: usize) -> &'static str {
        if self.spinner.is_empty() {
            return "";
        }
        self.spinner[tick % self.spinner.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_picks_unicode_by_default() {
        let s = SymbolTheme::from_name("anything-else");
        assert_eq!(s.box_sharp.top_left, '┌');
        assert_eq!(s.icons.success, "✓");
    }

    #[test]
    fn from_name_picks_ascii() {
        let s = SymbolTheme::from_name("ascii");
        assert_eq!(s.box_sharp.top_left, '+');
        assert_eq!(s.icons.success, "[+]");
    }

    /// Spinner cycles deterministically.
    #[test]
    fn spinner_frame_cycles() {
        let s = UNICODE;
        let first = s.spinner_frame(0);
        // After one full rotation we return to the same frame.
        assert_eq!(s.spinner_frame(s.spinner.len()), first);
        // Different ticks within the cycle differ.
        assert_ne!(s.spinner_frame(0), s.spinner_frame(1));
    }

    /// Empty-spinner preset never panics, returns "".
    #[test]
    fn empty_spinner_returns_empty() {
        let s = SymbolTheme { spinner: &[], ..UNICODE };
        assert_eq!(s.spinner_frame(0), "");
        assert_eq!(s.spinner_frame(1234), "");
    }

    /// `StatusIcons::pick` covers every variant.
    #[test]
    fn pick_covers_all_variants() {
        let i = UNICODE.icons;
        // Just exercise every arm — no panics, no empty strings.
        for v in [StatusIcon::Pending, StatusIcon::Running, StatusIcon::Success,
                  StatusIcon::Warning, StatusIcon::Error, StatusIcon::Info] {
            assert!(!i.pick(v).is_empty(), "{v:?} icon must be non-empty");
        }
    }

    /// Round-corners preset uses `╭╮╰╯`.
    #[test]
    fn round_corners_distinct_from_sharp() {
        let s = UNICODE;
        assert_eq!(s.box_round.top_left, '╭');
        assert_eq!(s.box_round.top_right, '╮');
        assert_eq!(s.box_round.bottom_left, '╰');
        assert_eq!(s.box_round.bottom_right, '╯');
        assert_ne!(s.box_round.top_left, s.box_sharp.top_left);
    }

    /// ASCII preset uses no chars > U+007F.
    /// Anti-regression: an "ASCII" preset that smuggled in `·` etc. would
    /// break on legacy terminals.
    #[test]
    fn ascii_preset_is_ascii_only() {
        let s = ASCII;
        for c in [s.box_sharp.top_left, s.box_sharp.horizontal, s.box_sharp.vertical,
                  s.box_round.top_left, s.dot, s.arrow] {
            assert!(c.is_ascii(), "{c:?} in ASCII preset is non-ASCII");
        }
        for icon in [s.icons.pending, s.icons.running, s.icons.success,
                     s.icons.warning, s.icons.error, s.icons.info] {
            assert!(icon.is_ascii(), "icon {icon:?} in ASCII preset is non-ASCII");
        }
        for frame in s.spinner {
            assert!(frame.is_ascii(), "spinner frame {frame:?} not ASCII");
        }
    }
}
