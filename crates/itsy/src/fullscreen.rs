//! Full-Screen TUI runtime — ratatui port of `src/tui/fullscreen.js`.
//!
//! The JS source is a hand-rolled ANSI framebuffer renderer; the Rust port
//! achieves feature parity using `ratatui` widgets + `crossterm` events. The
//! public API mirrors the JS methods (snake_case) so the executor and the
//! command dispatcher can drive both implementations from the same call sites.
//!
//! Features ported:
//!   * Scrollback buffer with viewport + PageUp/PageDown nav
//!   * Streaming token rendering into a trailing assistant line
//!   * Tool indicators (running / ok / err) with coloured glyphs
//!   * Diff blocks (red `-` / green `+`) inside a bordered region
//!   * Status bar: model, token counts, cwd, task type, latency
//!   * Multi-line input area with cursor, history (Up/Down), Home/End edits
//!   * Slash command palette with fuzzy match against `crate::commands`
//!   * Approval modal (Yes/No) via `show_modal`
//!   * Multi-session tabs (top bar, Tab to cycle)
//!   * Toast notifications (`set_status`)
//!   * Themes (`ITSY_THEME=dark|light|minimal`)
//!   * Quit modal (Esc / Ctrl+Q)
//!   * Terminal resize handling

use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

// ─── Themes ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub muted: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub border: Color,
    pub status_bg: Color,
    pub input_bg: Color,
    pub brand: Color,
    pub brand_dim: Color,
    pub cmd_highlight: Color,
}

impl Theme {
    pub const fn dark() -> Self {
        Self {
            bg: Color::Rgb(15, 15, 15),
            fg: Color::Rgb(190, 190, 195),
            accent: Color::Rgb(180, 180, 185),
            muted: Color::Rgb(90, 90, 100),
            success: Color::Rgb(140, 200, 140),
            error: Color::Rgb(220, 90, 90),
            warning: Color::Rgb(220, 180, 80),
            border: Color::Rgb(50, 50, 55),
            status_bg: Color::Rgb(20, 20, 22),
            input_bg: Color::Rgb(18, 18, 20),
            brand: Color::Rgb(220, 220, 225),
            brand_dim: Color::Rgb(120, 120, 130),
            cmd_highlight: Color::Rgb(160, 140, 200),
        }
    }

    pub const fn light() -> Self {
        Self {
            bg: Color::Rgb(250, 250, 252),
            fg: Color::Rgb(30, 30, 40),
            accent: Color::Rgb(60, 60, 70),
            muted: Color::Rgb(140, 140, 160),
            success: Color::Rgb(20, 160, 60),
            error: Color::Rgb(200, 40, 40),
            warning: Color::Rgb(180, 130, 0),
            border: Color::Rgb(200, 200, 210),
            status_bg: Color::Rgb(235, 235, 240),
            input_bg: Color::Rgb(245, 245, 248),
            brand: Color::Rgb(40, 40, 50),
            brand_dim: Color::Rgb(120, 120, 130),
            cmd_highlight: Color::Rgb(100, 80, 160),
        }
    }

    pub const fn minimal() -> Self {
        Self {
            bg: Color::Reset,
            fg: Color::Reset,
            accent: Color::Gray,
            muted: Color::DarkGray,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            border: Color::DarkGray,
            status_bg: Color::Reset,
            input_bg: Color::Reset,
            brand: Color::White,
            brand_dim: Color::Gray,
            cmd_highlight: Color::Magenta,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "light" => Self::light(),
            "minimal" => Self::minimal(),
            _ => Self::dark(),
        }
    }

    pub fn from_env() -> Self {
        Self::from_name(&env::var("ITSY_THEME").unwrap_or_else(|_| "dark".into()))
    }
}

// ─── Chat model ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone)]
pub enum ChatLine {
    /// Plain text line for the given role. May contain newlines (wrapped at
    /// render time by ratatui).
    Text { role: ChatRole, text: String },
    /// Tool indicator: `[⚙ name] running... <msg>` or `[✓ name] <msg>`.
    Tool { name: String, status: ToolStatus, msg: String },
    /// Diff header introducing a block.
    DiffHeader { path: String, line: u32 },
    /// `-` line inside a diff block.
    DiffOld(String),
    /// `+` line inside a diff block.
    DiffNew(String),
    /// `... (N more)` truncation marker.
    DiffMore(String),
    /// Spacer between chat messages.
    Spacer,
}

