//! Cognition span writer + in-memory trace buffer.
//!
//! Every prompt invocation emits one or more spans through
//! [`write_span`]. Spans are mirrored to two backends:
//!
//! * an in-process ring buffer (used by older callers via
//!   [`TraceBuffer::record`] / [`TraceBuffer::dump`]), and
//! * a JSONL file on disk under `.itsy/traces/spans/<date>.jsonl`,
//!   in OpenTelemetry-style flat records.
//!
//! Persistence failures are logged and swallowed — a broken disk
//! must never take down inference. Set `ITSY_TRACES_DISABLE=1` to
//! turn the writer into a no-op (the in-memory ring buffer still
//! works in case other code wants to inspect the recent stream).

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// One structured cognition span, written as a single JSONL row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub span_id: String,
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub workflow: String,
    pub step: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    pub latency_ms: u64,
    pub status: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// RFC3339 timestamp captured when the span was finalised.
    pub finished_at: String,
}

/// Builder-style initialiser for a span. Mirrors the JS `init` object.
#[derive(Debug, Default)]
pub struct SpanInit {
    pub trace_id: String,
    pub parent_span_id: Option<String>,
    pub workflow: String,
    pub step: String,
    pub kind: String,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub latency_ms: u64,
    pub status: String,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

// ─── Lightweight legacy event (kept for `TraceBuffer::record` callers) ──────

#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    pub trace_id: String,
    pub at: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// In-memory ring buffer of recent events. Callers can dump it for
/// debugging or for the dev TUI. Independent from the on-disk JSONL.
pub struct TraceBuffer {
    events: Mutex<VecDeque<TraceEvent>>,
    capacity: usize,
}

impl TraceBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { events: Mutex::new(VecDeque::with_capacity(capacity)), capacity }
    }

    pub fn record(&self, trace_id: &str, kind: &str, data: serde_json::Value) {
        let mut g = self.events.lock();
        if g.len() >= self.capacity {
            g.pop_front();
        }
        g.push_back(TraceEvent {
            trace_id: trace_id.to_string(),
            at: Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            data,
        });
    }

    pub fn dump(&self) -> Vec<TraceEvent> {
        self.events.lock().iter().cloned().collect()
    }
}

impl Default for TraceBuffer {
    fn default() -> Self {
        Self::new(1024)
    }
}

// ─── Global writer ──────────────────────────────────────────────────────────

struct SpanWriter {
    buffer: Mutex<VecDeque<Span>>,
    capacity: usize,
    disabled: bool,
    spans_dir: PathBuf,
}

impl SpanWriter {
    fn new() -> Self {
        let disabled = crate::settings::get().traces_disable;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let spans_dir = crate::paths::traces_dir(&cwd).join("spans");
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(10_000)),
            capacity: 10_000,
            disabled,
            spans_dir,
        }
    }

    fn append(&self, span: &Span) {
        // In-memory ring buffer mirror.
        {
            let mut buf = self.buffer.lock();
            if buf.len() >= self.capacity {
                buf.pop_front();
            }
            buf.push_back(span.clone());
        }
        if self.disabled {
            return;
        }
        if let Err(e) = self.persist(span) {
            // Best-effort logging; never propagate.
            log::warn!("cognition_span_write_failed: {}", e);
        }
    }

    fn persist(&self, span: &Span) -> std::io::Result<()> {
        fs::create_dir_all(&self.spans_dir)?;
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let file = self.spans_dir.join(format!("{day}.jsonl"));
        let mut f = OpenOptions::new().create(true).append(true).open(file)?;
        let mut line = serde_json::to_string(span).map_err(std::io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.flush()?;
        Ok(())
    }

    fn load_trace(&self, trace_id: &str) -> Vec<Span> {
        let mut out: Vec<Span> = self
            .buffer
            .lock()
            .iter()
            .filter(|s| s.trace_id == trace_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.finished_at.cmp(&b.finished_at));
        out
    }

    fn reset(&self) {
        self.buffer.lock().clear();
    }
}

static WRITER: Lazy<SpanWriter> = Lazy::new(SpanWriter::new);

fn random_span_id() -> String {
    // Hex-encoded random 128-bit id. We avoid pulling in `uuid` since the
    // workspace already exposes `rand`.
    let hi: u64 = rand::random();
    let lo: u64 = rand::random();
    format!("{:016x}{:016x}", hi, lo)
}

