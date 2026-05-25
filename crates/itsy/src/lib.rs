//! itsy — AI coding agent for small LLMs (8B-35B parameters).
//!
//! Major modules:
//!
//! * [`config`]       — environment, .env, itsy.toml, CLI flag layering
//! * [`tools`]        — tool schemas + 2-stage routing entrypoint
//! * [`executor`]     — built-in tool execution
//! * [`model_client`] — OpenAI-compatible chat client (sync + streaming)
//! * [`governor`]     — tool scoring, verification, hard-fail, classifier
//! * [`memory`]       — typed project memory store
//! * [`mcp_bridge`]   — built-in code-graph MCP server lifecycle
//! * [`code_graph`]   — native (tree-sitter + SQLite) code graph
//! * [`tui`]          — classic line-based rendering
//! * [`fullscreen`]   — ratatui alternate-screen renderer
//! * [`commands`]     — slash-command dispatch
//! * [`security`]     — redaction, ANSI stripping, path safety, shell escape
//! * [`session`]      — persistence, undo, snapshots, references
//! * [`tools_impl`]   — persistent shell, read tracker, MCP client, web browse
//! * [`model`]        — profiles, routing, adaptive temperature
//! * [`runtime`]      — deterministic tool router + provider adapters
//! * [`plugins`]      — plugin/skill loaders
//! * [`api`]          — programmatic embedding API
//! * [`paths`]        — canonical ~/.config/itsy/ layout

pub mod adapters;
pub mod api;
pub mod code_graph;
pub mod cognition_adapter;
pub mod commands;
pub mod config;
pub mod eval_runner;
pub mod executor;
pub mod features_adapter;
pub mod fullscreen;
pub mod fullscreen_widgets;
pub mod governor;
pub mod init_wizard;
pub mod interrupt;
pub mod knowledge;
pub mod loops_adapter;
pub mod lsp;
pub mod mcp_bridge;
pub mod memory;
pub mod model;
pub mod model_client;
pub mod paths;
pub mod plugins;
pub mod runtime;
pub mod security;
pub mod session;
pub mod session_log;
pub mod verification;
pub mod settings;
pub mod token_monitor;
pub mod tools;
pub mod tools_impl;
pub mod trace_recorder;
pub mod tui;

pub use config::Config;
