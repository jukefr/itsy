//! Programmatic Rust API for embedding itsy
//! into other apps.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

use crate::executor::{execute_tool, ExecCtx};
use crate::governor::early_stop::EarlyStopDetector;
use crate::memory::MemoryStore;
use crate::model::profiles::{get_profile, EffectiveProfile};
use crate::model_client::{chat_completion, build_system_prompt, ChatContext};
use crate::tools::{get_all_tools, ToolDeps};
use crate::Config;

pub struct ItsyApi {
    pub config: Config,
    pub early_stop: EarlyStopDetector,
    pub profile: EffectiveProfile,
    pub memory: Arc<Mutex<MemoryStore>>,
    pub history: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub response: String,
    pub tool_calls: Vec<Value>,
    pub files_created: Vec<String>,
    pub files_modified: Vec<String>,
    pub elapsed_ms: u128,
}

impl ItsyApi {
    pub fn new(config: Config) -> Self {
        let profile = get_profile(&config.model.name, config.context.detected_window);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            config,
            profile,
            early_stop: EarlyStopDetector::new(),
            memory: Arc::new(Mutex::new(MemoryStore::new(&cwd))),
            history: Vec::new(),
        }
    }

    pub async fn run(&mut self, prompt: &str) -> Result<RunResult> {
        let start = std::time::Instant::now();
        self.early_stop.new_turn();
        self.history.push(serde_json::json!({"role": "user", "content": prompt}));
        let mut files_created = Vec::new();
        let mut files_modified = Vec::new();
        let mut tool_calls_out = Vec::new();
        let flags = crate::config::Flags::default();
        let ctx = ExecCtx {
            config: &self.config,
            flags: &flags,
            memory: self.memory.clone(),
            mcp_bridge: None,
            mcp_client: None,
            fullscreen: None,
        };

        for _ in 0..self.config.escalation.max_per_session.max(20) {
            let tools = get_all_tools(&self.config, None, &ToolDeps::default());
            let chat_ctx = ChatContext {
                config: &self.config,
                conversation: &self.history,
                tools,
                current_task_type: None,
                system_prompt: build_system_prompt(&self.config, "", "", "", None),
            };
            let Some(data) = chat_completion(&chat_ctx).await else { break };
            let Some(msg) = data.pointer("/choices/0/message") else { break };
            self.history.push(msg.clone());
            let tool_calls = msg.get("tool_calls").and_then(|v| v.as_array()).cloned();
            if let Some(calls) = tool_calls {
                if calls.is_empty() {
                    break;
                }
                for tc in calls {
                    tool_calls_out.push(tc.clone());
                    let name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args_str = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                    let args: Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                    let result = execute_tool(&name, args.clone(), &ctx).await;
                    if let Some(action) = result.get("action").and_then(|a| a.as_str()) {
                        if let Some(path) = result.get("path").and_then(|p| p.as_str()) {
                            match action {
                                "Created" => files_created.push(path.into()),
                                "Updated" | "Edited" => files_modified.push(path.into()),
                                _ => {}
                            }
                        }
                    }
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    self.history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": result.to_string(),
                    }));
                }
                continue;
            }
            break;
        }

        let response = self
            .history
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .and_then(|m| m.get("content").cloned())
            .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
            .unwrap_or_default();

        Ok(RunResult {
            response,
            tool_calls: tool_calls_out,
            files_created,
            files_modified,
            elapsed_ms: start.elapsed().as_millis(),
        })
    }
}