#[derive(Debug, Clone, Default)]
pub struct SessionTab {
    pub title: String,
    pub chat_lines: Vec<ChatLine>,
    pub scroll: i32, // 0 = pinned to bottom, negative = scrolled up
    pub streaming: bool,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub cmd: &'static str,
    pub alias: Option<&'static str>,
    pub desc: &'static str,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec { cmd: "/quit", alias: Some("/q"), desc: "Exit itsy" },
    CommandSpec { cmd: "/clear", alias: None, desc: "Reset conversation" },
    CommandSpec { cmd: "/model", alias: None, desc: "Show/switch model" },
    CommandSpec { cmd: "/endpoint", alias: None, desc: "Switch API endpoint" },
    CommandSpec { cmd: "/stats", alias: None, desc: "Session statistics" },
    CommandSpec { cmd: "/tokens", alias: None, desc: "Token usage report" },
    CommandSpec { cmd: "/budget", alias: None, desc: "Context window budget" },
    CommandSpec { cmd: "/files", alias: None, desc: "List project files" },
    CommandSpec { cmd: "/diff", alias: None, desc: "Git diff summary" },
    CommandSpec { cmd: "/git", alias: None, desc: "Run git command" },
    CommandSpec { cmd: "/loop", alias: None, desc: "Validate + auto-fix file" },
    CommandSpec { cmd: "/memory", alias: None, desc: "View project memory" },
    CommandSpec { cmd: "/trace", alias: None, desc: "View execution traces" },
    CommandSpec { cmd: "/eval", alias: None, desc: "Run prompt evaluation" },
    CommandSpec { cmd: "/profile", alias: None, desc: "Model profile + routing" },
    CommandSpec { cmd: "/cognition", alias: None, desc: "Cognition status" },
    CommandSpec { cmd: "/mcp", alias: None, desc: "Connected MCP servers" },
    CommandSpec { cmd: "/skill", alias: None, desc: "Manage reusable skills" },
    CommandSpec { cmd: "/plugin", alias: None, desc: "Manage plugins" },
    CommandSpec { cmd: "/sessions", alias: None, desc: "List/resume sessions" },
    CommandSpec { cmd: "/session", alias: None, desc: "Parallel sessions" },
    CommandSpec { cmd: "/share", alias: None, desc: "Export session" },
    CommandSpec { cmd: "/undo", alias: None, desc: "Revert last edit" },
    CommandSpec { cmd: "/compact", alias: None, desc: "Trim conversation history" },
    CommandSpec { cmd: "/help", alias: None, desc: "Show all commands" },
    CommandSpec { cmd: "/version", alias: None, desc: "Show itsy version" },
];

/// Result returned from a blocking modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionResult {
    Selected(usize),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct PendingModal {
    pub prompt: String,
    pub options: Vec<String>,
    pub selection: usize,
    /// Set to `Some` by the event loop once a choice is made.
    pub resolved: Option<SelectionResult>,
}

const MAX_CHAT_LINES: usize = 5000;
const MAX_HISTORY: usize = 500;

#[derive(Debug)]
pub struct FullscreenState {
    pub theme: Theme,
    pub sessions: Vec<SessionTab>,
    pub active_session: usize,
    pub input: String,
    pub input_cursor: usize, // byte offset into input
    pub history: Vec<String>,
    pub history_idx: usize,
    pub palette_open: bool,
    pub palette_selection: usize,
    pub palette_scroll: usize,
    pub status: String,
    pub status_set_at: Option<Instant>,
    pub model: String,
    pub token_prompt: u32,
    pub token_completion: u32,
    pub task_type: String,
    pub latency_ms: u64,
    pub cwd: PathBuf,
    pub modal: Option<PendingModal>,
    pub quit_confirm: bool,
    pub quit: bool,
}

impl Default for FullscreenState {
    fn default() -> Self {
        Self {
            theme: Theme::from_env(),
            sessions: vec![SessionTab { title: "main".into(), ..Default::default() }],
            active_session: 0,
            input: String::new(),
            input_cursor: 0,
            history: Vec::new(),
            history_idx: 0,
            palette_open: false,
            palette_selection: 0,
            palette_scroll: 0,
            status: String::new(),
            status_set_at: None,
            model: env::var("ITSY_MODEL").unwrap_or_else(|_| "unknown".into()),
            token_prompt: 0,
            token_completion: 0,
            task_type: String::new(),
            latency_ms: 0,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            modal: None,
            quit_confirm: false,
            quit: false,
        }
    }
}

impl FullscreenState {
    fn active_mut(&mut self) -> &mut SessionTab {
        let idx = self.active_session.min(self.sessions.len().saturating_sub(1));
        &mut self.sessions[idx]
    }

    fn active(&self) -> &SessionTab {
        let idx = self.active_session.min(self.sessions.len().saturating_sub(1));
        &self.sessions[idx]
    }

    fn push_line(&mut self, line: ChatLine) {
        let tab = self.active_mut();
        tab.chat_lines.push(line);
        tab.scroll = 0;
        if tab.chat_lines.len() > MAX_CHAT_LINES {
            let drop = tab.chat_lines.len() - MAX_CHAT_LINES;
            tab.chat_lines.drain(0..drop);
        }
    }
}

