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

/// One-shot chat against an arbitrary OpenAI-compatible endpoint with custom
/// (model, endpoint, thinking budget). Built for the third-evaluator path in
/// `bin/itsy.rs::review_verdict`, where the second-opinion model is not the
/// configured main model and the main [`crate::model_client`] would be the
/// wrong shape.
///
/// Why the explicit `thinking_budget`: reasoning-enabled models
/// (Qwen3, Gemma3+, DeepSeek-R1, …) spend `max_tokens` on `reasoning_content`
/// first, then on output. With `max_tokens=1024` the whole budget gets eaten
/// by thinking and the content field comes back empty — that's the bug we hit
/// when the assertion-negotiation second-opinion call always returned `""`.
/// Default headroom: `2048` for thinking + `4096` for the actual response.
/// Sends `chat_template_kwargs` and the Anthropic-shape `thinking` knob too
/// so llama-server-backed reasoning models pick it up correctly.
pub async fn chat_oneshot(
    endpoint: &str,
    model: &str,
    prompt: &str,
    thinking_budget: Option<u32>,
    timeout_secs: u64,
) -> Result<String> {
    assert_endpoint_allowed(endpoint).map_err(|e| anyhow!(e))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))
        .or_else(|_| env::var("DEEPSEEK_API_KEY"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    let thinking_budget = thinking_budget.unwrap_or(2048);
    let max_tokens = thinking_budget + 4096;
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.2,
        "max_tokens": max_tokens,
        "enable_thinking": true,
        "thinking_budget": thinking_budget,
        "chat_template_kwargs": {
            "enable_thinking": true,
            "thinking_budget": thinking_budget,
        },
        "thinking": {"type": "enabled", "budget_tokens": thinking_budget},
    });
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
    let res = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        return Err(anyhow!("API {status}: {}", &text[..text.len().min(200)]));
    }
    let data: Value = res.json().await?;
    data.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("empty response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{ChatMessage, ChatRequest};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::json!("hi"),
                name: None, tool_call_id: None, tool_calls: None,
            }],
            temperature: Some(0.0),
            top_p: None,
            max_output: None,
            stop: None,
            json: false,
            tools: None,
            tool_choice: None,
        }
    }

    /// Constructor rejects loopback by default (SSRF gate).
    #[test]
    fn new_refuses_loopback_without_env_flag() {
        if std::env::var("LLM_ALLOW_PUBLIC_ENDPOINTS").ok().as_deref() == Some("1") {
            return; // env unlocks loopback; skip.
        }
        // SSRF default allows loopback/RFC1918 (assert_endpoint_allowed flow).
        // What it RAW-rejects is metadata. Use that.
        let r = OpenAICompatProvider::new("http://169.254.169.254/v1", None);
        assert!(r.is_err(), "metadata endpoint must always be refused");
    }

    /// Constructor strips trailing slash from endpoint.
    #[tokio::test]
    async fn new_strips_trailing_slash() {
        let server = MockServer::start().await;
        // server.uri() already lacks trailing slash; add one explicitly.
        let endpoint_with_slash = format!("{}/", server.uri());
        let p = OpenAICompatProvider::new(&endpoint_with_slash, None).unwrap();
        assert!(!p.endpoint.ends_with('/'),
            "endpoint must be normalized; got {}", p.endpoint);
    }

    /// 200 response is parsed: content, usage, tool_calls all populated.
    #[tokio::test]
    async fn chat_parses_successful_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!",
                        "tool_calls": [{
                            "id": "call_123",
                            "type": "function",
                            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 50, "completion_tokens": 10}
            })))
            .mount(&server).await;

        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let resp = p.chat(&req("test-model")).await.unwrap();
        assert_eq!(resp.content, "Hello!");
        assert_eq!(resp.usage.prompt_tokens, Some(50));
        assert_eq!(resp.usage.completion_tokens, Some(10));
        let calls = resp.tool_calls.expect("tool_calls must be populated");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_123");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
    }

    /// Empty tool_calls array filters to None (not Some(vec![])).
    /// Anti-regression: downstream branches on `Some(_)` vs `None`.
    #[tokio::test]
    async fn chat_filters_empty_tool_calls_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "no tools", "tool_calls": []},
                    "finish_reason": "stop"
                }]
            })))
            .mount(&server).await;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let resp = p.chat(&req("test")).await.unwrap();
        assert!(resp.tool_calls.is_none(),
            "empty tool_calls array must filter to None, got {:?}", resp.tool_calls);
    }

    /// 4xx returns Err with status + body in message.
    #[tokio::test]
    async fn chat_returns_err_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("malformed request"))
            .mount(&server).await;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let err = p.chat(&req("test")).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"), "must include status code; got {msg}");
        assert!(msg.contains("malformed request"), "must include body; got {msg}");
    }

    /// 5xx returns Err.
    #[tokio::test]
    async fn chat_returns_err_on_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server).await;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        assert!(p.chat(&req("test")).await.is_err());
    }

    /// `tools` are included in the request body when ChatRequest specifies them.
    #[tokio::test]
    async fn chat_serialises_tools_into_request() {
        use super::super::types::ToolSpec;
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        let mut r = req("test");
        r.tools = Some(vec![ToolSpec {
            name: "bash".into(), description: "run".into(),
            parameters: json!({"type":"object"}),
        }]);
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let _ = p.chat(&r).await;

        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        let tools = body["tools"].as_array().expect("tools must be present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
    }

    /// When `json=true` AND no tools, `response_format` is set to json_object.
    /// When `json=true` AND tools present, `response_format` is removed
    /// (anti-regression: json mode conflicts with tool mode on most providers).
    #[tokio::test]
    async fn json_mode_and_tools_are_mutually_exclusive() {
        use super::super::types::ToolSpec;
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]
            })))
            .mount(&server).await;

        // (a) json=true, no tools → response_format=json_object set.
        let mut r = req("test");
        r.json = true;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let _ = p.chat(&r).await;
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["response_format"]["type"], "json_object",
            "json=true without tools must set response_format; body={body}");

        // (b) json=true, tools present → response_format REMOVED.
        let server2 = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]
            })))
            .mount(&server2).await;
        r.tools = Some(vec![ToolSpec {
            name: "bash".into(), description: "".into(),
            parameters: json!({"type":"object"}),
        }]);
        let p2 = OpenAICompatProvider::new(&server2.uri(), None).unwrap();
        let _ = p2.chat(&r).await;
        let received2 = server2.received_requests().await.unwrap();
        let body2: Value = serde_json::from_slice(&received2[0].body).unwrap();
        assert!(body2.get("response_format").is_none(),
            "json + tools must REMOVE response_format; body={body2}");
    }

    /// Confidence is computed from logprobs when present.
    #[tokio::test]
    async fn confidence_derived_from_logprobs() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{
                    "index":0,
                    "message":{"role":"assistant","content":"hi"},
                    "finish_reason":"stop",
                    "logprobs": {
                        "content": [
                            {"token": "h", "logprob": -0.1},
                            {"token": "i", "logprob": -0.1}
                        ]
                    }
                }]
            })))
            .mount(&server).await;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let resp = p.chat(&req("test")).await.unwrap();
        let c = resp.confidence.expect("confidence must be set from logprobs");
        // exp(-0.1) ≈ 0.905 — in [0,1] range.
        assert!(c > 0.5 && c <= 1.0, "confidence should be in (0.5,1.0]; got {c}");
    }

    /// Missing logprobs → confidence is None (don't fabricate).
    #[tokio::test]
    async fn confidence_is_none_when_no_logprobs() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]
            })))
            .mount(&server).await;
        let p = OpenAICompatProvider::new(&server.uri(), None).unwrap();
        let resp = p.chat(&req("test")).await.unwrap();
        assert!(resp.confidence.is_none());
    }
}
