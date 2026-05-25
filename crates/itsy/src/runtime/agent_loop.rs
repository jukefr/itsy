//! Agent loop state machine — `TurnState` + guard methods.
//!
//! Contains [`handle_turn`] (the main agent loop), [`TurnState`] (per-turn
//! counters), and [`AgentSession`] (the session compartment structs).
//! Everything here is bin-independent and lives in the library crate.

use std::path::PathBuf;
use std::sync::Arc;
use std::collections::{HashMap, HashSet};

use serde_json::Value;
use serde_json::json;
use crate::config::{Config, Flags};
use crate::fullscreen::Fullscreen;
use crate::governor::{ToolScorer, VerificationHistory};
use crate::governor::early_stop::EarlyStopDetector;
use crate::memory::MemoryStore;
use crate::plugins::loader::PluginLoader;
use crate::plugins::skills::SkillManager;
use crate::session::persistence::SessionStore;
use crate::session::snapshot::SnapshotManager;
use crate::session::file_state::FileStateTracker;
use crate::session::tokens::TokenTracker;
use crate::token_monitor::TokenMonitor;
use crate::tools_impl::read_tracker::ReadTracker;
use crate::trace_recorder::TraceRecorder;
use crate::mcp_bridge::McpBridge;


// ── Agent session: shared per-process state ──────────────────────────────────

/// Immutable after startup — no lock required.
pub struct AgentSessionReadOnly {
    pub flags: Flags,
    pub cwd: PathBuf,
    pub mcp_bridge: Arc<McpBridge>,
}

/// Read-mostly state behind an `RwLock`.
pub struct AgentSessionShared {
    pub config: Config,
    pub memory: Arc<parking_lot::Mutex<MemoryStore>>,
    pub skills: SkillManager,
    pub plugins: PluginLoader,
    pub tokens: TokenTracker,
    pub token_monitor: TokenMonitor,
    pub sessions: SessionStore,
    pub read_tracker: Arc<ReadTracker>,
    pub file_state: Arc<FileStateTracker>,
}

/// Frequently-written state behind a single `Mutex`.
pub struct AgentSessionMutable {
    pub history: Vec<Value>,
    pub scorer: ToolScorer,
    pub verification: VerificationHistory,
    pub early_stop: EarlyStopDetector,
    pub trace: TraceRecorder,
    pub current_tool_category: Option<String>,
    pub fullscreen: Option<Arc<Fullscreen>>,
    pub tool_repeat_counts: HashMap<String, u32>,
    pub mutated_paths: HashSet<String>,
    pub bash_loop_keys: HashSet<String>,
    pub readonly_turn_count: u32,
    pub total_mutating_calls: u32,
    pub snapshot_manager: Arc<SnapshotManager>,
}

impl AgentSessionMutable {
    /// After a successful mutating tool call, retire the bash-loop tracking
    /// keys: drain `bash_loop_keys` and remove each of those keys from
    /// `tool_repeat_counts` so the next identical bash invocation isn't
    /// instantly flagged as a stuck loop. Both maps are mutated in place;
    /// this used to be implemented against a discarded clone (bug #3).
    pub fn reset_bash_loop_after_mutation(&mut self) {
        let keys: Vec<String> = self.bash_loop_keys.drain().collect();
        for k in keys {
            self.tool_repeat_counts.remove(&k);
        }
    }
}

/// Bundle of state that lives across user turns within a single agent run.
pub struct AgentSession {
    pub ro: AgentSessionReadOnly,
    pub shared: parking_lot::RwLock<AgentSessionShared>,
    pub mutable: parking_lot::Mutex<AgentSessionMutable>,
}

/// Action a guard can take when it fires.
#[derive(Debug)]
pub enum GuardAction {
    /// Inject a system message and continue the inner loop.
    Inject { message: String },
    /// Skip the current tool call without breaking the loop.
    Skip,
    /// Break out of the inner tool-batch loop.
    Break,
    /// Break out of the outer turn loop.
    Stop,
}

