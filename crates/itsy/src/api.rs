//! Programmatic Rust API for embedding itsy
//! into other applications.
//!
//! Mirrors `src/api/index.js` from the JS source. EventEmitter is replaced
//! with a `tokio::sync::broadcast` channel so any number of subscribers can
//! observe tool/agent events without blocking the agent loop.
//!
//! ```ignore
//! let cfg = itsy::config::load_config(&Default::default());
//! let mut agent = itsy::api::ItsyApi::new(cfg);
//! let mut events = agent.subscribe();
//! tokio::spawn(async move {
//!     while let Ok(ev) = events.recv().await {
//!         println!("{:?}", ev);
//!     }
//! });
//! let result = agent.run("create a hello world script").await?;
//! println!("{}", result.response);
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::executor::{execute_tool, ExecCtx};
use crate::governor::early_stop::EarlyStopDetector;
use crate::memory::MemoryStore;
use crate::model::profiles::{get_profile, EffectiveProfile};
use crate::model_client::{build_system_prompt, chat_completion, ChatContext};
use crate::tools::{get_all_tools, ToolDeps};
use crate::Config;

/// Per-run knobs that mirror the JS `config` object passed to `new SmallCode`.
/// These are kept separate from the global [`Config`] so embedders can tune
/// runtime behaviour without rebuilding the whole config tree.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Per-request timeout for the chat completion call.
    pub timeout: Duration,
    /// Optional whitelist of tool names. `None` = all tools allowed.
    pub tools: Option<HashSet<String>>,
    /// Verbose logging for diagnostics.
    pub verbose: bool,
    /// Working directory the agent runs from.
    pub cwd: PathBuf,
}

impl Default for RunOptions {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let s = crate::settings::get();
        Self {
            timeout: Duration::from_millis(s.request_timeout_ms),
            tools: None,
            verbose: s.verbose,
            cwd,
        }
    }
}

/// Event broadcast over the agent's subscriber channel. Mirrors the JS
/// `EventEmitter` events: `tool_start`, `tool_end`, `early_stop`, `token`,
/// `error`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    ToolStart { name: String, args: Value },
    ToolEnd { name: String, result: Value, ms: u128 },
    EarlyStop { signal: Value },
    Token { chunk: String },
    Error { message: String },
}

/// Result of one `run`. Mirrors JS `RunResult` exactly, with snake_case
/// field names for Rust idiom (the `serde(rename_all = "camelCase")` attr
/// keeps JSON output identical to the JS version).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub response: String,
    pub tool_calls: Vec<Value>,
    pub files_created: Vec<String>,
    /// Renamed from JS `filesEdited` to match the prior Rust field name.
    /// JSON serialises as `filesModified`.
    pub files_modified: Vec<String>,
    pub tokens_used: TokenUsage,
    /// Total elapsed time of `run`, in milliseconds.
    pub elapsed_ms: u128,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
}

pub struct ItsyApi {
    pub config: Config,
    pub options: RunOptions,
    pub early_stop: EarlyStopDetector,
    pub profile: EffectiveProfile,
    pub memory: Arc<Mutex<MemoryStore>>,
    pub history: Vec<Value>,
    events: broadcast::Sender<AgentEvent>,
}

impl ItsyApi {
    /// Construct with default run options sourced from `ITSY_*` environment.
    pub fn new(config: Config) -> Self {
        Self::with_options(config, RunOptions::default())
    }

    pub fn with_options(config: Config, options: RunOptions) -> Self {
        let profile = get_profile(&config.model.name, config.context.detected_window);
        let (tx, _) = broadcast::channel(256);
        Self {
            memory: Arc::new(Mutex::new(MemoryStore::new(&options.cwd))),
            config,
            options,
            profile,
            early_stop: EarlyStopDetector::new(),
            history: Vec::new(),
            events: tx,
        }
    }

    /// Subscribe to agent events. Drop the returned receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    /// Get the resolved model profile.
    pub fn profile(&self) -> &EffectiveProfile {
        &self.profile
    }

    fn emit(&self, ev: AgentEvent) {
        // Ignore send errors — no subscribers is fine.
        let _ = self.events.send(ev);
    }