pub type SharedState = Arc<Mutex<FullscreenState>>;

pub struct Fullscreen {
    pub state: SharedState,
}

impl Fullscreen {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(FullscreenState::default())) }
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self { state: Arc::new(Mutex::new(FullscreenState { theme, ..Default::default() })) }
    }

    // ── Public API mirroring the JS class ────────────────────────────────

    pub fn add_chat(&self, role: ChatRole, text: impl Into<String>) {
        let mut st = self.state.lock();
        st.push_line(ChatLine::Text { role, text: text.into() });
        st.push_line(ChatLine::Spacer);
    }

    pub fn add_tool(&self, name: &str, status: &str, msg: &str) {
        let status = match status {
            "ok" => ToolStatus::Ok,
            "err" | "error" => ToolStatus::Err,
            _ => ToolStatus::Running,
        };
        let mut st = self.state.lock();
        st.push_line(ChatLine::Tool {
            name: name.to_string(),
            status,
            msg: msg.to_string(),
        });
    }

    pub fn add_diff(&self, path: &str, old_str: &str, new_str: &str, line: u32) {
        const MAX: usize = 8;
        let mut st = self.state.lock();
        st.push_line(ChatLine::DiffHeader { path: path.into(), line });
        let old_lines: Vec<&str> = old_str.split('\n').collect();
        let new_lines: Vec<&str> = new_str.split('\n').collect();
        for l in old_lines.iter().take(MAX) {
            st.push_line(ChatLine::DiffOld((*l).into()));
        }
        if old_lines.len() > MAX {
            st.push_line(ChatLine::DiffMore(format!("... ({} more)", old_lines.len() - MAX)));
        }
        for l in new_lines.iter().take(MAX) {
            st.push_line(ChatLine::DiffNew((*l).into()));
        }
        if new_lines.len() > MAX {
            st.push_line(ChatLine::DiffMore(format!("... ({} more)", new_lines.len() - MAX)));
        }
        st.push_line(ChatLine::Spacer);
    }

    pub fn set_streaming(&self, s: bool) {
        let mut st = self.state.lock();
        st.active_mut().streaming = s;
    }

    pub fn stream_token(&self, token: &str) {
        let mut st = self.state.lock();
        // Append to a trailing Assistant Text line; otherwise create one.
        let need_new = !matches!(st.active().chat_lines.last(), Some(ChatLine::Text { role: ChatRole::Assistant, .. }));
        if need_new {
            st.push_line(ChatLine::Text { role: ChatRole::Assistant, text: String::new() });
        }
        // Now mutate the trailing text in place.
        let tab = st.active_mut();
        if let Some(ChatLine::Text { text, .. }) = tab.chat_lines.last_mut() {
            text.push_str(token);
        }
        tab.scroll = 0;
    }

    pub fn end_stream(&self) {
        let mut st = self.state.lock();
        st.active_mut().streaming = false;
        st.push_line(ChatLine::Spacer);
    }

    pub fn set_status(&self, status: impl Into<String>) {
        let mut st = self.state.lock();
        st.status = status.into();
        st.status_set_at = Some(Instant::now());
    }

    pub fn set_model(&self, name: impl Into<String>) {
        self.state.lock().model = name.into();
    }

    pub fn set_token_count(&self, prompt: u32, completion: u32) {
        let mut st = self.state.lock();
        st.token_prompt = prompt;
        st.token_completion = completion;
    }

    pub fn set_task_type(&self, s: impl Into<String>) {
        self.state.lock().task_type = s.into();
    }

    pub fn set_latency(&self, ms: u64) {
        self.state.lock().latency_ms = ms;
    }

    pub fn new_session(&self, title: impl Into<String>) {
        let mut st = self.state.lock();
        st.sessions.push(SessionTab { title: title.into(), ..Default::default() });
        st.active_session = st.sessions.len() - 1;
    }

    /// Request a blocking modal. Returns a token the event loop will resolve
    /// once the user presses a key. The caller polls `try_take_modal_result`.
    pub fn show_modal(&self, prompt: impl Into<String>, options: Vec<String>) -> SelectionResult {
        {
            let mut st = self.state.lock();
            st.modal = Some(PendingModal {
                prompt: prompt.into(),
                options,
                selection: 0,
                resolved: None,
            });
        }
        // Spin until the event loop fills in the result. This must be called
        // from a thread that is NOT the event loop thread.
        loop {
            {
                let mut st = self.state.lock();
                if let Some(m) = &st.modal {
                    if m.resolved.is_some() {
                        let res = st.modal.as_mut().unwrap().resolved.take().unwrap();
                        st.modal = None;
                        return res;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn request_quit(&self) {
        self.state.lock().quit = true;
    }
}

impl Default for Fullscreen {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn cursor_byte_to_char_idx(s: &str, byte_idx: usize) -> usize {
    s.char_indices().take_while(|(b, _)| *b < byte_idx).count()
}

fn insert_str_at(s: &mut String, byte_idx: usize, text: &str) {
    s.insert_str(byte_idx, text);
}

fn remove_prev_char(s: &mut String, byte_idx: usize) -> usize {
    if byte_idx == 0 {
        return 0;
    }
    // Find previous char boundary.
    let mut prev = byte_idx - 1;
    while prev > 0 && !s.is_char_boundary(prev) {
        prev -= 1;
    }
    s.replace_range(prev..byte_idx, "");
    prev
}

fn remove_next_char(s: &mut String, byte_idx: usize) {
    if byte_idx >= s.len() {
        return;
    }
    let mut end = byte_idx + 1;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    s.replace_range(byte_idx..end, "");
}

fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.len();
    }
    let mut end = byte_idx + 1;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    end
}

fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx == 0 {
        return 0;
    }
    let mut prev = byte_idx - 1;
    while prev > 0 && !s.is_char_boundary(prev) {
        prev -= 1;
    }
    prev
}

fn filtered_commands(filter: &str) -> Vec<&'static CommandSpec> {
    let f = filter.trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .filter(|c| {
            let name = c.cmd.trim_start_matches('/').to_lowercase();
            let matches_name = name.starts_with(&f) || name.contains(&f);
            let matches_alias = c
                .alias
                .map(|a| {
                    let a = a.trim_start_matches('/').to_lowercase();
                    a.starts_with(&f) || a.contains(&f)
                })
                .unwrap_or(false);
            matches_name || matches_alias
        })
        .collect()
}

// ─── Rendering ────────────────────────────────────────────────────────────

fn render_chat_lines<'a>(lines: &'a [ChatLine], theme: &Theme) -> Vec<Line<'a>> {
    let mut out = Vec::with_capacity(lines.len() * 2);
    for cl in lines.iter() {
        match cl {
            ChatLine::Text { role, text } => {
                let (prefix, prefix_style) = match role {
                    ChatRole::User => (
                        " You: ",
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    ChatRole::Assistant => (
                        " AI:  ",
                        Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
                    ),
                    ChatRole::System => ("      ", Style::default().fg(theme.muted)),
                    ChatRole::Tool => ("      ", Style::default().fg(theme.accent)),
                };
                let body_style = match role {
                    ChatRole::System => Style::default().fg(theme.muted),
                    _ => Style::default().fg(theme.fg),
                };
                // Split on newlines so the prefix appears only on the first line.
                let parts: Vec<&str> = text.split('\n').collect();
                for (i, part) in parts.iter().enumerate() {
                    if i == 0 {
                        out.push(Line::from(vec![
                            Span::styled(prefix, prefix_style),
                            Span::styled((*part).to_string(), body_style),
                        ]));
                    } else {
                        out.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled((*part).to_string(), body_style),
                        ]));
                    }
                }
            }
            ChatLine::Tool { name, status, msg } => {
                let (glyph, glyph_style) = match status {
                    ToolStatus::Running => (
                        "⚙",
                        Style::default().fg(theme.accent),
                    ),
                    ToolStatus::Ok => (
                        "✓",
                        Style::default().fg(theme.success),
                    ),
                    ToolStatus::Err => (
                        "✗",
                        Style::default().fg(theme.error),
                    ),
                };
                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled(glyph, glyph_style),
                    Span::raw(" ["),
                    Span::styled(name.clone(), Style::default().fg(theme.accent)),
                    Span::raw("] "),
                ];
                if matches!(status, ToolStatus::Running) && !msg.contains("running") {
                    spans.push(Span::styled("running... ", Style::default().fg(theme.muted)));
                }
                spans.push(Span::styled(msg.clone(), Style::default().fg(theme.muted)));
                out.push(Line::from(spans));
            }
            ChatLine::DiffHeader { path, line } => {
                out.push(Line::from(vec![
                    Span::styled("  ┌─ ", Style::default().fg(theme.border)),
                    Span::styled(
                        format!("{path}:{line}"),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            ChatLine::DiffOld(s) => {
                out.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(theme.border)),
                    Span::styled(format!("- {s}"), Style::default().fg(theme.error)),
                ]));
            }
            ChatLine::DiffNew(s) => {
                out.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(theme.border)),
                    Span::styled(format!("+ {s}"), Style::default().fg(theme.success)),
                ]));
            }
            ChatLine::DiffMore(s) => {
                out.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(theme.border)),
                    Span::styled(s.clone(), Style::default().fg(theme.muted)),
                ]));
            }
            ChatLine::Spacer => {
                out.push(Line::from(""));
            }
        }
    }
    out
}

