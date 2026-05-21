//! Multi-session orchestrator.
//!
//! Run parallel agent sessions on different tasks. Each session is an
//! independent conversation with its own message history.
//!
//! Usage:
//!   /session new "task description"     — spawn a new parallel session
//!   /session list                       — show all active sessions
//!   /session switch <id>                — switch focus to another session
//!   /session kill <id>                  — terminate a session

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use parking_lot::Mutex;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Paused,
    Completed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Paused => "paused",
            SessionStatus::Completed => "completed",
        }
    }
}

/// A single conversation/session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default)]
    pub messages: Vec<Value>,
    pub started_at_ms: i64,
    #[serde(default)]
    pub tool_calls: u64,
}

/// Lightweight info row returned by [`MultiSession::list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    pub messages: usize,
    pub active: bool,
    /// Age in seconds.
    pub age_secs: i64,
    pub tool_calls: u64,
}

pub struct MultiSession {
    sessions: Mutex<Vec<Session>>,
    active: Mutex<Option<String>>,
    counter: AtomicU64,
}

impl MultiSession {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            active: Mutex::new(None),
            counter: AtomicU64::new(0),
        }
    }

    fn new_id() -> String {
        // 6 hex chars, like JS `crypto.randomBytes(3).toString('hex')`.
        let mut bytes = [0u8; 3];
        rand::thread_rng().fill_bytes(&mut bytes);
        format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
    }

    /// Create a new session. Returns the created session (cloned).
    pub fn create(&self, title: Option<&str>) -> Session {
        let mut sessions = self.sessions.lock();
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = Self::new_id();
        let session = Session {
            id: id.clone(),
            title: title.map(str::to_string).unwrap_or_else(|| format!("Session {}", n)),
            status: SessionStatus::Active,
            messages: Vec::new(),
            started_at_ms: Utc::now().timestamp_millis(),
            tool_calls: 0,
        };
        sessions.push(session.clone());
        drop(sessions);
        let mut active = self.active.lock();
        if active.is_none() {
            *active = Some(id);
        }
        session
    }

    pub fn active_id(&self) -> Option<String> {
        self.active.lock().clone()
    }

    /// Currently active session (cloned).
    pub fn active(&self) -> Option<Session> {
        let id = self.active.lock().clone()?;
        self.sessions.lock().iter().find(|s| s.id == id).cloned()
    }

    /// Switch focus to another session. Returns the switched-to session.
    pub fn switch(&self, id: &str) -> Option<Session> {
        let sessions = self.sessions.lock();
        let s = sessions.iter().find(|s| s.id == id).cloned()?;
        drop(sessions);
        *self.active.lock() = Some(id.to_string());
        Some(s)
    }

    /// All sessions as a summary list.
    pub fn list(&self) -> Vec<SessionInfo> {
        let now = Utc::now().timestamp_millis();
        let active = self.active.lock().clone();
        self.sessions
            .lock()
            .iter()
            .map(|s| SessionInfo {
                id: s.id.clone(),
                title: s.title.clone(),
                status: s.status,
                messages: s.messages.len(),
                active: active.as_deref() == Some(s.id.as_str()),
                age_secs: ((now - s.started_at_ms) / 1000).max(0),
                tool_calls: s.tool_calls,
            })
            .collect()
    }

    /// Detailed status for a single session by id.
    pub fn status(&self, id: &str) -> Option<SessionInfo> {
        self.list().into_iter().find(|s| s.id == id)
    }

    /// Kill / remove a session. Returns true on success.
    pub fn kill(&self, id: &str) -> bool {
        let mut sessions = self.sessions.lock();
        let before = sessions.len();
        sessions.retain(|s| s.id != id);
        if sessions.len() == before {
            return false;
        }
        let mut active = self.active.lock();
        if active.as_deref() == Some(id) {
            *active = sessions.first().map(|s| s.id.clone());
        }
        true
    }

    /// Set lifecycle status for a session.
    pub fn set_status(&self, id: &str, status: SessionStatus) -> bool {
        let mut sessions = self.sessions.lock();
        if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
            s.status = status;
            true
        } else {
            false
        }
    }

    /// Messages for the active session.
    pub fn get_messages(&self) -> Vec<Value> {
        match self.active_id() {
            Some(id) => self
                .sessions
                .lock()
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.messages.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Push a message to the active session.
    pub fn push_message(&self, msg: Value) -> bool {
        let id = match self.active_id() {
            Some(i) => i,
            None => return false,
        };
        let mut sessions = self.sessions.lock();
        if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
            s.messages.push(msg);
            true
        } else {
            false
        }
    }

    /// Increment the tool-call counter on the active session.
    pub fn inc_tool_calls(&self) {
        if let Some(id) = self.active_id() {
            let mut sessions = self.sessions.lock();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.tool_calls += 1;
            }
        }
    }

    pub fn count(&self) -> usize {
        self.sessions.lock().len()
    }
}

impl Default for MultiSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_switches_active_first_time() {
        let m = MultiSession::new();
        let s1 = m.create(Some("first"));
        assert_eq!(m.active_id().as_deref(), Some(s1.id.as_str()));
        let _s2 = m.create(Some("second"));
        // Still the first one.
        assert_eq!(m.active_id().as_deref(), Some(s1.id.as_str()));
    }

    #[test]
    fn kill_active_promotes_remaining() {
        let m = MultiSession::new();
        let s1 = m.create(None);
        let s2 = m.create(None);
        assert!(m.kill(&s1.id));
        assert_eq!(m.active_id().as_deref(), Some(s2.id.as_str()));
    }

    #[test]
    fn list_marks_active() {
        let m = MultiSession::new();
        let s1 = m.create(None);
        let s2 = m.create(None);
        m.switch(&s2.id);
        let l = m.list();
        assert_eq!(l.len(), 2);
        assert!(l.iter().find(|i| i.id == s2.id).unwrap().active);
        assert!(!l.iter().find(|i| i.id == s1.id).unwrap().active);
    }
}
