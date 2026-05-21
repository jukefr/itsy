//! OpenAI-compatible HTTP
//! chat provider. Handles tool calls, logprobs-based confidence, and SSRF
//! guarding.

use std::env;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::ssrf_guard::assert_endpoint_allowed;
use super::types::{ChatRequest, ChatResponse, ToolCall, Usage};

pub struct OpenAICompatProvider {
    pub endpoint: String,
    pub api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAICompatProvider {
    pub fn new(endpoint: &str, api_key: Option<String>) -> Result<Self> {
        assert_endpoint_allowed(endpoint).map_err(|e| anyhow!(e))?;
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let api_key = api_key.or_else(|| env::var("OPENAI_COMPAT_API_KEY").ok()).filter(|s| !s.is_empty());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self { endpoint, api_key, client })
    }

    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let mut body = json!({
            "model": req.model,
            "messages": req.messages,
            "temperature": req.temperature.unwrap_or(0.0),
            "stream": false,
        });
        if let Some(p) = req.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(m) = req.max_output {
            body["max_tokens"] = json!(m);
        }
        if let Some(stop) = &req.stop {
            body["stop"] = json!(stop);
        }
        if req.json {
            body["response_format"] = json!({"type": "json_object"});
        }
        if let Some(tools) = &req.tools {
            if !tools.is_empty() {
                body["tools"] = json!(tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    }))
                    .collect::<Vec<_>>());
                if let Some(tc) = &req.tool_choice {
                    body["tool_choice"] = tc.clone();
                }
                if body.get("response_format").is_some() {
                    body.as_object_mut().unwrap().remove("response_format");
                }
            }
        }
        body["logprobs"] = json!(true);
        body["top_logprobs"] = json!(1);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(k) = &self.api_key {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
                headers.insert(AUTHORIZATION, v);
            }
        }

        let url = format!("{}/chat/completions", self.endpoint);
        let res = self.client.post(&url).headers(headers).json(&body).send().await
            .with_context(|| format!("POST {url}"))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            return Err(anyhow!("openai_compat {status}: {}", &text[..text.len().min(256)]));
        }
        let data: Value = res.json().await?;
        let msg = data.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let usage = Usage {
            prompt_tokens: data.pointer("/usage/prompt_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
            completion_tokens: data.pointer("/usage/completion_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
        };

        let tool_calls = msg.get("tool_calls").and_then(|tc| tc.as_array()).map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, tc)| {
                    let func = tc.get("function")?;
                    let name = func.get("name")?.as_str()?.to_string();
                    let id = tc.get("id").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| format!("tool_{i}"));
                    let args = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                    Some(ToolCall { id, name, arguments: args })
                })
                .collect::<Vec<_>>()
        });

        let confidence = data
            .pointer("/choices/0/logprobs/content")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                let mut sum = 0.0f64;
                let mut n = 0u32;
                for tok in arr {
                    if let Some(lp) = tok.get("logprob").and_then(|v| v.as_f64()) {
                        if lp.is_finite() {
                            sum += lp;
                            n += 1;
                        }
                    }
                }
                if n == 0 {
                    None
                } else {
                    let c = (sum / n as f64).exp();
                    if c.is_finite() {
                        Some(c.clamp(0.0, 1.0))
                    } else {
                        None
                    }
                }
            });

        Ok(ChatResponse {
            content,
            usage,
            confidence,
            tool_calls: tool_calls.filter(|v| !v.is_empty()),
            raw: data,
        })
    }
}