fn render_welcome(theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let logo: &[&str] = if width >= 80 {
        &[
            "██╗████████╗███████╗██╗   ██╗",
            "██║╚══██╔══╝██╔════╝╚██╗ ██╔╝",
            "██║   ██║   ███████╗ ╚████╔╝ ",
            "██║   ██║   ╚════██║  ╚██╔╝  ",
            "██║   ██║   ███████║   ██║   ",
            "╚═╝   ╚═╝   ╚══════╝   ╚═╝   ",
        ]
    } else {
        &["i t s y"]
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    for l in logo {
        lines.push(Line::from(Span::styled(
            (*l).to_string(),
            Style::default().fg(theme.brand).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("v{}", env!("CARGO_PKG_VERSION")),
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));
    let hints = [
        ("/help", "show help", "ctrl+l"),
        ("/model", "switch model", ""),
        ("/memory", "project memory", ""),
        ("/skill", "manage skills", ""),
        ("/quit", "exit", "ctrl+c"),
    ];
    for (c, d, s) in hints {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<10}", c), Style::default().fg(theme.cmd_highlight)),
            Span::styled(format!(" {:<18}", d), Style::default().fg(theme.fg)),
            Span::styled(s.to_string(), Style::default().fg(theme.muted)),
        ]));
    }
    lines
}

