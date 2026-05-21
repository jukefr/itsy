//! In-memory prompt cache with the
//! same `derive_key`/`get`/`put` contract. The Postgres backend in the JS
//! version is omitted — this binary does not depend on Postgres.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub value: serde_json::Value,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cached_at: u128,
    pub expires_at: u128,
}

#[derive(Debug, Clone)]
pub struct DeriveKeyArgs<'a> {
    pub prompt_name: &'a str,
    pub model_id: &'a str,
    pub template_hash: &'a str,
    pub output_type: &'a str,
    pub input: &'a serde_json::Value,
}

fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        serde_json::Value::Array(arr) => {
            let inner = arr.iter().map(canonical).collect::<Vec<_>>().join(",");
            format!("[{inner}]")
        }
        serde_json::Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            let inner = keys
                .iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical(&obj[*k])))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

pub fn derive_key(args: DeriveKeyArgs<'_>) -> String {
    let composite = [
        args.prompt_name,
        args.model_id,
        args.template_hash,
        args.output_type,
        &canonical(args.input),
    ]
    .join("|");
    let mut h = Sha256::new();
    h.update(composite.as_bytes());
    format!("{:x}", h.finalize())
}

#[derive(Debug)]
pub struct PromptCache {
    store: Mutex<HashMap<String, CacheEntry>>,
}

impl PromptCache {
    pub fn new() -> Self {
        Self { store: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        let now = now_ms();
        let mut g = self.store.lock();
        if let Some(entry) = g.get(key) {
            if entry.expires_at >= now {
                return Some(entry.clone());
            }
            g.remove(key);
        }
        None
    }

    pub fn put(
        &self,
        key: &str,
        value: serde_json::Value,
        ttl_ms: u128,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        cost_usd: Option<f64>,
    ) {
        let now = now_ms();
        self.store.lock().insert(
            key.to_string(),
            CacheEntry {
                value,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                cached_at: now,
                expires_at: now + ttl_ms,
            },
        );
    }

    pub fn clear(&self) {
        self.store.lock().clear();
    }
}

impl Default for PromptCache {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