/// Per-turn mutable state.
///
/// Created at the start of every [`handle_turn`] call and dropped at the end.
/// Session-scoped counters (e.g. `tool_repeat_counts`, `readonly_turn_count`)
/// stay on [`AgentSession`].
pub struct TurnState {
    // ── config ──────────────────────────────────────────────────
    pub user_msg: String,
    pub augmented: String,
    pub task_type: &'static str,
    pub current_category: Option<String>,

    // ── counters ────────────────────────────────────────────────
    pub tool_calls_this_turn: u32,
    pub max_tool_calls: u32,
    pub consecutive_blocks: u32,
    pub last_prompt_tokens: u64,
    pub force_disable_thinking: bool,
    pub had_mutating_call: bool,
    pub empty_retry_injected: bool,

    // ── tracking ────────────────────────────────────────────────
    pub edited_files: Vec<String>,
    pub improvement_attempts: HashMap<String, u32>,
    pub per_turn_repeats: HashMap<String, u32>,
    pub per_turn_write_seen: HashSet<String>,
    pub recent_tool_failures: HashMap<String, u32>,

    /// Last few model-response signatures (content + tool-call sequence)
    /// to detect identical replies. A small IQ-quantised model can settle
    /// into emitting the exact same response several times in a row —
    /// the existing no-progress nudge is a soft warning that doesn't
    /// escape the loop. Three identical responses → hard-stop the turn.
    pub recent_response_sigs: std::collections::VecDeque<u64>,
}

impl TurnState {
    /// Create initial state for a new turn.
    pub fn new(user_msg: String, task_type: &'static str, stage2_category: Option<String>, max_tool_calls: u32) -> Self {
        Self {
            augmented: user_msg.clone(),
            user_msg,
            task_type,
            current_category: stage2_category,
            tool_calls_this_turn: 0,
            max_tool_calls,
            consecutive_blocks: 0,
            last_prompt_tokens: 0,
            force_disable_thinking: false,
            had_mutating_call: false,
            empty_retry_injected: false,
            edited_files: Vec::new(),
            improvement_attempts: HashMap::new(),
            per_turn_repeats: HashMap::new(),
            per_turn_write_seen: HashSet::new(),
            recent_tool_failures: HashMap::new(),
            recent_response_sigs: std::collections::VecDeque::with_capacity(3),
        }
    }