    pub async fn run(&mut self, prompt: &str) -> Result<RunResult> {
        let start = std::time::Instant::now();
        self.early_stop.new_turn();
        self.history.push(serde_json::json!({"role": "user", "content": prompt}));

        let mut result = RunResult {
            response: String::new(),
            tool_calls: Vec::new(),
            files_created: Vec::new(),
            files_modified: Vec::new(),
            tokens_used: TokenUsage::default(),
            elapsed_ms: 0,
            success: false,
            error: None,
        };

        let flags = crate::config::Flags::default();
        let ctx = ExecCtx {
            config: &self.config,
            flags: &flags,
            memory: self.memory.clone(),
            mcp_bridge: None,
            mcp_client: None,
            fullscreen: None,
        };

        let mut tool_call_count: u32 = 0;
        'outer: while tool_call_count < self.config.limits.max_tool_calls_per_turn {
            let mut tools = get_all_tools(&self.config, None, &ToolDeps::default());
            if let Some(whitelist) = &self.options.tools {
                tools.retain(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|n| whitelist.contains(n))
                        .unwrap_or(false)
                });
            }

            let chat_ctx = ChatContext {
                config: &self.config,
                conversation: &self.history,
                tools,
                current_task_type: None,
                system_prompt: build_system_prompt(&self.config, "", "", "", None),
            };

            let completion = tokio::time::timeout(self.options.timeout, chat_completion(&chat_ctx)).await;
            let data = match completion {
                Ok(Some(d)) => d,
                Ok(None) => {
                    result.error = Some("No response from model".into());
                    break;
                }
                Err(_) => {
                    result.error = Some("model request timed out".into());
                    self.emit(AgentEvent::Error { message: "model request timed out".into() });
                    break;
                }
            };

            // Track token usage when the provider returned a `usage` block.
            if let Some(usage) = data.get("usage") {
                let pt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let ct = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                result.tokens_used.input += pt;
                result.tokens_used.output += ct;
                result.tokens_used.total = result.tokens_used.input + result.tokens_used.output;
            }

            let Some(msg) = data.pointer("/choices/0/message") else { break };
            self.history.push(msg.clone());

            let tool_calls = msg.get("tool_calls").and_then(|v| v.as_array()).cloned();
            if let Some(calls) = tool_calls {
                if !calls.is_empty() {
                    for tc in calls {
                        if tool_call_count >= self.config.limits.max_tool_calls_per_turn {
                            break 'outer;
                        }
                        tool_call_count += 1;
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_str = tc
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let args: Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| serde_json::json!({}));

                        self.emit(AgentEvent::ToolStart { name: name.clone(), args: args.clone() });
                        let tool_start = std::time::Instant::now();
                        let tool_result = execute_tool(&name, args.clone(), &ctx).await;
                        let tool_ms = tool_start.elapsed().as_millis();
                        self.emit(AgentEvent::ToolEnd {
                            name: name.clone(),
                            result: tool_result.clone(),
                            ms: tool_ms,
                        });

                        if let Some(action) = tool_result.get("action").and_then(|a| a.as_str()) {
                            if let Some(path) = tool_result
                                .get("path")
                                .and_then(|p| p.as_str())
                                .or_else(|| args.get("path").and_then(|p| p.as_str()))
                            {
                                match action {
                                    "Created" => result.files_created.push(path.into()),
                                    "Updated" | "Edited" => result.files_modified.push(path.into()),
                                    _ => {}
                                }
                            }
                        }

                        let tool_err = tool_result.get("error").and_then(|e| e.as_str()).map(String::from);
                        let tool_text = tool_result
                            .get("result")
                            .and_then(|r| r.as_str())
                            .map(String::from)
                            .or_else(|| tool_err.clone())
                            .unwrap_or_else(|| tool_result.to_string());

                        result.tool_calls.push(serde_json::json!({
                            "name": name,
                            "args": args,
                            "result": tool_text.clone(),
                            "error": tool_err.clone(),
                            "durationMs": tool_ms,
                        }));

                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.history.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": tool_text,
                        }));

                        // Patch-spiral detection mirrors the JS behaviour.
                        if name == "patch" || name == "read_and_patch" {
                            let arg_path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                            let old_str = args.get("old_str").and_then(|p| p.as_str()).unwrap_or("");
                            let new_str = args.get("new_str").and_then(|p| p.as_str()).unwrap_or("");
                            let ok = tool_err.is_none();
                            if let Some(signal) =
                                self.early_stop.record_patch_result(arg_path, ok, old_str, new_str)
                            {
                                let signal_value = serde_json::json!({
                                    "reason": signal.reason,
                                    "message": signal.message,
                                    "action": signal.action,
                                    "injection": signal.injection,
                                });
                                self.emit(AgentEvent::EarlyStop { signal: signal_value });
                                self.history.push(serde_json::json!({
                                    "role": "user",
                                    "content": signal.injection,
                                }));
                                break;
                            }
                        }
                    }
                    continue;
                }
            }

            // Plain text response — terminal.
            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                result.response = content.to_string();
                self.emit(AgentEvent::Token { chunk: content.to_string() });
            }
            break;
        }

        if result.response.is_empty() {
            // Fall back to the last assistant message we recorded.
            if let Some(content) = self
                .history
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
                .and_then(|m| m.get("content").cloned())
                .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
            {
                result.response = content;
            }
        }

        result.success = result.error.is_none();
        result.elapsed_ms = start.elapsed().as_millis();
        Ok(result)
    }
}