/// Persist a cognition span. Never panics; failures are logged.
/// Returns the generated `span_id`.
pub fn write_span(init: SpanInit) -> String {
    let span = Span {
        span_id: random_span_id(),
        trace_id: init.trace_id,
        parent_span_id: init.parent_span_id,
        workflow: init.workflow,
        step: init.step,
        kind: init.kind,
        prompt: init.prompt,
        model: init.model,
        provider: init.provider,
        input: init.input,
        output: init.output,
        latency_ms: init.latency_ms,
        status: init.status,
        metadata: init.metadata,
        finished_at: Utc::now().to_rfc3339(),
    };
    WRITER.append(&span);
    span.span_id
}

/// Load all spans known to the in-memory mirror for a given trace.
/// Sorted by `finished_at`.
pub fn load_trace(trace_id: &str) -> Vec<Span> {
    WRITER.load_trace(trace_id)
}

/// Test helper — clears the in-memory mirror. Does not touch on-disk JSONL.
pub fn reset_traces() {
    WRITER.reset();
}

/// Returns whether the on-disk writer is disabled (via `ITSY_TRACES_DISABLE`).
pub fn is_disabled() -> bool {
    WRITER.disabled
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TraceBuffer::record` and `dump` round-trip an event.
    #[test]
    fn buffer_record_then_dump() {
        let b = TraceBuffer::new(10);
        b.record("t1", "test_kind", serde_json::json!({"x": 1}));
        let evs = b.dump();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].trace_id, "t1");
        assert_eq!(evs[0].kind, "test_kind");
        assert_eq!(evs[0].data, serde_json::json!({"x": 1}));
    }

    /// When `capacity` is exceeded, oldest entries are evicted (ring-buffer
    /// invariant). Anti-regression: never grow unboundedly.
    #[test]
    fn buffer_evicts_oldest_when_full() {
        let b = TraceBuffer::new(3);
        for i in 0..5 {
            b.record("t", "k", serde_json::json!({"i": i}));
        }
        let evs = b.dump();
        assert_eq!(evs.len(), 3, "buffer must cap at capacity=3");
        // Oldest (i=0,1) evicted; we keep i=2, 3, 4.
        let ids: Vec<i64> = evs.iter()
            .filter_map(|e| e.data.get("i").and_then(|v| v.as_i64()))
            .collect();
        assert_eq!(ids, vec![2, 3, 4]);
    }

    /// `random_span_id` produces 32-hex-char strings and is collision-resistant.
    #[test]
    fn random_span_id_format() {
        let a = random_span_id();
        let b = random_span_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()),
            "span_id must be pure hex; got {a:?}");
        assert_ne!(a, b, "two consecutive span_ids must differ");
    }

    /// `write_span` returns a span_id and appends to the in-memory mirror.
    /// Filter by a unique trace_id to isolate from other parallel tests.
    #[test]
    fn write_span_appends_to_in_memory_mirror() {
        let trace = format!("test-trace-{}", random_span_id());
        let _id = write_span(SpanInit {
            trace_id: trace.clone(),
            workflow: "wf".into(),
            step: "step".into(),
            kind: "test".into(),
            latency_ms: 42,
            status: "ok".into(),
            ..Default::default()
        });
        let spans = load_trace(&trace);
        assert_eq!(spans.len(), 1, "must record exactly one span for this trace");
        assert_eq!(spans[0].latency_ms, 42);
        assert_eq!(spans[0].status, "ok");
        assert_eq!(spans[0].step, "step");
    }

    /// Spans returned by `load_trace` are sorted by `finished_at`.
    #[test]
    fn load_trace_returns_sorted_by_finished_at() {
        let trace = format!("sort-trace-{}", random_span_id());
        let _ = write_span(SpanInit {
            trace_id: trace.clone(),
            step: "first".into(),
            status: "ok".into(),
            ..Default::default()
        });
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _ = write_span(SpanInit {
            trace_id: trace.clone(),
            step: "second".into(),
            status: "ok".into(),
            ..Default::default()
        });
        let spans = load_trace(&trace);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].finished_at <= spans[1].finished_at,
            "spans must be sorted by finished_at");
    }
}
