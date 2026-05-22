//! Contract — a per-project "definition of done" backed by files on disk.
//!
//! A contract is a list of testable **assertions** the model commits
//! to up-front, scaled for a single-session agent on a small /
//! quantised model. The agent then works through them, marking each
//! `passed` or `failed` with command-line evidence. The model can't claim the work is done
//! while any assertion is still `pending` — there's literally a guard
//! that refuses the "I'm done" final text response otherwise.
//!
//! Layout on disk (per project, per contract):
//!
//! ```text
//! ~/.config/itsy/projects/<project>/contracts/
//!   .active                     <contract-id> the agent is currently working on
//!   <contract-id>/
//!     contract.md               proposal / brief (human-readable)
//!     assertions.md             rendered assertion list (human-readable)
//!     state.json                canonical machine-readable state (source of truth)
//!     features.json             optional sub-tasks (assertions cluster under them)
//!     log.jsonl                 append-only event log
//! ```
//!
//! `state.json` is the authoritative copy. The `.md` files are
//! re-rendered from it on each write so a human reader is always
//! looking at the current state.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── data model ──────────────────────────────────────────────────────────────

/// Top-level contract — a list of assertions + optional features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contract {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub status: ContractStatus,
    /// Free-form proposal / brief (markdown).
    pub brief: String,
    pub assertions: Vec<Assertion>,
    #[serde(default)]
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContractStatus {
    Draft,
    Active,
    Completed,
    Aborted,
}

