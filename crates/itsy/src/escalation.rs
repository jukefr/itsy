//! Cloud-model fallback engine.

use std::env;

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    pub key: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_key: &'static str,
    pub default_model: &'static str,
}

pub const ESCALATION_PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        key: "anthropic",
        name: "Claude",
        base_url: "https://api.anthropic.com/v1",
        env_key: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-5",
    },
    ProviderInfo {
        key: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        env_key: "OPENAI_API_KEY",
        default_model: "gpt-5.4-mini",
    },
    ProviderInfo {
        key: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        env_key: "DEEPSEEK_API_KEY",
        default_model: "deepseek-v4",
    },
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationOptions {
    pub max_per_session: u32,
    pub confirm: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
}

impl Default for EscalationOptions {
    fn default() -> Self {
        Self {
            max_per_session: 5,
            confirm: true,
            provider: None,
            api_key: None,
            model: None,
            base_url: None,
        }
    }
}

pub struct EscalationEngine {
    pub enabled: bool,
    pub provider: Option<&'static ProviderInfo>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_per_session: u32,
    pub count: u32,
    pub confirm: bool,
    client: reqwest::Client,
}

impl EscalationEngine {
    pub fn new(options: EscalationOptions) -> Self {
        let mut engine = Self {
            enabled: false,
            provider: None,
            api_key: None,
            model: None,
            base_url: None,
            max_per_session: options.max_per_session,
            count: 0,
            confirm: options.confirm,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("build reqwest client"),
        };
        engine.detect_config(&options);
        engine
    }

    fn detect_config(&mut self, options: &EscalationOptions) {
        if let (Some(p), Some(k)) = (&options.provider, &options.api_key) {
            if let Some(info) = ESCALATION_PROVIDERS.iter().find(|pi| pi.key == p) {
                self.enabled = true;
                self.provider = Some(info);
                self.api_key = Some(k.clone());
                self.model = options.model.clone().or(Some(info.default_model.to_string()));
                self.base_url = options.base_url.clone().or(Some(info.base_url.to_string()));
                return;
            }
        }
        for info in ESCALATION_PROVIDERS {
            if let Ok(key) = env::var(info.env_key) {
                if !key.is_empty() {
                    self.enabled = true;
                    self.provider = Some(info);
                    self.api_key = Some(key);
                    self.model = options.model.clone().or(Some(info.default_model.to_string()));
                    self.base_url = options.base_url.clone().or(Some(info.base_url.to_string()));
                    return;
                }
            }
        }
    }

    pub fn can_escalate(&self) -> bool {
        self.enabled && self.count < self.max_per_session
    }

    pub fn status(&self) -> String {
        if !self.enabled {
            return "disabled (no API key configured)".into();
        }
        let provider = self.provider.map(|p| p.name).unwrap_or("?");
        let model = self.model.as_deref().unwrap_or("?");
        format!("{provider} ({model}) — {}/{} used", self.count, self.max_per_session)
    }

    pub async fn escalate(
        &mut self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        system_extra: &str,
    ) -> Result<Option<Value>> {
        if !self.can_escalate() {
            return Ok(None);
        }
        self.count += 1;
        let system = format!(
            "You are an expert coding assistant called in as escalation support.\n\
             A smaller local model attempted this task but failed after multiple retry and decompose attempts.\n\
             Your job: solve it correctly in as few tool calls as possible.\n\
             Be precise. Don't explain unnecessarily. Just fix it.\n{system_extra}"
        );
        let provider_key = self.provider.map(|p| p.key).unwrap_or("openai");
        match provider_key {
            "anthropic" => Ok(self.call_anthropic(&system, messages, tools).await?),
            _ => Ok(self.call_openai_compat(&system, messages, tools).await?),
        }
    }