fn render_status_bar<'a>(st: &'a FullscreenState) -> Line<'a> {
    let theme = &st.theme;
    let left = if !st.status.is_empty() {
        format!(" {} ", st.status)
    } else if !st.task_type.is_empty() {
        format!(" {} ", st.task_type)
    } else {
        " enter send  shift+drag copy ".to_string()
    };
    let cwd = st.cwd.display().to_string();
    let mid = format!(
        " tok p={} c={} t={}  {}ms ",
        st.token_prompt,
        st.token_completion,
        st.token_prompt + st.token_completion,
        st.latency_ms
    );
    let stream_glyph = if st.active().streaming { "⟳" } else { "●" };
    let right = format!(" itsy  {}  {}  {} ", st.model, cwd, stream_glyph);
    Line::from(vec![
        Span::styled(left, Style::default().fg(theme.accent).bg(theme.status_bg)),
        Span::styled(mid, Style::default().fg(theme.muted).bg(theme.status_bg)),
        Span::styled(right, Style::default().fg(theme.brand_dim).bg(theme.status_bg)),
    ])
}

fn render_input<'a>(st: &'a FullscreenState) -> Paragraph<'a> {
    let theme = &st.theme;
    let title = if st.palette_open {
        "itsy  ↑↓ navigate  enter select  esc cancel"
    } else {
        "itsy"
    };
    Paragraph::new(Line::from(vec![
        Span::styled(" > ", Style::default().fg(theme.muted)),
        Span::styled(st.input.clone(), Style::default().fg(theme.fg)),
    ]))
    .wrap(Wrap { trim: false })
    .style(Style::default().bg(theme.input_bg))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(
                title.to_string(),
                Style::default().fg(theme.accent),
            )),
    )
}

fn render_palette<'a>(st: &'a FullscreenState, area: Rect) -> (Paragraph<'a>, Rect) {
    let theme = &st.theme;
    let filtered = filtered_commands(&st.input);
    let max_h = area.height.saturating_sub(2).clamp(3, 12) as usize;
    let visible = filtered.len().min(max_h);
    let scroll = st.palette_scroll.min(filtered.len().saturating_sub(visible));
    let sel = st.palette_selection.min(filtered.len().saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for i in 0..visible {
        let idx = i + scroll;
        let Some(c) = filtered.get(idx) else { break };
        let is_sel = idx == sel;
        let cmd_text = match c.alias {
            Some(a) => format!("{} ({})", c.cmd, a),
            None => c.cmd.to_string(),
        };
        let style = if is_sel {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<16}", cmd_text), style),
            Span::styled(format!(" {}", c.desc), Style::default().fg(theme.muted)),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no matches)".to_string(),
            Style::default().fg(theme.muted),
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" commands {}/{} ", filtered.len().min(scroll + visible), filtered.len()),
            Style::default().fg(theme.accent),
        ));
    let p = Paragraph::new(lines).block(block);
    let h = (visible.max(1) as u16) + 2;
    let w = area.width.min(50);
    let x = area.x;
    let y = area.y.saturating_sub(h);
    let rect = Rect { x, y, width: w, height: h };
    (p, rect)
}

