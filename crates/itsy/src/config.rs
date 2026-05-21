//! Loads configuration from environment variables,
//! `itsy.toml`, and CLI flags. CLI flags win over toml; env wins over both
//! except for the `model.name` field, which the toml is allowed to populate.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

/// CLI flags that may override config values.
#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub base_url: Option<String>,
    pub classic: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    #[serde(rename = "baseUrl", alias = "base_url")]
    pub base_url: String,
    pub timeout: u64,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub max_budget_pct: u32,
    pub detected_window: u32,
    pub working_memory_tokens: u32,
    pub summary_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub bash_timeout: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub show_token_usage: bool,
    pub auto_approve: bool,
    pub theme: String,
    #[serde(default)]
    pub classic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationConfig {
    pub enabled: bool,
    pub max_per_session: u32,
    pub confirm: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub auto_commit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiModels {
    pub fast: String,
    pub default: String,
    pub strong: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub tui: TuiConfig,
    pub escalation: EscalationConfig,
    pub git: GitConfig,
    #[serde(default)]
    pub models: Option<MultiModels>,
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_str(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

/// Walk the canonical `.env` search paths and inject values into the process
/// environment. Matches the JS version's "first found wins, never override
/// existing env" semantics.
pub fn load_dotenv() {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let paths = [
        cwd.join(".env"),
        cwd.join(".itsy").join(".env"),
        home.join(".config").join("itsy").join(".env"),
        home.join(".itsy").join(".env"),
    ];
    for p in &paths {
        if !p.exists() {
            continue;
        }
        let Ok(content) = fs::read_to_string(p) else { continue };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some(eq) = trimmed.find('=') else { continue };
            let key = trimmed[..eq].trim();
            let mut value = trimmed[eq + 1..].trim().to_string();
            let bytes = value.as_bytes();
            if value.len() >= 2
                && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                    || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
            {
                value = value[1..value.len() - 1].to_string();
            }
            if env::var(key).is_err() {
                // SAFETY: process is still single-threaded during startup.
                unsafe { env::set_var(key, value) };
            }
        }
        break;
    }
}

pub fn load_config(flags: &Flags) -> Config {
    let base_url_default = if let Some(host) = env_str("OLLAMA_HOST") {
        format!("{host}/v1")
    } else {
        "http://localhost:1234/v1".to_string()
    };

    let mut config = Config {
        model: ModelConfig {
            provider: env_str("ITSY_PROVIDER").unwrap_or_else(|| "openai".into()),
            name: env_str("ITSY_MODEL").unwrap_or_default(),
            base_url: env_str("ITSY_BASE_URL").unwrap_or(base_url_default),
            timeout: env_or("ITSY_MODEL_TIMEOUT", 300u64),
            api_key: None,
        },
        context: ContextConfig {
            max_budget_pct: env_or("ITSY_CONTEXT_BUDGET", 70),
            detected_window: env_or("ITSY_CONTEXT_WINDOW", 128_000),
            working_memory_tokens: 500,
            summary_threshold: 200,
        },
        tools: ToolsConfig {
            bash_timeout: env_or("ITSY_BASH_TIMEOUT", 30),
        },
        tui: TuiConfig {
            show_token_usage: true,
            auto_approve: env_str("ITSY_AUTO_APPROVE").as_deref() == Some("true"),
            theme: env_str("ITSY_THEME").unwrap_or_else(|| "dark".into()),
            classic: false,
        },
        escalation: EscalationConfig {
            enabled: true,
            max_per_session: env_or("ITSY_ESCALATION_MAX", 5),
            confirm: env_str("ITSY_ESCALATION_CONFIRM").as_deref() != Some("false"),
            provider: None,
            api_key: None,
            model: env_str("ITSY_ESCALATION_MODEL"),
        },
        git: GitConfig {
            auto_commit: env_str("ITSY_AUTO_COMMIT").as_deref() == Some("true"),
        },
        models: None,
    };

    if env_str("ITSY_MODEL_FAST").is_some() || env_str("ITSY_MODEL_STRONG").is_some() {
        config.models = Some(MultiModels {
            fast: env_str("ITSY_MODEL_FAST").unwrap_or_else(|| config.model.name.clone()),
            default: env_str("ITSY_MODEL_DEFAULT").unwrap_or_else(|| config.model.name.clone()),
            strong: env_str("ITSY_MODEL_STRONG").unwrap_or_else(|| config.model.name.clone()),
        });
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let toml_paths = [
        cwd.join("itsy.toml"),
        cwd.join(".itsy").join("config.toml"),
        home.join(".config").join("itsy").join("config.toml"),
    ];
    for p in &toml_paths {
        if p.exists() && config.model.name.is_empty() {
            if let Ok(content) = fs::read_to_string(p) {
                for line in content.lines() {
                    if let Some(m) = line.strip_prefix("name") {
                        if let Some(v) = extract_value(m) {
                            config.model.name = v;
                        }
                    } else if line.starts_with("baseUrl") || line.starts_with("base_url") {
                        if let Some(v) = extract_value(line) {
                            config.model.base_url = v;
                        }
                    } else if line.starts_with("provider") {
                        if let Some(v) = extract_value(line) {
                            config.model.provider = v;
                        }
                    } else if let Some(rest) = line.strip_prefix("timeout") {
                        if let Some(v) = extract_value(rest) {
                            if let Ok(n) = v.parse() {
                                config.model.timeout = n;
                            }
                        }
                    }
                }
                break;
            }
        }
    }

    if let Some(m) = flags.model.clone() {
        config.model.name = m;
    }
    if let Some(p) = flags.provider.clone() {
        config.model.provider = p;
    }
    if let Some(b) = flags.endpoint.clone().or_else(|| flags.base_url.clone()) {
        config.model.base_url = b;
    }
    // Normalise: prepend http:// if the URL lacks a scheme. `reqwest` and our
    // SSRF guard both refuse `host:port/path` URLs without one.
    config.model.base_url = normalize_base_url(&config.model.base_url);
    if flags.classic {
        config.tui.classic = true;
    }

    config
}

/// Prepend `http://` if the URL string lacks a scheme. Trims a trailing
/// slash so `host:port/v1/` and `host:port/v1` are equivalent. Falls back
/// to the input unchanged when it already has a scheme.
pub fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "http://localhost:1234/v1".to_string();
    }
    let has_scheme = trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("unix://");
    if has_scheme {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn extract_value(line: &str) -> Option<String> {
    let after_eq = line.find('=')?;
    let mut v = line[after_eq + 1..].trim();
    if let Some(idx) = v.find('#') {
        v = v[..idx].trim();
    }
    let v = v.trim_matches(|c| c == '"' || c == '\'').trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Build auth headers for API requests (mirrors `buildAuthHeaders`).
pub fn build_auth_headers(config: &Config) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = env_str("OPENAI_API_KEY")
        .or_else(|| env_str("ANTHROPIC_API_KEY"))
        .or_else(|| env_str("DEEPSEEK_API_KEY"))
        .or_else(|| config.model.api_key.clone());
    if let Some(k) = api_key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    if config.model.base_url.contains("openrouter.ai") {
        if let Ok(v) = HeaderValue::from_str("itsy") {
            headers.insert("X-Title", v);
        }
    }
    headers
}

/// Mirror of `checkEndpoint` — prints status, mutates context window if the
/// remote advertises one. Returns whether the endpoint was reachable.
pub async fn check_endpoint(config: &mut Config) -> bool {
    let ollama_host = env_str("OLLAMA_HOST").unwrap_or_else(|| "http://localhost:11434".into());
    let base_url = if !config.model.base_url.is_empty() {
        config.model.base_url.clone()
    } else {
        ollama_host.clone()
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    if config.model.provider == "openai" || base_url.contains("/v1") {
        let mut req = client.get(format!("{base_url}/models"));
        let api_key = env_str("OPENAI_API_KEY")
            .or_else(|| env_str("ANTHROPIC_API_KEY"))
            .or_else(|| env_str("DEEPSEEK_API_KEY"));
        if let Some(k) = api_key {
            req = req.bearer_auth(k);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let models = data.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                    if !models.is_empty() {
                        println!("  Connected: {base_url}");
                        println!("  Model: {}", config.model.name);
                        let want = config.model.name.as_str();
                        let active = models
                            .iter()
                            .find(|m| {
                                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let nm = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                id.contains(want) || nm.contains(want)
                            })
                            .unwrap_or(&models[0]);
                        if let Some(cl) = active.get("context_length").and_then(|v| v.as_u64()) {
                            config.context.detected_window = cl as u32;
                            println!("  Context: {cl} tokens");
                        }
                    }
                }
                true
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                println!("  ⚠ Cannot reach endpoint at {base_url}");
                println!("  Check that your model server is running and accessible.");
                if status == 401 || status == 403 {
                    println!("  Got {status} — set OPENAI_API_KEY in .env if your server requires auth.");
                }
                false
            }
            Err(_) => {
                println!("  ⚠ Cannot reach endpoint at {base_url}");
                println!("  Check that your model server is running and the URL is correct.");
                false
            }
        }
    } else {
        let host = ollama_host;
        match client.get(format!("{host}/api/tags")).send().await {
            Ok(resp) if resp.status().is_success() => {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                let want = config.model.name.split(':').next().unwrap_or("");
                let has_model = data
                    .get("models")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter().any(|m| {
                            m.get("name").and_then(|n| n.as_str()).is_some_and(|n| n.contains(want))
                        })
                    })
                    .unwrap_or(false);
                if !has_model {
                    println!("  ⚠ Model \"{}\" not found in Ollama.", config.model.name);
                    println!("  Run: ollama pull {}", config.model.name);
                    return false;
                }
                true
            }
            _ => {
                println!("  ⚠ Ollama not running. Start it with: ollama serve");
                false
            }
        }
    }
}

/// Resolve which API key, if any, would be used for the current provider.
pub fn resolve_api_key(config: &Config) -> Option<String> {
    env_str("OPENAI_API_KEY")
        .or_else(|| env_str("ANTHROPIC_API_KEY"))
        .or_else(|| env_str("DEEPSEEK_API_KEY"))
        .or_else(|| config.model.api_key.clone())
}

/// Returns a stable map of provider → env-var name for diagnostic UI.
pub fn provider_env_map() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("anthropic", "ANTHROPIC_API_KEY");
    m.insert("openai", "OPENAI_API_KEY");
    m.insert("deepseek", "DEEPSEEK_API_KEY");
    m
}

pub fn deps_ok() -> Result<()> {
    Ok(())
}
