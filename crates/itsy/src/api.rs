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
        let snap_arc = crate::session::snapshot::get_snapshot_manager(self.options.cwd.clone());
        let ctx = ExecCtx {
            config: &self.config,
            flags: &flags,
            memory: self.memory.clone(),
            mcp_bridge: None,
            mcp_client: None,
            fullscreen: None,
            read_tracker: crate::tools_impl::read_tracker::get_read_tracker(),
            file_state: crate::session::file_state::get_file_state_tracker(),
            snapshot_manager: snap_arc.as_ref(),
        };

        let mut tool_call_count: u32 = 0;
        'outer: while tool_call_count < crate::settings::get().max_tool_calls_per_turn {
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
                model_name: &self.config.model.name,
                base_url: &self.config.model.base_url,
                api_key: self.config.model.api_key.clone(),
                timeout: self.options.timeout,
                temp_adapt: self.config.features.temp_adapt,
                conversation: &self.history,
                tools,
                current_task_type: None,
                system_prompt: build_system_prompt(&self.config, "", "", "", None),
                force_disable_thinking: false,
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
                        if tool_call_count >= crate::settings::get().max_tool_calls_per_turn {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn min_config() -> Config {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        Config {
            model: ModelConfig { provider: "openai".into(), name: "test".into(), base_url: "http://localhost:1234/v1".into(), timeout: 60, api_key: None },
            context: ContextConfig { max_budget_pct: 70, detected_window: 32_768, working_memory_tokens: 8192, summary_threshold: 8000 },
            tools: ToolsConfig { bash_timeout: 30, tool_routing: "direct".into(), web_browse: false, shell_persist: true, shell_contain: false, rtk: true },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: Default::default(), models: None, limits: Default::default(),
            security: Default::default(), diff: Default::default(), filetree: Default::default(),
            snapshots: Default::default(), code_graph: Default::default(),
            tests: Default::default(), traces: Default::default(),
            dedup: Default::default(), evidence: Default::default(),
            plugins: Default::default(), diag: Default::default(),
            second_opinion: Default::default(),
        }
    }

    /// `ItsyApi::new` constructs with default options.
    #[test]
    fn new_uses_default_options() {
        let api = ItsyApi::new(min_config());
        // History starts empty.
        assert!(api.history.is_empty());
        // Default tools whitelist is None (all tools allowed).
        assert!(api.options.tools.is_none());
        // Profile resolves to something (even if no match → defaults).
        let _ = api.profile();
    }

    /// `RunResult` JSON serialisation uses camelCase keys.
    /// Anti-regression: JS callers depend on this exact shape.
    #[test]
    fn run_result_serialises_camel_case() {
        let r = RunResult {
            response: "hi".into(),
            tool_calls: vec![],
            files_created: vec!["a.rs".into()],
            files_modified: vec!["b.rs".into()],
            tokens_used: TokenUsage { input: 10, output: 5, total: 15 },
            elapsed_ms: 123,
            success: true,
            error: None,
        };
        let j: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(j["response"], "hi");
        assert!(j.get("toolCalls").is_some(), "JSON key must be toolCalls; got keys {:?}",
            j.as_object().unwrap().keys().collect::<Vec<_>>());
        assert!(j.get("filesCreated").is_some());
        assert!(j.get("filesModified").is_some());
        assert!(j.get("tokensUsed").is_some());
        assert!(j.get("elapsedMs").is_some());
        // Optional error is omitted when None.
        assert!(j.get("error").is_none(), "error must be omitted when None");
    }

    /// `RunResult` includes `error` when present.
    #[test]
    fn run_result_includes_error_when_some() {
        let r = RunResult {
            response: String::new(), tool_calls: vec![],
            files_created: vec![], files_modified: vec![],
            tokens_used: TokenUsage::default(), elapsed_ms: 0,
            success: false,
            error: Some("timeout".into()),
        };
        let j: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(j["error"], "timeout");
        assert_eq!(j["success"], false);
    }

    /// `TokenUsage` JSON serialisation uses camelCase + flat fields.
    #[test]
    fn token_usage_serialises_camel_case() {
        let t = TokenUsage { input: 100, output: 50, total: 150 };
        let j: Value = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(j["input"], 100);
        assert_eq!(j["output"], 50);
        assert_eq!(j["total"], 150);
    }

    /// `AgentEvent` is tagged on the `type` field with snake_case variants.
    /// Anti-regression: external subscribers branch on this string.
    #[test]
    fn agent_event_is_externally_tagged() {
        let ev = AgentEvent::ToolStart { name: "bash".into(), args: serde_json::json!({"command":"ls"}) };
        let j: Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(j["type"], "tool_start");
        assert_eq!(j["name"], "bash");

        let ev = AgentEvent::Token { chunk: "hello".into() };
        let j: Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(j["type"], "token");
        assert_eq!(j["chunk"], "hello");

        let ev = AgentEvent::Error { message: "boom".into() };
        let j: Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(j["type"], "error");
        assert_eq!(j["message"], "boom");
    }

    /// `subscribe` returns a Receiver that gets events emitted via `events.send`.
    #[tokio::test]
    async fn subscribers_receive_emitted_events() {
        let api = ItsyApi::new(min_config());
        let mut rx = api.subscribe();
        // Emit via private `emit` — but it's pub(crate), so we send through events directly.
        let _ = api.events.send(AgentEvent::Token { chunk: "tick".into() });
        let received = rx.recv().await.expect("receiver must get event");
        if let AgentEvent::Token { chunk } = received {
            assert_eq!(chunk, "tick");
        } else {
            panic!("wrong event variant");
        }
    }

    /// Two subscribers both see the same event (broadcast semantics).
    #[tokio::test]
    async fn broadcast_to_multiple_subscribers() {
        let api = ItsyApi::new(min_config());
        let mut a = api.subscribe();
        let mut b = api.subscribe();
        let _ = api.events.send(AgentEvent::Token { chunk: "x".into() });
        let ra = a.recv().await.unwrap();
        let rb = b.recv().await.unwrap();
        assert!(matches!(ra, AgentEvent::Token { .. }));
        assert!(matches!(rb, AgentEvent::Token { .. }));
    }

    /// Default RunOptions uses the current cwd, no tool whitelist, and a
    /// reasonable timeout.
    #[test]
    fn run_options_default_sane() {
        let opts = RunOptions::default();
        assert!(opts.tools.is_none(), "default allows all tools");
        assert!(opts.timeout > Duration::from_millis(0), "timeout must be positive");
        assert!(opts.cwd.exists() || opts.cwd == PathBuf::from("."),
            "cwd must be reachable; got {:?}", opts.cwd);
    }

    /// `with_options` retains the user-supplied RunOptions.
    #[test]
    fn with_options_retains_user_options() {
        let mut whitelist = HashSet::new();
        whitelist.insert("read_file".to_string());
        let opts = RunOptions {
            timeout: Duration::from_secs(10),
            tools: Some(whitelist.clone()),
            verbose: true,
            cwd: PathBuf::from("/tmp"),
        };
        let api = ItsyApi::with_options(min_config(), opts);
        assert_eq!(api.options.timeout, Duration::from_secs(10));
        assert_eq!(api.options.tools.as_ref().unwrap(), &whitelist);
        assert!(api.options.verbose);
    }
}