fn render_modal<'a>(modal: &'a PendingModal, area: Rect, theme: &Theme) -> (Paragraph<'a>, Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        modal.prompt.clone(),
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for (i, opt) in modal.options.iter().enumerate() {
        let style = if i == modal.selection {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.fg)
        };
        lines.push(Line::from(Span::styled(format!(" {}. {} ", i + 1, opt), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ select   enter confirm   esc cancel ".to_string(),
        Style::default().fg(theme.muted),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning))
        .title(Span::styled(" confirm ", Style::default().fg(theme.warning)));
    let p = Paragraph::new(lines).block(block).alignment(Alignment::Left);
    let w = area.width.clamp(20, 60);
    let h = (modal.options.len() as u16 + 5).min(area.height.saturating_sub(2)).max(5);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    (p, Rect { x, y, width: w, height: h })
}

fn render_quit_confirm<'a>(area: Rect, theme: &Theme) -> (Paragraph<'a>, Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Quit itsy?".to_string(),
            Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  y to confirm    n / esc to cancel  ".to_string(),
            Style::default().fg(theme.muted),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning))
        .title(Span::styled(" quit? ", Style::default().fg(theme.warning)));
    let p = Paragraph::new(lines).block(block).alignment(Alignment::Center);
    let w = 36u16.min(area.width.saturating_sub(2));
    let h = 6u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    (p, Rect { x, y, width: w, height: h })
}

// ─── Event loop ───────────────────────────────────────────────────────────

/// Run an interactive fullscreen loop. `on_submit` is invoked when the user
/// presses Enter on non-slash text. `on_command` is invoked for `/...` lines.
pub fn run_loop<F, G>(state: SharedState, mut on_submit: F, mut on_command: G) -> io::Result<()>
where
    F: FnMut(String),
    G: FnMut(String),
{
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let res = (|| -> io::Result<()> {
        loop {
            let need_quit = {
                let st = state.lock();
                st.quit
            };
            if need_quit {
                break;
            }

            // Auto-clear status toast after 5s.
            {
                let mut st = state.lock();
                if let Some(t) = st.status_set_at {
                    if t.elapsed() > Duration::from_secs(5) {
                        st.status.clear();
                        st.status_set_at = None;
                    }
                }
            }

            term.draw(|f| {
                let area = f.area();
                let st = state.lock();
                let theme = st.theme;

                // Layout: tabs (1) | chat (min) | input (3) | status (1)
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(1),
                        Constraint::Length(3),
                        Constraint::Length(1),
                    ])
                    .split(area);

                // Tabs
                let tabs: Vec<Line> = st
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let style = if i == st.active_session {
                            Style::default()
                                .fg(theme.brand)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                        } else {
                            Style::default().fg(theme.muted)
                        };
                        Line::from(Span::styled(format!(" {} ", s.title), style))
                    })
                    .collect();
                let tabs_widget = Tabs::new(tabs)
                    .select(st.active_session)
                    .divider("│")
                    .style(Style::default().bg(theme.status_bg));
                f.render_widget(tabs_widget, chunks[0]);

                // Chat
                let tab = st.active();
                let chat_area = chunks[1];
                let body_lines: Vec<Line> = if tab.chat_lines.is_empty() {
                    render_welcome(&theme, chat_area.width)
                } else {
                    render_chat_lines(&tab.chat_lines, &theme)
                };
                // Compute scroll: the Paragraph scroll offset is relative to the top.
                // Pin to bottom unless user scrolled up (negative scroll).
                let total = body_lines.len() as i32;
                let view_h = chat_area.height as i32;
                let max_top = (total - view_h).max(0);
                let top = ((max_top) + tab.scroll).clamp(0, max_top) as u16;
                let chat = Paragraph::new(body_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((top, 0))
                    .block(Block::default().borders(Borders::NONE));
                f.render_widget(chat, chat_area);

                // Input
                let input_area = chunks[2];
                let input_widget = render_input(&st);
                f.render_widget(input_widget, input_area);

                // Status bar
                let status_widget = Paragraph::new(render_status_bar(&st))
                    .style(Style::default().bg(theme.status_bg));
                f.render_widget(status_widget, chunks[3]);

                // Cursor position inside the input area.
                let cursor_char_idx = cursor_byte_to_char_idx(&st.input, st.input_cursor);
                let before_cursor: String = st.input.chars().take(cursor_char_idx).collect();
                let cursor_visual = UnicodeWidthStr::width(before_cursor.as_str()) as u16;
                // "│ > " prefix inside the bordered input → x offset = 4.
                let cursor_x = input_area.x + 4 + cursor_visual;
                let cursor_y = input_area.y + 1;
                f.set_cursor_position((cursor_x.min(area.x + area.width - 1), cursor_y));

                // Palette overlay (above the input box).
                if st.palette_open {
                    let (pal, rect) = render_palette(&st, input_area);
                    f.render_widget(Clear, rect);
                    f.render_widget(pal, rect);
                }

                // Modal overlay.
                if let Some(modal) = &st.modal {
                    let (m, r) = render_modal(modal, area, &theme);
                    f.render_widget(Clear, r);
                    f.render_widget(m, r);
                }

                // Quit confirmation overlay.
                if st.quit_confirm {
                    let (q, r) = render_quit_confirm(area, &theme);
                    f.render_widget(Clear, r);
                    f.render_widget(q, r);
                }
            })?;

            if !event::poll(Duration::from_millis(50))? {
                continue;
            }

            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    handle_key(key, &state, &mut on_submit, &mut on_command);
                }
                Event::Mouse(m) => handle_mouse(m, &state),
                Event::Resize(_, _) => { /* ratatui redraws on next iteration */ }
                _ => {}
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    res
}