    async fn call_openai_compat(
        &self,
        system: &str,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<Option<Value>> {
        let mut all = vec![json!({"role": "system", "content": system})];
        all.extend(messages);
        let mut body = json!({
            "model": self.model.as_deref().unwrap_or(""),
            "messages": all,
            "temperature": 0.1,
            "max_tokens": 4096,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let url = format!("{}/chat/completions", self.base_url.as_deref().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(k) = &self.api_key {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
                headers.insert(AUTHORIZATION, v);
            }
        }
        let res = self.client.post(&url).headers(headers).json(&body).send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            return Ok(Some(json!({ "error": format!("Escalation API error {status}: {}", &text[..text.len().min(200)]) })));
        }
        let data: Value = res.json().await?;
        Ok(data.pointer("/choices/0/message").cloned())
    }

    async fn call_anthropic(
        &self,
        system: &str,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<Option<Value>> {
        let non_system: Vec<Value> = messages
            .into_iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))
            .collect();
        let raw_messages: Vec<Value> = non_system
            .into_iter()
            .map(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                if role == "tool" {
                    json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
                            "content": m.get("content").cloned().unwrap_or(Value::Null),
                        }]
                    })
                } else if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    let content: Vec<Value> = tcs
                        .iter()
                        .map(|tc| {
                            let name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let args_str = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                            let parsed: Value = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
                            json!({ "type": "tool_use", "id": id, "name": name, "input": parsed })
                        })
                        .collect();
                    json!({ "role": "assistant", "content": content })
                } else {
                    json!({ "role": role, "content": m.get("content").cloned().unwrap_or(Value::Null) })
                }
            })
            .collect();

        let mut anthropic_messages: Vec<Value> = Vec::new();
        for msg in raw_messages {
            if let Some(prev) = anthropic_messages.last_mut() {
                if prev.get("role") == msg.get("role") {
                    merge_into(prev, &msg);
                    continue;
                }
            }
            anthropic_messages.push(msg);
        }
        if let Some(first) = anthropic_messages.first() {
            if first.get("role").and_then(|r| r.as_str()) != Some("user") {
                anthropic_messages.insert(0, json!({"role": "user", "content": "(continuing from earlier context)"}));
            }
        }

        let anthropic_tools: Vec<Value> = tools
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.pointer("/function/name").cloned().unwrap_or(Value::Null),
                    "description": t.pointer("/function/description").cloned().unwrap_or(Value::Null),
                    "input_schema": t.pointer("/function/parameters").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model.as_deref().unwrap_or(""),
            "max_tokens": 4096,
            "system": system,
            "messages": anthropic_messages,
        });
        if !anthropic_tools.is_empty() {
            body["tools"] = json!(anthropic_tools);
        }

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(k) = &self.api_key {
            if let Ok(v) = HeaderValue::from_str(k) {
                headers.insert("x-api-key", v);
            }
        }
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

        let res = self.client
            .post("https://api.anthropic.com/v1/messages")
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            return Ok(Some(json!({ "error": format!("Anthropic error {status}: {}", &text[..text.len().min(200)]) })));
        }

        let data: Value = res.json().await?;
        let content = data.get("content").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let text_blocks: Vec<&Value> = content.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")).collect();
        let tool_blocks: Vec<&Value> = content.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use")).collect();

        if !tool_blocks.is_empty() {
            let combined_text: String = text_blocks.iter().filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(String::from)).collect();
            let tool_calls: Vec<Value> = tool_blocks.iter().map(|b| {
                let id = b.get("id").cloned().unwrap_or(Value::Null);
                let name = b.get("name").cloned().unwrap_or(Value::Null);
                let input = b.get("input").cloned().unwrap_or(json!({}));
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": input.to_string() }
                })
            }).collect();
            Ok(Some(json!({
                "role": "assistant",
                "content": if combined_text.is_empty() { Value::Null } else { Value::String(combined_text) },
                "tool_calls": tool_calls,
            })))
        } else {
            let combined: String = text_blocks.iter().filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(String::from)).collect();
            Ok(Some(json!({
                "role": "assistant",
                "content": combined,
            })))
        }
    }
}

fn merge_into(prev: &mut Value, msg: &Value) {
    let prev_content = prev.get("content").cloned().unwrap_or(Value::Null);
    let msg_content = msg.get("content").cloned().unwrap_or(Value::Null);
    let merged = match (prev_content, msg_content) {
        (Value::String(a), Value::String(b)) => Value::String(format!("{a}\n\n{b}")),
        (Value::Array(mut a), Value::Array(b)) => {
            a.extend(b);
            Value::Array(a)
        }
        (Value::String(a), Value::Array(b)) => {
            let mut out = vec![json!({"type": "text", "text": a})];
            out.extend(b);
            Value::Array(out)
        }
        (Value::Array(mut a), Value::String(b)) => {
            a.push(json!({"type": "text", "text": b}));
            Value::Array(a)
        }
        (a, _) => a,
    };
    if let Some(obj) = prev.as_object_mut() {
        obj.insert("content".into(), merged);
    }
}
