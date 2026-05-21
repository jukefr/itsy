//! itsy — AI coding agent for small LLMs (8B-35B parameters).
//!
//! Major modules:
//!
//! * [`config`]       — environment, .env, itsy.toml, CLI flag layering
//! * [`tools`]        — tool schemas + 2-stage routing entrypoint
//! * [`executor`]     — built-in tool execution
//! * [`model_client`] — OpenAI-compatible chat client (sync + streaming)
//! * [`governor`]     — tool scoring, verification, hard-fail, classifier
//! * [`escalation`]   — cloud-model fallback (Claude / OpenAI / DeepSeek)
//! * [`memory`]       — typed project memory store
//! * [`mcp_bridge`]   — built-in code-graph MCP server lifecycle
//! * [`tui`]          — classic line-based rendering
//! * [`fullscreen`]   — ratatui alternate-screen renderer
//! * [`commands`]     — slash-command dispatch
//! * [`security`]     — redaction, ANSI stripping, path safety, shell escape
//! * [`session`]      — persistence, undo, snapshots, references, plan
//! * [`tools_impl`]   — persistent shell, read tracker, MCP client, web browse
//! * [`model`]        — profiles, routing, adaptive temperature
//! * [`compiled`]     — deterministic tool router + provider adapters
//! * [`plugins`]      — plugin/skill loaders
//! * [`api`]          — programmatic embedding API

pub mod adapters;
pub mod api;
pub mod commands;
pub mod compiled;
pub mod config;
pub mod escalation;
pub mod executor;
pub mod fullscreen;
pub mod governor;
pub mod knowledge;
pub mod lsp;
pub mod mcp_bridge;
pub mod memory;
pub mod model;
pub mod model_client;
pub mod plugins;
pub mod security;
pub mod session;
pub mod tools;
pub mod tools_impl;
pub mod tui;

pub use config::Config;
