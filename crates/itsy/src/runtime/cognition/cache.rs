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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key_args<'a>(input: &'a serde_json::Value) -> DeriveKeyArgs<'a> {
        DeriveKeyArgs {
            prompt_name: "test",
            model_id: "m",
            template_hash: "h",
            output_type: "json",
            input,
        }
    }

    /// `derive_key` is deterministic on identical input.
    #[test]
    fn derive_key_is_deterministic() {
        let input = json!({"a": 1, "b": "x"});
        let k1 = derive_key(key_args(&input));
        let k2 = derive_key(key_args(&input));
        assert_eq!(k1, k2);
    }

    /// `derive_key` is order-independent in object keys — critical invariant
    /// so that `{"a":1,"b":2}` and `{"b":2,"a":1}` produce the same cache key
    /// even though serde may serialize them in insertion order.
    #[test]
    fn derive_key_normalizes_object_key_order() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(derive_key(key_args(&a)), derive_key(key_args(&b)),
            "object key order must not affect derived key");
    }

    /// `derive_key` differs when any component differs.
    #[test]
    fn derive_key_changes_with_input() {
        let a = json!({"x": 1});
        let b = json!({"x": 2});
        assert_ne!(derive_key(key_args(&a)), derive_key(key_args(&b)),
            "different inputs must yield different keys");
    }

    /// `derive_key` is sensitive to `model_id` (so two models don't share cache).
    #[test]
    fn derive_key_changes_with_model_id() {
        let input = json!({"q": "hi"});
        let mut a = key_args(&input);
        let mut b = key_args(&input);
        a.model_id = "model-a";
        b.model_id = "model-b";
        assert_ne!(derive_key(a), derive_key(b),
            "different model_id must yield different keys");
    }

    /// `put` then `get` returns the same value when not yet expired.
    #[test]
    fn put_then_get_returns_value_when_unexpired() {
        let c = PromptCache::new();
        c.put("k", json!(42), 60_000, None, None, None);
        let entry = c.get("k").expect("cache miss for unexpired entry");
        assert_eq!(entry.value, json!(42));
    }

    /// `get` returns None for expired entries and evicts them.
    /// Anti-regression: stale entries must never be returned.
    #[test]
    fn get_returns_none_and_evicts_on_expiry() {
        let c = PromptCache::new();
        // ttl = 0 means already expired (expires_at == now)
        c.put("k", json!("stale"), 0, None, None, None);
        // small sleep to ensure now > expires_at
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(c.get("k").is_none(), "expired entry must not be returned");
        // and the second get should still be None (already evicted)
        assert!(c.get("k").is_none());
    }

    /// `clear` empties the store.
    #[test]
    fn clear_removes_all_entries() {
        let c = PromptCache::new();
        c.put("a", json!(1), 60_000, None, None, None);
        c.put("b", json!(2), 60_000, None, None, None);
        c.clear();
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_none());
    }

    /// `canonical` produces stable representations across nested orderings.
    #[test]
    fn canonical_normalizes_nested_objects() {
        let a = canonical(&json!({"outer": {"a": 1, "b": [3, 2, 1]}}));
        let b = canonical(&json!({"outer": {"b": [3, 2, 1], "a": 1}}));
        assert_eq!(a, b, "canonical form must be stable across key-order");
    }
}
