//! Structured logger — implements spec §3 (Logging Schema).
//!
//! Logs are only emitted when explicitly enabled, to avoid polluting CLI
//! output:
//!   - `ITSY_COGNITION_LOG=stdout`  — structured JSON to stdout
//!   - `ITSY_COGNITION_LOG=stderr`  — structured JSON to stderr
//!   - unset                        — silent
//!
//! Errors and fatals are also routed to stderr when emitting to stdout (so
//! pipes don't lose error information).
//!
//! The `debug` / `warn` helpers below are also kept as a lightweight stderr
//! tap (gated by `ITSY_DEBUG`) for places in the crate that just need a quick
//! human-readable trace.

use std::env;

use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Map, Value};

fn span_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    format!("{ts:016x}-{n:08x}")
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub level: String,
    pub event: String,
    pub status: String,
    pub trace_id: String,
    pub timestamp: String,
    pub service: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Logger {
    pub service: String,
}

impl Logger {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn emit(&self, level: &str, event: &str, status: &str, fields: Option<&Value>) {
        let target = env::var("ITSY_COGNITION_LOG").ok();
        let Some(target) = target.filter(|s| !s.is_empty()) else {
            return;
        };

        let trace_id = fields
            .and_then(|f| f.get("trace_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let metadata = fields
            .and_then(|f| f.get("metadata"))
            .cloned();

        let entry = LogEntry {
            level: level.to_string(),
            event: event.to_string(),
            status: status.to_string(),
            trace_id,
            timestamp: Utc::now().to_rfc3339(),
            service: self.service.clone(),
            span_id: span_id(),
            metadata,
        };

        let mut payload = serde_json::to_value(&entry).unwrap_or(Value::Null);
        // Splat any additional fields (e.g. event-specific data) onto the top
        // level, matching the JS behaviour.
        if let (Some(extra), Value::Object(out)) = (fields, &mut payload) {
            if let Some(extra_obj) = extra.as_object() {
                merge_fields(out, extra_obj);
            }
        }
        let line = serde_json::to_string(&payload).unwrap_or_default();
        match target.as_str() {
            "stderr" => eprintln!("{line}"),
            _ => {
                if level == "error" || level == "fatal" {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
        }
    }

    pub fn info(&self, event: &str, fields: Option<&Value>) {
        self.emit("info", event, "success", fields);
    }

    pub fn warn(&self, event: &str, fields: Option<&Value>) {
        self.emit("warn", event, "rejected", fields);
    }

    pub fn error(&self, event: &str, fields: Option<&Value>) {
        self.emit("error", event, "failure", fields);
    }

    pub fn debug(&self, event: &str, fields: Option<&Value>) {
        if env::var("LOG_LEVEL").ok().as_deref() == Some("debug") {
            self.emit("debug", event, "success", fields);
        }
    }
}

fn merge_fields(out: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (k, v) in extra {
        // The reserved keys are populated by `emit` itself; let the explicit
        // field override them.
        out.insert(k.clone(), v.clone());
    }
}

/// Build a fresh logger with the given service tag.
pub fn create_logger(service: impl Into<String>) -> Logger {
    Logger::new(service)
}

/// The global, default logger — equivalent to `exports.logger` in the JS
/// version (service name: `ItsyCognition`).
pub fn logger() -> Logger {
    Logger::new("ItsyCognition")
}

// ---------------------------------------------------------------------------
// Free-function shims used inside the crate (`runtime::logger::info(...)`)
// ---------------------------------------------------------------------------

pub fn info(event: &str, area: &str, error: Option<&str>) {
    let fields = build_fields(area, error);
    logger().info(event, Some(&fields));
}

pub fn warn(event: &str, area: &str, error: Option<&str>) {
    let fields = build_fields(area, error);
    logger().warn(event, Some(&fields));
    if env::var("ITSY_DEBUG").is_ok() {
        eprintln!("[{area}] warn: {}", error.unwrap_or(""));
    }
}

pub fn error(event: &str, area: &str, error: Option<&str>) {
    let fields = build_fields(area, error);
    logger().error(event, Some(&fields));
}

fn build_fields(area: &str, error: Option<&str>) -> Value {
    let mut metadata = Map::new();
    if let Some(e) = error {
        metadata.insert("error".into(), Value::String(e.to_string()));
    }
    json!({
        "event_area": area,
        "metadata": Value::Object(metadata),
    })
}

// ---------------------------------------------------------------------------
// Existing minimal stderr helpers, preserved for current callers.
// ---------------------------------------------------------------------------

pub fn debug(area: &str, msg: &str) {
    if env::var("ITSY_DEBUG").is_ok() {
        eprintln!("[{area}] {msg}");
    }
}