impl ContractStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ContractStatus::Draft => "draft",
            ContractStatus::Active => "active",
            ContractStatus::Completed => "completed",
            ContractStatus::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assertion {
    pub id: String,
    pub text: String,
    pub state: AssertionState,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub last_check: Option<CommandEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssertionState {
    Pending,
    Passed,
    Failed,
    Skipped,
}

impl AssertionState {
    pub fn as_str(self) -> &'static str {
        match self {
            AssertionState::Pending => "pending",
            AssertionState::Passed => "passed",
            AssertionState::Failed => "failed",
            AssertionState::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEvidence {
    pub command: String,
    pub exit_code: i64,
    pub observation: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub description: String,
    /// IDs of assertions this feature is meant to fulfill.
    #[serde(default)]
    pub fulfills: Vec<String>,
    pub state: FeatureState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

impl FeatureState {
    pub fn as_str(self) -> &'static str {
        match self {
            FeatureState::Pending => "pending",
            FeatureState::InProgress => "in_progress",
            FeatureState::Done => "done",
            FeatureState::Cancelled => "cancelled",
        }
    }
}

/// One line in the append-only progress log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

// ── computed views ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct AssertionCounts {
    pub total: usize,
    pub pending: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl Contract {
    pub fn counts(&self) -> AssertionCounts {
        let mut c = AssertionCounts { total: 0, pending: 0, passed: 0, failed: 0, skipped: 0 };
        for a in &self.assertions {
            c.total += 1;
            match a.state {
                AssertionState::Pending => c.pending += 1,
                AssertionState::Passed => c.passed += 1,
                AssertionState::Failed => c.failed += 1,
                AssertionState::Skipped => c.skipped += 1,
            }
        }
        c
    }

    pub fn assertion(&self, id: &str) -> Option<&Assertion> {
        self.assertions.iter().find(|a| a.id == id)
    }

    pub fn assertion_mut(&mut self, id: &str) -> Option<&mut Assertion> {
        self.assertions.iter_mut().find(|a| a.id == id)
    }

    pub fn feature_mut(&mut self, id: &str) -> Option<&mut Feature> {
        self.features.iter_mut().find(|f| f.id == id)
    }

    /// Can this contract be marked `completed`? Only if every assertion
    /// has a terminal state (passed, failed, or explicitly skipped).
    pub fn ready_to_close(&self) -> bool {
        self.assertions.iter().all(|a| a.state != AssertionState::Pending)
    }
}

// ── storage layer ─────────────────────────────────────────────────────────

/// One contract on disk. Cheap to construct; doesn't read anything
/// until you call [`load`] / [`save`].
pub struct ContractFile {
    pub dir: PathBuf,
}

impl ContractFile {
    pub fn for_project(cwd: &Path, contract_id: &str) -> Self {
        let dir = crate::paths::project_dir(cwd).join("contracts").join(contract_id);
        Self { dir }
    }

    pub fn exists(&self) -> bool {
        self.dir.join("state.json").exists()
    }

    /// Ensure the directory + seed files exist. Does not overwrite a
    /// pre-existing state.json.
    fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("create contract dir {}", self.dir.display()))?;
        Ok(())
    }

    pub fn load(&self) -> Result<Contract> {
        let path = self.dir.join("state.json");
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let c: Contract = serde_json::from_str(&text)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(c)
    }

    /// Persist the canonical state.json AND re-render the human files.
    pub fn save(&self, c: &Contract) -> Result<()> {
        self.ensure_dir()?;
        let state_path = self.dir.join("state.json");
        let json = serde_json::to_string_pretty(c)?;
        atomic_write(&state_path, json.as_bytes())?;
        // Re-render the human-readable views.
        atomic_write(&self.dir.join("contract.md"), render_brief_md(c).as_bytes())?;
        atomic_write(
            &self.dir.join("assertions.md"),
            render_assertions_md(c).as_bytes(),
        )?;
        if !c.features.is_empty() {
            let features_json = serde_json::to_string_pretty(&c.features)?;
            atomic_write(&self.dir.join("features.json"), features_json.as_bytes())?;
        }
        Ok(())
    }

    pub fn append_log(&self, kind: &str, payload: serde_json::Value) -> Result<()> {
        self.ensure_dir()?;
        let entry = LogEntry {
            timestamp: now_iso(),
            kind: kind.to_string(),
            payload,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        let log_path = self.dir.join("log.jsonl");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open {} for append", log_path.display()))?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ── active-contract tracking ──────────────────────────────────────────────

fn active_marker(cwd: &Path) -> PathBuf {
    crate::paths::project_dir(cwd).join("contracts").join(".active")
}

/// Persist "this is the contract the agent is currently working on" for
/// a given cwd. Subsequent launches against the same cwd will see it.
pub fn set_active_id(cwd: &Path, contract_id: &str) -> Result<()> {
    let marker = active_marker(cwd);
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&marker, contract_id.as_bytes())
}

/// Clear the active contract marker (no contract in flight).
pub fn clear_active_id(cwd: &Path) -> Result<()> {
    let marker = active_marker(cwd);
    if marker.exists() {
        fs::remove_file(&marker)?;
    }
    Ok(())
}

/// Read the active contract id from disk, if any.
pub fn read_active_id(cwd: &Path) -> Option<String> {
    let marker = active_marker(cwd);
    if !marker.exists() {
        return None;
    }
    fs::read_to_string(&marker).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ── in-process cache ──────────────────────────────────────────────────────

/// Latest snapshot kept in memory so the agent loop doesn't re-read
/// state.json after every tool call. Updated by the tools themselves.
static CURRENT: OnceLock<Mutex<Option<Contract>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Contract>> {
    CURRENT.get_or_init(|| Mutex::new(None))
}

pub fn current() -> Option<Contract> {
    cache().lock().expect("contract cache poisoned").clone()
}

pub fn set_current(c: Option<Contract>) {
    *cache().lock().expect("contract cache poisoned") = c;
}

/// Try to load the active contract for `cwd` into the in-memory cache.
/// Called once at agent startup so subsequent tool calls see the right
/// state without round-tripping to disk.
pub fn rehydrate(cwd: &Path) {
    let Some(id) = read_active_id(cwd) else {
        set_current(None);
        return;
    };
    let file = ContractFile::for_project(cwd, &id);
    match file.load() {
        Ok(c) => set_current(Some(c)),
        Err(_) => set_current(None),
    }
}

// ── helpers used by tools ─────────────────────────────────────────────────

/// Make a contract id from a title — kebab-cased, ≤32 chars, suffixed
/// with a short timestamp to keep multiple runs distinct.
pub fn make_id(title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut out = String::new();
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(c);
    }
    let out = out.trim_matches('-').to_string();
    let short = if out.len() > 32 { &out[..32] } else { &out };
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    if short.is_empty() {
        format!("contract-{stamp}")
    } else {
        format!("{short}-{stamp}")
    }
}

/// Render a compact text snapshot of the contract for embedding in the
/// model's system prompt. Skips low-signal noise (timestamps, evidence
/// blobs) to keep the token budget tight on small models.
pub fn render_for_prompt(c: &Contract) -> String {
    let counts = c.counts();
    let mut out = String::new();
    out.push_str(&format!(
        "## Active contract: {} ({})\n",
        c.title,
        c.status.as_str()
    ));
    out.push_str(&format!(
        "Assertions: {} passed · {} failed · {} pending · {} skipped (of {})\n",
        counts.passed, counts.failed, counts.pending, counts.skipped, counts.total
    ));
    out.push_str("\n");
    for a in &c.assertions {
        let badge = match a.state {
            AssertionState::Passed => "[x]",
            AssertionState::Failed => "[!]",
            AssertionState::Skipped => "[~]",
            AssertionState::Pending => "[ ]",
        };
        out.push_str(&format!("  {} {}  {}\n", badge, a.id, a.text));
        if let Some(ev) = &a.evidence {
            if !ev.is_empty() && a.state != AssertionState::Pending {
                // One indented line of evidence. Truncate so the prompt
                // doesn't explode if the model dumped a verbose log.
                let trim = if ev.len() > 140 { &ev[..140] } else { ev };
                out.push_str(&format!("        evidence: {trim}\n"));
            }
        }
    }
    if !c.features.is_empty() {
        out.push_str("\nFeatures:\n");
        for f in &c.features {
            let badge = match f.state {
                FeatureState::Done => "[x]",
                FeatureState::InProgress => "[~]",
                FeatureState::Cancelled => "[-]",
                FeatureState::Pending => "[ ]",
            };
            out.push_str(&format!(
                "  {} {}  {}  (fulfills: {})\n",
                badge,
                f.id,
                f.description,
                if f.fulfills.is_empty() { "—".to_string() } else { f.fulfills.join(", ") },
            ));
        }
    }
    out
}

fn render_brief_md(c: &Contract) -> String {
    format!(
        "# {}\n\nContract id: `{}`\nStatus: `{}`\nCreated: {}\n\n## Brief\n\n{}\n",
        c.title, c.id, c.status.as_str(), c.created_at, c.brief.trim()
    )
}

fn render_assertions_md(c: &Contract) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Assertions — {}\n\n", c.title));
    let counts = c.counts();
    out.push_str(&format!(
        "{} passed · {} failed · {} pending · {} skipped (of {})\n\n",
        counts.passed, counts.failed, counts.pending, counts.skipped, counts.total
    ));
    for a in &c.assertions {
        let badge = match a.state {
            AssertionState::Passed => "✅",
            AssertionState::Failed => "❌",
            AssertionState::Skipped => "⏭",
            AssertionState::Pending => "⬜",
        };
        out.push_str(&format!("- {} **{}** — {}\n", badge, a.id, a.text));
        if let Some(ev) = &a.evidence {
            if !ev.is_empty() {
                out.push_str(&format!("  - evidence: {ev}\n"));
            }
        }
        if let Some(c) = &a.last_check {
            out.push_str(&format!(
                "  - last check: `{}` exit={} → {}\n",
                c.command, c.exit_code, c.observation
            ));
        }
    }
    out
}

// ── high-level mutation helpers ───────────────────────────────────────────

/// Create a contract, persist it, set it active, and load it into the
/// cache. Returns the loaded contract.
pub fn create(
    cwd: &Path,
    title: String,
    brief: String,
    assertions: Vec<(String, String)>, // (id, text)
    features: Vec<(String, String, Vec<String>)>, // (id, description, fulfills)
) -> Result<Contract> {
    if assertions.is_empty() {
        return Err(anyhow!(
            "a contract must have at least one assertion — define what 'done' means"
        ));
    }
    let id = make_id(&title);
    let c = Contract {
        id: id.clone(),
        title,
        created_at: now_iso(),
        status: ContractStatus::Active,
        brief,
        assertions: assertions
            .into_iter()
            .map(|(id, text)| Assertion {
                id,
                text,
                state: AssertionState::Pending,
                evidence: None,
                last_check: None,
            })
            .collect(),
        features: features
            .into_iter()
            .map(|(id, description, fulfills)| Feature {
                id,
                description,
                fulfills,
                state: FeatureState::Pending,
            })
            .collect(),
    };
    let file = ContractFile::for_project(cwd, &id);
    file.save(&c)?;
    file.append_log("created", serde_json::json!({ "title": c.title }))?;
    set_active_id(cwd, &id)?;
    set_current(Some(c.clone()));
    Ok(c)
}

/// Mark an assertion `passed` / `failed` / `skipped` with evidence.
/// Returns the updated contract.
pub fn mark_assertion(
    cwd: &Path,
    assertion_id: &str,
    state: AssertionState,
    evidence: String,
    check: Option<CommandEvidence>,
) -> Result<Contract> {
    let Some(mut c) = current() else {
        return Err(anyhow!("no active contract — call propose_contract first"));
    };
    {
        let a = c
            .assertion_mut(assertion_id)
            .ok_or_else(|| anyhow!("assertion `{assertion_id}` not found"))?;
        a.state = state;
        a.evidence = Some(evidence.clone());
        a.last_check = check.clone();
    }
    let file = ContractFile::for_project(cwd, &c.id);
    file.save(&c)?;
    file.append_log(
        "assertion_marked",
        serde_json::json!({ "id": assertion_id, "state": state.as_str(), "evidence": evidence }),
    )?;
    set_current(Some(c.clone()));
    Ok(c)
}

/// Mark a feature `in_progress` / `done` / `cancelled`.
pub fn mark_feature(cwd: &Path, feature_id: &str, state: FeatureState) -> Result<Contract> {
    let Some(mut c) = current() else {
        return Err(anyhow!("no active contract"));
    };
    {
        let f = c
            .feature_mut(feature_id)
            .ok_or_else(|| anyhow!("feature `{feature_id}` not found"))?;
        f.state = state;
    }
    let file = ContractFile::for_project(cwd, &c.id);
    file.save(&c)?;
    file.append_log(
        "feature_marked",
        serde_json::json!({ "id": feature_id, "state": state.as_str() }),
    )?;
    set_current(Some(c.clone()));
    Ok(c)
}

/// Finalize the contract — refuses if any assertion is still pending.
pub fn close(cwd: &Path, status: ContractStatus) -> Result<Contract> {
    let Some(mut c) = current() else {
        return Err(anyhow!("no active contract to close"));
    };
    if status == ContractStatus::Completed {
        let pending: Vec<&str> = c
            .assertions
            .iter()
            .filter(|a| a.state == AssertionState::Pending)
            .map(|a| a.id.as_str())
            .collect();
        if !pending.is_empty() {
            return Err(anyhow!(
                "cannot close as `completed` — these assertions are still pending: {}. \
                 Call `mark_assertion` on each (passed/failed/skipped with evidence) first.",
                pending.join(", ")
            ));
        }
        let failed: Vec<&str> = c
            .assertions
            .iter()
            .filter(|a| a.state == AssertionState::Failed)
            .map(|a| a.id.as_str())
            .collect();
        if !failed.is_empty() {
            return Err(anyhow!(
                "cannot close as `completed` — these assertions are still failed: {}. \
                 Either fix the underlying issue and re-mark them `passed`, mark them \
                 `skipped` with a justification, or close as `aborted` if the work \
                 cannot be done.",
                failed.join(", ")
            ));
        }
    }
    c.status = status;
    let file = ContractFile::for_project(cwd, &c.id);
    file.save(&c)?;
    file.append_log("closed", serde_json::json!({ "status": status.as_str() }))?;
    clear_active_id(cwd)?;
    set_current(None);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    // These tests mutate the shared `ITSY_HOME` env var and the
    // process-wide CURRENT cache, so they must run serially.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tmp_cwd() -> PathBuf {
        let p = env::temp_dir().join(format!("itsy-contract-test-{}", uuid_like()));
        fs::create_dir_all(&p).unwrap();
        // Set ITSY_HOME so project_dir resolves under tmp.
        unsafe { env::set_var("ITSY_HOME", p.join(".itsy")) };
        // Always start each test with a clean in-memory cache.
        set_current(None);
        p
    }

    fn uuid_like() -> String {
        use std::time::SystemTime;
        format!(
            "{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn create_round_trip() {
        let _g = TEST_LOCK.lock().unwrap();
        let cwd = tmp_cwd();
        let c = create(
            &cwd,
            "Wire auth middleware".into(),
            "Add login + session cookie handling.".into(),
            vec![
                ("A.001".into(), "/login returns 200 on valid creds".into()),
                ("A.002".into(), "/login returns 401 on bad creds".into()),
            ],
            vec![],
        )
        .unwrap();
        assert_eq!(c.assertions.len(), 2);
        assert_eq!(c.status, ContractStatus::Active);

        let loaded = ContractFile::for_project(&cwd, &c.id).load().unwrap();
        assert_eq!(loaded.id, c.id);
        assert_eq!(loaded.assertions[0].state, AssertionState::Pending);

        // Active id persisted.
        assert_eq!(read_active_id(&cwd).as_deref(), Some(c.id.as_str()));
        set_current(None);
    }

    #[test]
    fn cannot_create_without_assertions() {
        let _g = TEST_LOCK.lock().unwrap();
        let cwd = tmp_cwd();
        let err = create(&cwd, "t".into(), "b".into(), vec![], vec![]).unwrap_err();
        assert!(err.to_string().contains("at least one assertion"));
        set_current(None);
    }

    #[test]
    fn mark_then_close() {
        let _g = TEST_LOCK.lock().unwrap();
        let cwd = tmp_cwd();
        let c = create(
            &cwd,
            "Tiny job".into(),
            "tiny".into(),
            vec![("A.001".into(), "the thing works".into())],
            vec![],
        )
        .unwrap();
        // Can't close as completed while pending.
        let err = close(&cwd, ContractStatus::Completed).unwrap_err();
        assert!(err.to_string().contains("still pending"));
        // Mark and try again.
        mark_assertion(
            &cwd,
            "A.001",
            AssertionState::Passed,
            "ran the smoke test; saw 'OK'".into(),
            Some(CommandEvidence {
                command: "./smoke.sh".into(),
                exit_code: 0,
                observation: "OK".into(),
                timestamp: now_iso(),
            }),
        )
        .unwrap();
        let closed = close(&cwd, ContractStatus::Completed).unwrap();
        assert_eq!(closed.status, ContractStatus::Completed);
        assert_eq!(read_active_id(&cwd), None);
        assert!(current().is_none());
        // Log file exists and has entries.
        let log = ContractFile::for_project(&cwd, &c.id).dir.join("log.jsonl");
        let text = fs::read_to_string(log).unwrap();
        assert!(text.contains("\"created\""));
        assert!(text.contains("\"assertion_marked\""));
        assert!(text.contains("\"closed\""));
        set_current(None);
    }

    #[test]
    fn render_for_prompt_is_compact() {
        let _g = TEST_LOCK.lock().unwrap();
        let cwd = tmp_cwd();
        let c = create(
            &cwd,
            "T".into(),
            "b".into(),
            vec![("A.001".into(), "x".into()), ("A.002".into(), "y".into())],
            vec![],
        )
        .unwrap();
        let s = render_for_prompt(&c);
        assert!(s.contains("Active contract: T"));
        assert!(s.contains("[ ] A.001"));
        assert!(s.contains("[ ] A.002"));
        set_current(None);
    }
}
