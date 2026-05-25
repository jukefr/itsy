//! OMP-style TUI widgets — bordered output blocks, code cells, status line,
//! todo tracker, slash overlay.
//!
//! Component philosophy mirrors `oh-my-pi/oh-my-pi`'s coding-agent TUI: a
//! shared rich `Theme`, a small set of unicode symbol presets, and pure
//! render helpers that return `Vec<Line<'static>>` for ratatui to paint.
//!
//! Each widget here is self-contained — no global state, no async — so they
//! can be unit-tested directly and composed however the parent layout wants.

pub mod theme;
pub mod symbols;
pub mod output_block;
pub mod status_line;
pub mod todo;
pub mod slash_overlay;
pub mod code_cell;