    /// Record a model response signature and report whether the agent has
    /// emitted the SAME response (same content + same tool-call sequence)
    /// three times in a row. The caller is expected to break out of the
    /// turn loop on `true`.
    pub fn note_response_and_check_stuck(
        &mut self,
        content: &str,
        tool_calls: &[Value],
    ) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut h);
        for tc in tool_calls {
            let n = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("");
            let a = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("");
            n.hash(&mut h);
            a.hash(&mut h);
        }
        let sig = h.finish();
        const STUCK_REPEAT: usize = 3;
        self.recent_response_sigs.push_back(sig);
        while self.recent_response_sigs.len() > STUCK_REPEAT {
            self.recent_response_sigs.pop_front();
        }
        self.recent_response_sigs.len() == STUCK_REPEAT
            && self.recent_response_sigs.iter().all(|s| *s == sig)
    }

    /// Check tool call names for validity. Returns `GuardAction::Inject` when
    /// one or more tool calls have empty or unknown names, with retry budget.
    pub fn check_quality_monitor(
        &mut self,
        tool_calls: &[Value],
        known: &[&str],
        history: &mut Vec<Value>,
    ) -> Option<GuardAction> {
        let bad: Vec<(String, String)> = tool_calls
            .iter()
            .filter_map(|tc| {
                let name = tc
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() || !known.contains(&name) {
                    Some((id.to_string(), name.to_string()))
                } else {
                    None
                }
            })
            .collect();

        if bad.is_empty() {
            return None;
        }

        let n = *self.per_turn_repeats
            .entry("__quality_correction".into())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        if n > 2 {
            return None;
        }

        let tool_list = known.join(", ");
        for (id, name) in &bad {
            let err = if name.is_empty() {
                format!("Error: tool name is empty. Available tools: {tool_list}.")
            } else {
                format!(
                    "Error: `{name}` is not a valid tool. Available tools: {tool_list}."
                )
            };
            history.push(json!({"role": "tool", "tool_call_id": id, "content": err}));
        }
        history.push(json!({
            "role": "user",
            "content": format!(
                "[SYSTEM] One or more tool calls used an invalid tool name. \
                 Call tools by their exact name. Available: {tool_list}."
            ),
        }));

        Some(GuardAction::Inject {
            message: format!("{} invalid tool name(s) — steering ({n}/2)", bad.len()),
        })
    }

    /// Check if a mutating tool call should be blocked by the contract gate.
    /// When the contract feature is on and no active contract exists, refuse
    /// mutating calls until the model calls `propose_contract`.
    pub fn check_contract_gate(
        &mut self,
        name: &str,
        is_mutating: bool,
        id: &str,
        history: &mut Vec<Value>,
    ) -> Option<GuardAction> {
        const MAX_REFUSALS: u32 = 3;
        if !crate::settings::get().contract
            || crate::session::contract::current().is_some()
            || !is_mutating
        {
            return None;
        }
        let refused = self.per_turn_repeats
            .entry("__contract_gate".into())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        if *refused <= MAX_REFUSALS {
            history.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": format!(
                    "[BLOCKED] No contract yet. Before any mutating action, call `propose_contract` \
                     with: (1) a short title, (2) a 2-3 sentence brief, (3) 2-6 assertions you'll verify \
                     (each one-line, testable, ≤120 chars; e.g. \"Exit code is 0 when running pytest\", \
                     \"about.md byte-for-byte matches the lost commit's version\"). Read-only tools \
                     (read_file, search, find_files, git status/log/show, …) are still allowed."
                ),
            }));
            Some(GuardAction::Inject {
                message: format!("blocked `{name}` — define what 'done' means first ({refused}/{MAX_REFUSALS})"),
            })
        } else {
            // Budget exhausted — let the call through.
            None
        }
    }

    /// Skip duplicate idempotent-write tool calls (memory_remember, memory_forget,
    /// mark_assertion) when called with identical arguments within the same turn.
    pub fn check_idempotent_write(
        &mut self,
        name: &str,
        args: &Value,
        id: &str,
        history: &mut Vec<Value>,
    ) -> Option<GuardAction> {
        const TOOLS: &[&str] = &["memory_remember", "memory_forget", "mark_assertion"];
        if !TOOLS.contains(&name) {
            return None;
        }
        let write_key = crate::tools_impl::dedup::idempotent_write_key(name, args);
        if self.per_turn_write_seen.insert(write_key) {
            return None; // First time — allow.
        }
        // Duplicate — skip.
        let skip_msg = if name == "mark_assertion" {
            let id_str = args.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let state_str = args.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            format!(
                "[DUPLICATE] mark_assertion for `{id_str}` as `{state_str}` was already called this turn. \
                 Move on to the next pending assertion — do not repeat the same mark."
            )
        } else {
            "[already stored this turn — identical call skipped]".to_string()
        };
        history.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": serde_json::to_string(&json!({"result": skip_msg})).unwrap_or_default(),
        }));
        Some(GuardAction::Skip)
    }

    /// Check if the model has spent too many consecutive tool-call batches without
    /// making any file mutations. Injects a nudge to stop reading and start editing.
    pub fn check_no_progress(
        &mut self,
        batch_had_mutating: bool,
        batch_had_contract_progress: bool,
        batch_had_active_bash: bool,
        batch_had_fresh_read: bool,
        session: &crate::runtime::agent_loop::AgentSession,
    ) -> Option<GuardAction> {
        if !batch_had_mutating && !batch_had_contract_progress && !batch_had_active_bash && !batch_had_fresh_read {
            let mut guard = session.mutable.lock();
            guard.readonly_turn_count += 1;
            let count = guard.readonly_turn_count;
            let total_mutations = guard.total_mutating_calls;
            let threshold = if total_mutations == 0 { 3 } else { 6 };
            if count >= threshold {
                guard.readonly_turn_count = 0;
                drop(guard);
                session.mutable.lock().history.push(json!({
                    "role": "user",
                    "content": format!(
                        "[SYSTEM] You have spent {} consecutive rounds reading files \
                         without making any changes. You already have enough information. \
                         STOP reading — make a concrete edit RIGHT NOW. Call patch or \
                         write_file to modify a file. Do not call read_file, bash, \
                         graph_search, or any other read-only tool until you have made \
                         at least one actual change.",
                        count
                    )
                }));
                return Some(GuardAction::Inject {
                    message: format!("no-progress nudge: {count} read-only batches — pushing model to act"),
                });
            }
        } else {
            session.mutable.lock().readonly_turn_count = 0;
        }
        None
    }

    /// Check if the model has responded with text-only multiple consecutive times
    /// without calling any tool. Injects a nudge to force tool use.
    pub fn check_text_only_streak(
        &mut self,
        content_opt: Option<&str>,
        history: &mut Vec<Value>,
    ) -> Option<GuardAction> {
        let n = *self.per_turn_repeats
            .entry("__text_only_streak".into())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        const MAX_STREAK: u32 = 3;
        if n < MAX_STREAK || self.current_category.as_deref() == Some("respond") {
            return None;
        }
        if let Some(content) = content_opt {
            history.push(json!({"role": "assistant", "content": content}));
        }
        let has_contract = crate::settings::get().contract
            && crate::session::contract::current()
                .map(|c| {
                    let cnt = c.counts();
                    cnt.pending > 0 || cnt.failed > 0
                })
                .unwrap_or(false);
        let msg = if has_contract {
            "[SYSTEM] You have responded with text only multiple times without using tools. \
             The contract has unverified assertions. Stop writing explanations — call a tool right now. \
             Run a verification command with `bash`, then call `mark_assertion` with the result. \
             Do NOT respond with text. Use a tool."
        } else {
            "[SYSTEM] You have responded with text only multiple times without using tools. \
             Use a tool to make progress instead of describing what you plan to do."
        };
        history.push(json!({"role": "user", "content": msg}));
        Some(GuardAction::Inject {
            message: format!("text-only streak: {n} consecutive responses without tools — forcing tool use ({n})"),
        })
    }

    /// Check if the contract still has pending/failed assertions when the model
    /// tries to end the turn. Injects a nudge to finish the contract.
    pub fn check_contract_close_loop(
        &mut self,
        content_opt: Option<&str>,
        history: &mut Vec<Value>,
    ) -> Option<GuardAction> {
        if !crate::settings::get().contract {
            return None;
        }
        let c = crate::session::contract::current()?;
        let counts = c.counts();
        let all_passed = counts.pending == 0 && counts.failed == 0 && counts.passed == counts.total;
        let contract_still_open = c.status == crate::session::contract::ContractStatus::Active;
        if !(counts.pending > 0 || counts.failed > 0 || (all_passed && contract_still_open)) {
            return None;
        }

        const MAX_LOOP: u32 = 12;
        let n = *self.per_turn_repeats
            .entry("__contract_loop".into())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        if n > MAX_LOOP {
            crate::session::contract::set_evaluator_result(
                crate::session::contract::EvaluatorResult {
                    passed: false,
                    findings: vec![format!("model gave up; {} pending + {} failed at end of turn", counts.pending, counts.failed)],
                },
            );
            return None;
        }

        if let Some(content) = content_opt {
            history.push(json!({"role": "assistant", "content": content}));
        }

        let mut msg = String::from("[SYSTEM] The turn cannot end yet — the contract is not closed.\n\n");
        if all_passed && contract_still_open {
            msg.push_str(
                "Every assertion is `passed`. \
                 Call `close_contract` `completed` RIGHT NOW to finish the task. \
                 Do not run any more commands or write any more text — \
                 just call `close_contract` `completed`."
            );
        } else {
            let failed_ids: Vec<&str> = c.assertions.iter()
                .filter(|a| a.state == crate::session::contract::AssertionState::Failed)
                .map(|a| a.id.as_str())
                .collect();
            let pending_ids: Vec<&str> = c.assertions.iter()
                .filter(|a| a.state == crate::session::contract::AssertionState::Pending)
                .map(|a| a.id.as_str())
                .collect();
            if !failed_ids.is_empty() {
                let blocks: Vec<String> = c.assertions.iter()
                    .filter(|a| a.state == crate::session::contract::AssertionState::Failed)
                    .map(|a| {
                        let mut block = format!("  `{}` — {}", a.id, a.text);
                        if let Some(ev) = &a.evidence {
                            block.push_str(&format!("\n    last evidence: {ev}"));
                        }
                        if let Some(chk) = &a.last_check {
                            block.push_str(&format!(
                                "\n    last command:  {}\n    exit_code:     {}\n    observation:   {}",
                                chk.command, chk.exit_code, chk.observation
                            ));
                        }
                        block
                    })
                    .collect();
                msg.push_str(&format!(
                    "FAILED assertion(s) — diagnose each failure below and fix it \
                     before the contract can close:\n\n{}\n\n\
                     For each: re-examine what you produced, understand exactly why \
                     the check failed (look at the observation above), make the \
                     necessary edits or re-runs, then call `mark_assertion` again \
                     with state=passed. Do NOT call `close_contract` until every \
                     assertion is `passed`.\n\n",
                    blocks.join("\n\n")
                ));
            } else if !pending_ids.is_empty() {
                msg.push_str(&format!(
                    "You have {} pending assertion(s). Verify ONE now — start with \
                     `{}`. Run a check command, call `mark_assertion` with the id, \
                     state (passed/failed), the command you ran, exit_code, and \
                     actual observation. If it PASSES: stop, the next turn handles \
                     the rest. If it FAILS: keep working right now — fix the issue \
                     and re-mark before stopping.\n\n",
                    pending_ids.len(),
                    pending_ids[0]
                ));
                msg.push_str(
                    "Once every assertion is `passed`, call `close_contract` `completed`. \
                     Keep working until they all pass."
                );
            }
        }
        history.push(json!({"role": "user", "content": msg}));
        Some(GuardAction::Inject {
            message: format!("contract not closed: {} pending, {} failed — re-asking model to finish ({n}/{MAX_LOOP})", counts.pending, counts.failed),
        })
    }

    /// Check for identical-tool call loops. Returns `GuardAction::Inject` when
    /// the same tool with the same arguments has been called more than 3 times.
    pub fn check_loop_detection(
        &mut self,
        name: &str,
        args: &Value,
        id: &str,
        session: &mut AgentSessionMutable,
    ) -> Option<GuardAction> {
        let history = &mut session.history;
        let tool_repeat_counts = &mut session.tool_repeat_counts;
        let mutated_paths = &mut session.mutated_paths;
        let bash_loop_keys = &mut session.bash_loop_keys;
        use sha2::{Digest, Sha256};
        let args_str = serde_json::to_string(args).unwrap_or_default();
        let key = format!("{}{}", name, args_str);
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        let hash = format!("{:x}", h.finalize());
        let repeat_key = format!("__tool_repeat_{}", &hash[..16]);
        let max_repeats: u32 = 3;

        // If this is a read_file call to a path that was mutated since last read,
        // the file content has genuinely changed — reset the loop counter.
        if name == "read_file" {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                if mutated_paths.remove(path) {
                    tool_repeat_counts.remove(&repeat_key);
                }
            }
        }

        // Track bash loop keys so they can be cleared after mutations.
        if name == "bash" {
            bash_loop_keys.insert(repeat_key.clone());
        }

        let n = tool_repeat_counts
            .entry(repeat_key)
            .and_modify(|n| *n += 1)
            .or_insert(1);
        if *n <= max_repeats {
            return None;
        }

        let msg = format!(
            "[LOOP DETECTED] You have called `{}` with these exact \
             arguments {} time(s). The result will not change. \
             Stop repeating — try a completely different approach \
             or call a different tool.",
            name, *n
        );
        history.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": serde_json::to_string(&json!({"result": msg})).unwrap_or_default(),
        }));

        if *n == max_repeats + 3 {
            let contract_hint = if crate::settings::get().contract {
                " You are under a contract — call `contract_status` \
                  to see pending assertions, then `mark_assertion` \
                  for each one, then `close_contract`."
            } else {
                ""
            };
            history.push(json!({
                "role": "user",
                "content": format!(
                    "[SYSTEM] You are stuck in a loop calling `{}`. \
                     That tool is now DISABLED for this session — further \
                     calls will be silently dropped.{contract_hint} \
                     Pick a different tool and make progress.",
                    name
                ),
            }));
        }

        Some(GuardAction::Inject {
            message: format!("loop: {name} called {n} times with same arguments"),
        })
    }

    /// Check whether the outer turn loop should stop.
    pub fn should_stop(&self) -> bool {
        self.tool_calls_this_turn >= self.max_tool_calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_mutable(cwd: &std::path::Path) -> AgentSessionMutable {
        AgentSessionMutable {
            history: Vec::new(),
            scorer: ToolScorer::new(),
            verification: VerificationHistory::default(),
            early_stop: EarlyStopDetector::new(),
            trace: TraceRecorder::new(cwd.to_path_buf()),
            current_tool_category: None,
            fullscreen: None,
            tool_repeat_counts: HashMap::new(),
            mutated_paths: HashSet::new(),
            bash_loop_keys: HashSet::new(),
            readonly_turn_count: 0,
            total_mutating_calls: 0,
            snapshot_manager: Arc::new(SnapshotManager::new(cwd.to_path_buf())),
        }
    }

    /// Bug #3 regression: the previous implementation cloned
    /// `tool_repeat_counts`, removed keys from the clone, and discarded it
    /// — so the next bash call would still see the old counter and trigger
    /// a false stuck-loop detection. The fix mutates the real maps; this
    /// test pins that behaviour.
    #[test]
    fn reset_bash_loop_actually_clears_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = fresh_mutable(tmp.path());

        m.tool_repeat_counts.insert("bash:cargo test".into(), 4);
        m.tool_repeat_counts.insert("bash:ls".into(), 1);
        m.tool_repeat_counts.insert("read:foo.rs".into(), 2);
        m.bash_loop_keys.insert("bash:cargo test".into());
        m.bash_loop_keys.insert("bash:ls".into());

        m.reset_bash_loop_after_mutation();

        assert!(m.bash_loop_keys.is_empty(), "bash_loop_keys must be drained");
        assert!(m.tool_repeat_counts.get("bash:cargo test").is_none(),
            "bash counter must be removed (not just zeroed in a clone)");
        assert!(m.tool_repeat_counts.get("bash:ls").is_none(),
            "second bash counter must be removed");
        // Non-bash counters are untouched — only the tracked bash keys reset.
        assert_eq!(m.tool_repeat_counts.get("read:foo.rs").copied(), Some(2),
            "non-tracked counters must survive the reset");
    }

    /// Calling `reset_bash_loop_after_mutation` on empty state is a no-op
    /// and does not panic.
    #[test]
    fn reset_bash_loop_on_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = fresh_mutable(tmp.path());
        m.reset_bash_loop_after_mutation();
        assert!(m.bash_loop_keys.is_empty());
        assert!(m.tool_repeat_counts.is_empty());
    }

    // ── TurnState::check_quality_monitor ───────────────────────────────────

    fn fresh_state() -> TurnState {
        TurnState::new("test".into(), "coding", None, 100)
    }

    /// Valid tool names produce no GuardAction.
    #[test]
    fn quality_monitor_accepts_known_tools() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let known = ["bash", "read_file", "write_file"];
        let calls = vec![json!({"id":"1","function":{"name":"bash","arguments":"{}"}})];
        assert!(s.check_quality_monitor(&calls, &known, &mut history).is_none());
        assert!(history.is_empty(), "no history mutation on valid calls");
    }

    /// An unknown tool name produces an Inject GuardAction and pushes a
    /// corrective tool message into history.
    #[test]
    fn quality_monitor_rejects_unknown_tool_with_inject() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let known = ["bash"];
        let calls = vec![json!({"id":"x","function":{"name":"frobnicate","arguments":"{}"}})];
        let action = s.check_quality_monitor(&calls, &known, &mut history);
        assert!(matches!(action, Some(GuardAction::Inject { .. })));
        assert_eq!(history.len(), 2, "must push 1 tool reply + 1 user nudge");
        // Tool reply mentions the bad tool name.
        let tool_msg = history[0]["content"].as_str().unwrap();
        assert!(tool_msg.contains("frobnicate"));
        assert!(tool_msg.contains("bash"), "must list valid tools");
    }

    /// Empty tool name is also flagged.
    #[test]
    fn quality_monitor_rejects_empty_tool_name() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let known = ["bash"];
        let calls = vec![json!({"id":"x","function":{"name":"","arguments":"{}"}})];
        let action = s.check_quality_monitor(&calls, &known, &mut history);
        assert!(matches!(action, Some(GuardAction::Inject { .. })));
        assert!(history[0]["content"].as_str().unwrap().contains("empty"));
    }

    /// After 2 corrections, the third invocation does NOT inject again
    /// (give up — the model is stuck).
    #[test]
    fn quality_monitor_gives_up_after_two_corrections() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let known = ["bash"];
        let bad = vec![json!({"id":"x","function":{"name":"xxx","arguments":"{}"}})];

        assert!(matches!(s.check_quality_monitor(&bad, &known, &mut history),
            Some(GuardAction::Inject { .. })));
        assert!(matches!(s.check_quality_monitor(&bad, &known, &mut history),
            Some(GuardAction::Inject { .. })));
        // Third time: no GuardAction.
        assert!(s.check_quality_monitor(&bad, &known, &mut history).is_none(),
            "third call must NOT keep nudging — model is hopelessly stuck");
    }

    // ── TurnState::check_idempotent_write ──────────────────────────────────

    /// Non-idempotent tools are NEVER skipped (bash, read_file, etc.).
    #[test]
    fn idempotent_check_passes_through_non_idempotent_tools() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let args = json!({"command": "ls"});
        assert!(s.check_idempotent_write("bash", &args, "x", &mut history).is_none());
        // Calling again with same args → still passes through (not idempotent).
        assert!(s.check_idempotent_write("bash", &args, "y", &mut history).is_none());
    }

    /// First memory_remember with given args is allowed; second is Skip.
    #[test]
    fn idempotent_check_dedupes_memory_remember() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let args = json!({"type":"decision","title":"x","content":"y"});
        // First time → allowed.
        assert!(s.check_idempotent_write("memory_remember", &args, "1", &mut history).is_none());
        // Second time with same args → Skip.
        let r = s.check_idempotent_write("memory_remember", &args, "2", &mut history);
        assert!(matches!(r, Some(GuardAction::Skip)),
            "duplicate memory_remember must be Skip; got {r:?}");
        // The skip pushes an explanatory tool message.
        assert_eq!(history.len(), 1);
        assert!(history[0]["content"].as_str().unwrap().contains("identical call skipped")
            || history[0]["content"].as_str().unwrap().contains("DUPLICATE"));
    }

    /// Different args → not deduped (it's a distinct write).
    #[test]
    fn idempotent_check_doesnt_dedup_different_args() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let args_a = json!({"type":"decision","title":"a","content":"x"});
        let args_b = json!({"type":"decision","title":"b","content":"y"});
        assert!(s.check_idempotent_write("memory_remember", &args_a, "1", &mut history).is_none());
        assert!(s.check_idempotent_write("memory_remember", &args_b, "2", &mut history).is_none(),
            "different args must NOT collapse to the same idempotent key");
    }

    /// mark_assertion duplicate has a more detailed skip message naming the
    /// assertion id and state.
    #[test]
    fn idempotent_check_mark_assertion_message_includes_id() {
        let mut s = fresh_state();
        let mut history = Vec::new();
        let args = json!({"id":"A.001","state":"passed","evidence":"ran ok"});
        let _ = s.check_idempotent_write("mark_assertion", &args, "1", &mut history);
        let r = s.check_idempotent_write("mark_assertion", &args, "2", &mut history);
        assert!(matches!(r, Some(GuardAction::Skip)));
        let payload = history[0]["content"].as_str().unwrap();
        assert!(payload.contains("A.001"), "must name the assertion id; got {payload}");
        assert!(payload.contains("passed"), "must name the state; got {payload}");
    }

    // ── TurnState::should_stop ────────────────────────────────────────────

    #[test]
    fn should_stop_at_max_tool_calls() {
        let mut s = TurnState::new("t".into(), "coding", None, 3);
        assert!(!s.should_stop());
        s.tool_calls_this_turn = 2;
        assert!(!s.should_stop());
        s.tool_calls_this_turn = 3;
        assert!(s.should_stop(), "must stop AT max (>= comparison)");
        s.tool_calls_this_turn = 4;
        assert!(s.should_stop(), "must remain stopped above max");
    }

    // ── TurnState::new initial state ──────────────────────────────────────

    #[test]
    fn turn_state_new_has_clean_counters() {
        let s = TurnState::new("hi".into(), "coding", Some("plan".into()), 50);
        assert_eq!(s.user_msg, "hi");
        assert_eq!(s.augmented, "hi", "augmented must mirror user_msg initially");
        assert_eq!(s.task_type, "coding");
        assert_eq!(s.current_category.as_deref(), Some("plan"));
        assert_eq!(s.tool_calls_this_turn, 0);
        assert_eq!(s.max_tool_calls, 50);
        assert_eq!(s.consecutive_blocks, 0);
        assert!(!s.had_mutating_call);
        assert!(!s.empty_retry_injected);
        assert!(s.edited_files.is_empty());
        assert!(s.per_turn_repeats.is_empty());
        assert!(s.per_turn_write_seen.is_empty());
        assert!(s.recent_response_sigs.is_empty());
    }

    // ── stuck-repetition detector ─────────────────────────────────────────

    /// Three identical responses in a row trip the stuck flag. Anti-
    /// regression for the regex-log-VEmMyGL pattern where the same
    /// completion repeated six times until wallclock killed it.
    #[test]
    fn note_response_flags_stuck_on_three_identical() {
        let mut s = TurnState::new("hi".into(), "coding", None, 50);
        let tc = vec![json!({
            "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
        })];
        assert!(!s.note_response_and_check_stuck("same content", &tc));
        assert!(!s.note_response_and_check_stuck("same content", &tc));
        assert!(s.note_response_and_check_stuck("same content", &tc),
            "3rd identical response must trip stuck");
    }

    /// Different content breaks the streak.
    #[test]
    fn note_response_resets_when_content_differs() {
        let mut s = TurnState::new("hi".into(), "coding", None, 50);
        let tc: Vec<Value> = vec![];
        assert!(!s.note_response_and_check_stuck("a", &tc));
        assert!(!s.note_response_and_check_stuck("a", &tc));
        // Different content interrupts the streak.
        assert!(!s.note_response_and_check_stuck("b", &tc));
        // Now back to "a" — only 1 in the window of 3, so not stuck.
        assert!(!s.note_response_and_check_stuck("a", &tc));
        assert!(!s.note_response_and_check_stuck("a", &tc));
        // Third identical "a" lands → stuck. The window is the last 3, and
        // all 3 are "a" now.
        assert!(s.note_response_and_check_stuck("a", &tc));
    }

    /// Different tool-call args also break identity — same text content
    /// with a different tool_call argument is NOT stuck.
    #[test]
    fn note_response_distinguishes_tool_call_args() {
        let mut s = TurnState::new("hi".into(), "coding", None, 50);
        let tc1 = vec![json!({"function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}})];
        let tc2 = vec![json!({"function":{"name":"read_file","arguments":"{\"path\":\"b\"}"}})];
        assert!(!s.note_response_and_check_stuck("X", &tc1));
        assert!(!s.note_response_and_check_stuck("X", &tc2));
        assert!(!s.note_response_and_check_stuck("X", &tc1),
            "alternating tool args is NOT stuck");
    }
}