fn handle_mouse(m: MouseEvent, state: &SharedState) {
    let mut st = state.lock();
    match m.kind {
        MouseEventKind::ScrollUp => {
            let tab = st.active_mut();
            let max_back = -((tab.chat_lines.len() as i32) - 1).max(0);
            tab.scroll = (tab.scroll - 3).max(max_back);
        }
        MouseEventKind::ScrollDown => {
            let tab = st.active_mut();
            tab.scroll = (tab.scroll + 3).min(0);
        }
        _ => {}
    }
}

fn handle_key<F, G>(key: KeyEvent, state: &SharedState, on_submit: &mut F, on_command: &mut G)
where
    F: FnMut(String),
    G: FnMut(String),
{
    // Modal first — eats all keys.
    {
        let mut st = state.lock();
        if let Some(modal) = st.modal.as_mut() {
            match key.code {
                KeyCode::Up
                    if modal.selection > 0 => {
                        modal.selection -= 1;
                    }
                KeyCode::Down
                    if modal.selection + 1 < modal.options.len() => {
                        modal.selection += 1;
                    }
                KeyCode::Enter => {
                    modal.resolved = Some(SelectionResult::Selected(modal.selection));
                }
                KeyCode::Esc => {
                    modal.resolved = Some(SelectionResult::Cancelled);
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    // Convention: y = first option.
                    modal.resolved = Some(SelectionResult::Selected(0));
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    modal.resolved = Some(SelectionResult::Cancelled);
                }
                KeyCode::Char(c) => {
                    if let Some(d) = c.to_digit(10) {
                        let idx = (d as usize).saturating_sub(1);
                        if idx < modal.options.len() {
                            modal.resolved = Some(SelectionResult::Selected(idx));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
    }

    // Quit confirmation
    {
        let mut st = state.lock();
        if st.quit_confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    st.quit = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    st.quit_confirm = false;
                }
                _ => {}
            }
            return;
        }
    }

    // Ctrl combos
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => {
                state.lock().quit = true;
                return;
            }
            KeyCode::Char('q') => {
                state.lock().quit_confirm = true;
                return;
            }
            KeyCode::Char('l') => {
                // Force redraw — handled by the loop on next tick.
                return;
            }
            KeyCode::Char('a') => {
                state.lock().input_cursor = 0;
                return;
            }
            KeyCode::Char('e') => {
                let mut st = state.lock();
                st.input_cursor = st.input.len();
                return;
            }
            KeyCode::Char('u') => {
                let mut st = state.lock();
                st.input.clear();
                st.input_cursor = 0;
                st.palette_open = false;
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => {
            let mut st = state.lock();
            if !st.sessions.is_empty() {
                st.active_session = (st.active_session + 1) % st.sessions.len();
            }
        }
        KeyCode::BackTab => {
            let mut st = state.lock();
            if !st.sessions.is_empty() {
                if st.active_session == 0 {
                    st.active_session = st.sessions.len() - 1;
                } else {
                    st.active_session -= 1;
                }
            }
        }
        KeyCode::Esc => {
            let mut st = state.lock();
            if st.palette_open {
                st.palette_open = false;
                st.palette_selection = 0;
                st.palette_scroll = 0;
            } else {
                st.quit_confirm = true;
            }
        }
        KeyCode::Enter => {
            let payload = {
                let mut st = state.lock();
                // Palette select-on-enter
                if st.palette_open {
                    let filtered = filtered_commands(&st.input);
                    if !filtered.is_empty() {
                        let sel = st.palette_selection.min(filtered.len() - 1);
                        st.input = filtered[sel].cmd.to_string();
                        st.input_cursor = st.input.len();
                    }
                    st.palette_open = false;
                    st.palette_selection = 0;
                    st.palette_scroll = 0;
                }
                let text = std::mem::take(&mut st.input);
                st.input_cursor = 0;
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    st.history.push(trimmed.clone());
                    if st.history.len() > MAX_HISTORY {
                        let drop = st.history.len() - MAX_HISTORY;
                        st.history.drain(0..drop);
                    }
                    st.history_idx = st.history.len();
                }
                trimmed
            };
            if payload.is_empty() {
                return;
            }
            if payload.starts_with('/') {
                on_command(payload);
            } else {
                state.lock().push_line(ChatLine::Text {
                    role: ChatRole::User,
                    text: payload.clone(),
                });
                state.lock().push_line(ChatLine::Spacer);
                on_submit(payload);
            }
        }
        KeyCode::Backspace => {
            let mut st = state.lock();
            let pos = st.input_cursor;
            let new_pos = remove_prev_char(&mut st.input, pos);
            st.input_cursor = new_pos;
            st.palette_open = st.input.starts_with('/');
            if !st.palette_open {
                st.palette_selection = 0;
                st.palette_scroll = 0;
            }
        }
        KeyCode::Delete => {
            let mut st = state.lock();
            let pos = st.input_cursor;
            remove_next_char(&mut st.input, pos);
        }
        KeyCode::Left => {
            let mut st = state.lock();
            st.input_cursor = prev_char_boundary(&st.input, st.input_cursor);
        }
        KeyCode::Right => {
            let mut st = state.lock();
            st.input_cursor = next_char_boundary(&st.input, st.input_cursor);
        }
        KeyCode::Home => {
            state.lock().input_cursor = 0;
        }
        KeyCode::End => {
            let mut st = state.lock();
            st.input_cursor = st.input.len();
        }
        KeyCode::Up => {
            let mut st = state.lock();
            if st.palette_open {
                if st.palette_selection > 0 {
                    st.palette_selection -= 1;
                }
                if st.palette_selection < st.palette_scroll {
                    st.palette_scroll = st.palette_selection;
                }
            } else if st.history_idx > 0 {
                st.history_idx -= 1;
                st.input = st.history.get(st.history_idx).cloned().unwrap_or_default();
                st.input_cursor = st.input.len();
            }
        }
        KeyCode::Down => {
            let mut st = state.lock();
            if st.palette_open {
                let max = filtered_commands(&st.input).len().saturating_sub(1);
                if st.palette_selection < max {
                    st.palette_selection += 1;
                }
            } else {
                let hist_len = st.history.len();
                if st.history_idx + 1 < hist_len {
                    st.history_idx += 1;
                    st.input = st.history.get(st.history_idx).cloned().unwrap_or_default();
                } else {
                    st.history_idx = hist_len;
                    st.input.clear();
                }
                let len = st.input.len();
                st.input_cursor = len;
            }
        }
        KeyCode::PageUp => {
            let mut st = state.lock();
            let h = 10i32;
            let tab = st.active_mut();
            let max_back = -((tab.chat_lines.len() as i32) - 1).max(0);
            tab.scroll = (tab.scroll - h).max(max_back);
        }
        KeyCode::PageDown => {
            let mut st = state.lock();
            let h = 10i32;
            let tab = st.active_mut();
            tab.scroll = (tab.scroll + h).min(0);
        }
        KeyCode::Char(c) => {
            let mut st = state.lock();
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            let pos = st.input_cursor;
            insert_str_at(&mut st.input, pos, s);
            st.input_cursor = pos + s.len();
            if st.input.starts_with('/') {
                st.palette_open = true;
            } else {
                st.palette_open = false;
                st.palette_selection = 0;
                st.palette_scroll = 0;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_movement_is_char_boundary_aware() {
        let mut s = String::from("a日b");
        // Cursor between 'a' and '日'
        let p = next_char_boundary(&s, 0);
        assert_eq!(p, 1);
        // Past '日' (3 bytes in UTF-8)
        let p2 = next_char_boundary(&s, p);
        assert_eq!(p2, 4);
        // Backspace at end
        let len = s.len();
        let new = remove_prev_char(&mut s, len);
        assert_eq!(s, "a日");
        assert_eq!(new, 4);
    }

    #[test]
    fn filtered_commands_matches_quit() {
        let m = filtered_commands("q");
        assert!(m.iter().any(|c| c.cmd == "/quit"));
    }

    #[test]
    fn stream_token_appends_to_assistant_line() {
        let fs = Fullscreen::new();
        fs.stream_token("hello ");
        fs.stream_token("world");
        let st = fs.state.lock();
        let tab = st.active();
        match tab.chat_lines.last().unwrap() {
            ChatLine::Text { role: ChatRole::Assistant, text } => {
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected trailing assistant line"),
        }
    }

    #[test]
    fn add_diff_emits_expected_lines() {
        let fs = Fullscreen::new();
        fs.add_diff("foo.rs", "a\nb", "c\nd", 7);
        let st = fs.state.lock();
        let lines = &st.active().chat_lines;
        assert!(matches!(lines.first(), Some(ChatLine::DiffHeader { .. })));
        let has_old = lines.iter().any(|l| matches!(l, ChatLine::DiffOld(s) if s == "a"));
        let has_new = lines.iter().any(|l| matches!(l, ChatLine::DiffNew(s) if s == "d"));
        assert!(has_old && has_new);
    }

    #[test]
    fn set_token_count_updates_state() {
        let fs = Fullscreen::new();
        fs.set_token_count(10, 20);
        let st = fs.state.lock();
        assert_eq!(st.token_prompt, 10);
        assert_eq!(st.token_completion, 20);
    }
}
