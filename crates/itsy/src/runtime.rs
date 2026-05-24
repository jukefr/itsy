//! Deterministic agent runtime: tool routing, cognition pipeline, feature
//! plumbing, and provider adapters. Everything here is no-LLM-call code
//! that drives the agent's structural behavior.

pub mod tool_router;
pub mod two_stage_router;
pub mod schemas;
pub mod metrics;
pub mod logger;
pub mod extensions;
pub mod flows;
pub mod cognition;
pub mod features;
pub mod providers;
pub mod tool_guidance;
pub mod agent_loop;
