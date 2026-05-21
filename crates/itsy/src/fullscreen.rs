//! A minimal alternate-screen Ratatui
//! renderer. The JS version is a custom renderer using raw stdin and
//! incremental writes; the Rust port uses ratatui's terminal abstraction.

use std::io;
use std::sync::Arc;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

#[derive(Debug, Clone)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub role: ChatRole,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct FullscreenState {
    pub chat_lines: Vec<ChatLine>,
    pub input: String,
    pub streaming: bool,
    pub status: String,
    pub quit: bool,
}

pub type SharedState = Arc<Mutex<FullscreenState>>;

pub struct Fullscreen {
    pub state: SharedState,
}

impl Fullscreen {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(FullscreenState::default())) }
    }

    pub fn add_chat(&self, role: ChatRole, text: impl Into<String>) {
        self.state.lock().chat_lines.push(ChatLine { role, text: text.into() });
    }

    pub fn add_tool(&self, name: &str, status: &str, msg: &str) {
        self.add_chat(ChatRole::Tool, format!("[{name}] {status} {msg}"));
    }

    pub fn add_diff(&self, path: &str, old_str: &str, new_str: &str, line: u32) {
        let mut state = self.state.lock();
        state.chat_lines.push(ChatLine {
            role: ChatRole::Tool,
            text: format!("--- {path}:{line}"),
        });
        for l in old_str.lines().take(3) {
            state.chat_lines.push(ChatLine { role: ChatRole::Tool, text: format!("- {l}") });
        }
        for l in new_str.lines().take(3) {
            state.chat_lines.push(ChatLine { role: ChatRole::Tool, text: format!("+ {l}") });
        }
    }

    pub fn set_streaming(&self, s: bool) {
        self.state.lock().streaming = s;
    }

    pub fn stream_token(&self, token: &str) {
        let mut state = self.state.lock();
        if let Some(last) = state.chat_lines.last_mut() {
            if matches!(last.role, ChatRole::Assistant) {
                last.text.push_str(token);
                return;
            }
        }
        state.chat_lines.push(ChatLine { role: ChatRole::Assistant, text: token.into() });
    }

    pub fn end_stream(&self) {
        // End-of-stream marker — currently a no-op; the next stream_token will
        // append to the same trailing assistant line until a new role appears.
    }

    pub fn set_status(&self, status: impl Into<String>) {
        self.state.lock().status = status.into();
    }
}

impl Default for Fullscreen {
    fn default() -> Self {
        Self::new()
    }
}

/// Run an interactive fullscreen loop. The provided `on_submit` is called when
/// the user presses Enter with non-empty input.
pub fn run_loop<F>(state: SharedState, mut on_submit: F) -> io::Result<()>
where
    F: FnMut(String),
{
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    loop {
        term.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(3), Constraint::Length(1)])
                .split(area);

            let st = state.lock();
            let lines: Vec<Line> = st
                .chat_lines
                .iter()
                .map(|l| match l.role {
                    ChatRole::User => Line::from(vec![
                        Span::styled("you: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::raw(l.text.clone()),
                    ]),
                    ChatRole::Assistant => Line::from(Span::raw(l.text.clone())),
                    ChatRole::System => Line::from(Span::styled(l.text.clone(), Style::default().fg(Color::DarkGray))),
                    ChatRole::Tool => Line::from(Span::styled(l.text.clone(), Style::default().fg(Color::Yellow))),
                })
                .collect();

            let chat = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::NONE));
            f.render_widget(chat, chunks[0]);

            let input = Paragraph::new(st.input.clone())
                .block(Block::default().borders(Borders::ALL).title("itsy"));
            f.render_widget(input, chunks[1]);

            let status = Paragraph::new(st.status.clone())
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(status, chunks[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(KeyEvent { code, kind: KeyEventKind::Press, .. }) = event::read()? {
                match code {
                    KeyCode::Char(c) => {
                        state.lock().input.push(c);
                    }
                    KeyCode::Backspace => {
                        state.lock().input.pop();
                    }
                    KeyCode::Enter => {
                        let text = std::mem::take(&mut state.lock().input);
                        if !text.is_empty() {
                            on_submit(text);
                        }
                    }
                    KeyCode::Esc => {
                        state.lock().quit = true;
                    }
                    _ => {}
                }
            }
        }

        if state.lock().quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
