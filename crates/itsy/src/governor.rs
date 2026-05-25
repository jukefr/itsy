//! Tool-scoring, verification, hard-fail, and the
//! synchronous task classifier.

pub mod early_stop;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};


#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ToolScore {
    pub tool_name: String,
    pub task_type: String,
    pub success_count: u32,
    pub failure_count: u32,
    pub total_calls: u32,
    pub confidence: f64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct ToolScorer {
    pub scores: HashMap<String, ToolScore>,
}

impl ToolScorer {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.load();
        s
    }

    fn key(tool_name: &str, task_type: &str) -> String {
        format!("{tool_name}:{task_type}")
    }

    pub fn record_success(&mut self, tool_name: &str, task_type: &str, _latency_ms: u64) {
        let entry = self.entry(tool_name, task_type);
        entry.success_count += 1;
        entry.total_calls += 1;
        entry.confidence = (entry.success_count + 1) as f64 / (entry.total_calls + 2) as f64;
        self.save();
    }

    pub fn record_failure(&mut self, tool_name: &str, task_type: &str, error: &str) {
        let entry = self.entry(tool_name, task_type);
        entry.failure_count += 1;
        entry.total_calls += 1;
        entry.confidence = (entry.success_count + 1) as f64 / (entry.total_calls + 2) as f64;
        entry.last_error = Some(error.to_string());
        self.save();
    }

    pub fn should_avoid(&self, tool_name: &str, task_type: &str) -> bool {
        let key = Self::key(tool_name, task_type);
        match self.scores.get(&key) {
            Some(s) => s.total_calls >= 3 && s.confidence < 0.35,
            None => false,
        }
    }

    pub fn get_score(&self, tool_name: &str, task_type: &str) -> f64 {
        let key = Self::key(tool_name, task_type);
        match self.scores.get(&key) {
            Some(s) => s.confidence.min(0.95),
            None => 0.65,
        }
    }

    fn entry(&mut self, tool_name: &str, task_type: &str) -> &mut ToolScore {
        let key = Self::key(tool_name, task_type);
        self.scores.entry(key).or_insert(ToolScore {
            tool_name: tool_name.into(),
            task_type: task_type.into(),
            success_count: 0,
            failure_count: 0,
            total_calls: 0,
            confidence: 0.5,
            last_error: None,
        })
    }

    pub fn load(&mut self) {
        let path = scores_path();
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, ToolScore>>(&content) {
                self.scores = map;
            }
        }
    }

    pub fn save(&self) {
        let path = scores_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.scores) {
            let _ = fs::write(path, json);
        }
    }
}

fn scores_path() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::paths::tool_scores(&cwd)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub passed: bool,
    pub confidence: f64,
    pub compiled: bool,
    pub executed: bool,
    pub errors: Vec<String>,
}

pub fn verify_code(file_path: &str) -> VerifyResult {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let full = cwd.join(file_path);
    if !full.exists() {
        return VerifyResult {
            passed: false,
            confidence: 0.0,
            compiled: false,
            executed: false,
            errors: vec!["File not found".into()],
        };
    }
    // Run verification in a temp sandbox to isolate from project files.
    let sandbox = tempfile::tempdir().ok();
    let workdir = sandbox.as_ref().map(|s| s.path().to_path_buf()).unwrap_or_else(|| cwd.clone());
    let sandbox_file = if sandbox.is_some() {
        let dest = workdir.join(file_path);
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::copy(&full, &dest);
        dest
    } else {
        full.clone()
    };

    let ext = Path::new(file_path).extension().and_then(|s| s.to_str()).unwrap_or("");
    let mut result = VerifyResult {
        passed: false,
        confidence: 0.0,
        compiled: false,
        executed: false,
        errors: Vec::new(),
    };

    let compile_exec: Option<(&str, Vec<String>)> = match ext {
        "py" => Some(("python", vec!["-m".into(), "py_compile".into(), sandbox_file.to_string_lossy().into()])),
        "js" | "mjs" => Some(("node", vec!["--check".into(), sandbox_file.to_string_lossy().into()])),
        "ts" | "tsx" => Some(("npx", vec!["tsc".into(), "--noEmit".into(), sandbox_file.to_string_lossy().into()])),
        "go" => Some(("go", vec!["build".into(), sandbox_file.to_string_lossy().into()])),
        "json" => {
            match fs::read_to_string(&full) {
                Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(_) => {
                        result.compiled = true;
                    }
                    Err(e) => result.errors.push(e.to_string()),
                },
                Err(e) => result.errors.push(e.to_string()),
            }
            None
        }
        _ => {
            result.compiled = true;
            None
        }
    };

    if let Some((cmd, args)) = compile_exec {
        match run_with_timeout(cmd, &args, &workdir, Duration::from_secs(15)) {
            Ok(()) => result.compiled = true,
            Err(e) => result.errors.push(truncate(&e, 500)),
        }
    }

    if result.compiled && (ext == "py" || ext == "js") {
        if let Ok(content) = fs::read_to_string(&sandbox_file) {
            let has_main_guard = content.contains("__name__") || content.contains("main()") || content.contains("console.log");
            if has_main_guard {
                let (cmd, args) = if ext == "py" {
                    ("python", vec![sandbox_file.to_string_lossy().to_string()])
                } else {
                    ("node", vec![sandbox_file.to_string_lossy().to_string()])
                };
                match run_with_timeout(cmd, &args, &workdir, Duration::from_secs(10)) {
                    Ok(()) => result.executed = true,
                    Err(e) => result.errors.push(format!("Runtime error: {}", truncate(&e, 300))),
                }
            } else {
                result.executed = true;
            }
        }
    } else if result.compiled {
        result.executed = true;
    }

    result.confidence = (if result.compiled { 0.4 } else { 0.0 })
        + (if result.executed { 0.4 } else { 0.0 })
        + (if result.errors.is_empty() { 0.2 } else { 0.0 });
    result.passed = result.compiled && result.executed;
    result
}

fn run_with_timeout(cmd: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .spawn()
        .map_err(|e| e.to_string())?;

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            return Err("timed out".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                } else {
                    return Err(format!("exit code: {}", status.code().unwrap_or(-1)));
                }
            }
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HardFailAction {
    Accept { confidence: f64 },
    Retry { errors: Vec<String>, attempt: u32, escalate: bool },
    Decompose { errors: Vec<String>, file_content: String, lines: usize, strategy: DecomposeStrategy },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeStrategy {
    #[serde(rename = "type")]
    pub kind: String,
    pub reason: String,
    pub instruction: String,
}

const MAX_VERIFICATION_RETRIES: u32 = 2;
const MAX_TRACKED_FILES: usize = 50;

#[derive(Default)]
pub struct VerificationHistory {
    pub by_file: HashMap<String, Vec<VerifyResult>>,
}

impl VerificationHistory {
    pub fn check_and_enforce(&mut self, file_path: &str) -> HardFailAction {
        let result = verify_code(file_path);
        let entry = self.by_file.entry(file_path.to_string()).or_default();
        entry.push(result.clone());
        // bound history
        if self.by_file.len() > MAX_TRACKED_FILES {
            let keys: Vec<String> = self.by_file.keys().cloned().collect();
            for k in keys.iter().take(self.by_file.len() - MAX_TRACKED_FILES) {
                self.by_file.remove(k);
            }
        }
        if result.passed {
            self.by_file.insert(file_path.to_string(), Vec::new());
            return HardFailAction::Accept { confidence: result.confidence };
        }
        let attempts = self.by_file.get(file_path).map(|v| v.len() as u32).unwrap_or(0);
        if attempts >= MAX_VERIFICATION_RETRIES {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let content = fs::read_to_string(cwd.join(file_path)).unwrap_or_default();
            let lines = content.split('\n').count();
            let errors = result.errors.clone();
            let strategy = pick_decompose_strategy(&content, &errors, file_path);
            self.by_file.insert(file_path.to_string(), Vec::new());
            return HardFailAction::Decompose { errors, file_content: content, lines, strategy };
        }
        HardFailAction::Retry { errors: result.errors, attempt: attempts, escalate: attempts >= 2 }
    }
}

pub fn pick_decompose_strategy(content: &str, errors: &[String], file_path: &str) -> DecomposeStrategy {
    let _ext = Path::new(file_path).extension().and_then(|s| s.to_str()).unwrap_or("");
    let lines = content.split('\n').count();
    let error_count = errors.len();
    if lines > 80 {
        return DecomposeStrategy {
            kind: "split_file".into(),
            reason: format!("File is {lines} lines with {error_count} errors. Too much for one pass."),
            instruction: format!(
                "The file {file_path} is too complex to fix in one go ({lines} lines, {error_count} errors).\n\
                 Split it into smaller files:\n\
                 1. First, extract the working parts into a separate file\n\
                 2. Then fix the broken parts in isolation\n\
                 3. Import between them\n\n\
                 Start by identifying which functions/sections are correct and which have errors."
            ),
        };
    }
    if error_count > 1 {
        let first = errors.first().cloned().unwrap_or_default();
        return DecomposeStrategy {
            kind: "one_error_at_a_time".into(),
            reason: format!("{error_count} errors found. Fix them one at a time."),
            instruction: format!(
                "Stop trying to fix everything at once. Focus on ONE error only:\n\n\
                 ERROR: {first}\n\n\
                 Fix ONLY this one error. Don't touch anything else. After this is fixed, I'll tell you the next one."
            ),
        };
    }
    DecomposeStrategy {
        kind: "rewrite_section".into(),
        reason: "Same error persists after 2 attempts.".into(),
        instruction: format!(
            "The fix attempts aren't working. Try a completely different approach:\n\
             1. Delete the broken section entirely\n\
             2. Rewrite it from scratch using a simpler implementation\n\
             3. Don't copy the old logic — start fresh\n\n\
             Error that won't go away: {}",
            errors.first().cloned().unwrap_or_else(|| "unknown".into())
        ),
    }
}

// ─── Task classifier ────────────────────────────────────────────────────────

static NON_NODE_BACKEND: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(python|django|fastapi|flask|go|golang|rust|actix|axum|ruby|rails|php|laravel|java|spring|c#|dotnet|asp\.net|elixir|phoenix)\b").expect("valid regex literal")
});
static BACKEND_A: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(api|backend|server|rest|crud|auth|database|endpoint|express|fastify|node|typescript|ts)\b.*\b(create|build|make|implement|set up)\b").expect("valid regex literal"));
static BACKEND_B: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(create|build|make)\b.*\b(api|backend|server|rest|crud|endpoint)\b").expect("valid regex literal"));
static BACKEND_C: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(node|typescript|ts|express|fastify)\b.*\b(api|backend|server|rest|crud)\b").expect("valid regex literal"));
static CODING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(create|write|build|make|implement|add)\b.*\b(file|function|class|module|component|api|server)\b").expect("valid regex literal"));
static EDITING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(fix|patch|edit|change|update|modify|replace|rename)\b").expect("valid regex literal"));
static SEARCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(find|search|grep|where|which|look for)\b").expect("valid regex literal"));
static SHELL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(run|execute|test|install|build|compile|deploy)\b").expect("valid regex literal"));
static EXPLANATION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(explain|what|how|why|describe|show me)\b").expect("valid regex literal"));
static MULTI_STEP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(and then|then|after that|also|plus|step)\b.*\b(and then|then|also)\b").expect("valid regex literal"));
static DEBUGGING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(debug|fix|error|bug|crash|broken|failing)\b").expect("valid regex literal"));

pub fn classify_task(user_message: &str) -> &'static str {
    let msg = user_message;
    let has_non_node = NON_NODE_BACKEND.is_match(msg);
    if !has_non_node && (BACKEND_A.is_match(msg) || BACKEND_B.is_match(msg) || BACKEND_C.is_match(msg)) {
        return "backend";
    }
    if CODING.is_match(msg) {
        return "coding";
    }
    if EDITING.is_match(msg) {
        return "editing";
    }
    if SEARCH.is_match(msg) {
        return "search";
    }
    if SHELL.is_match(msg) {
        return "shell";
    }
    if EXPLANATION.is_match(msg) {
        return "explanation";
    }
    if MULTI_STEP.is_match(msg) {
        return "multi_step";
    }
    if DEBUGGING.is_match(msg) {
        return "debugging";
    }
    "coding"
}

pub async fn classify_task_async(user_message: &str) -> &'static str {
    // The JS version optionally delegates to a compiled LLM classifier with a
    // synchronous regex fallback. The Rust port skips the LLM call and always
    // returns the deterministic fallback — callers that need LLM classification
    // dispatch it from `crate::model::router`.
    classify_task(user_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_task: each branch ────────────────────────────────────────

    #[test]
    fn classifies_python_create_as_coding_not_backend() {
        // Python (non-node backend) creating an API stays in `coding`, not `backend`.
        // The backend branch is reserved for node/typescript stacks.
        assert_eq!(classify_task("create a python API"), "coding");
    }

    #[test]
    fn classifies_node_api_as_backend() {
        assert_eq!(classify_task("create a node API"), "backend");
        assert_eq!(classify_task("build a typescript express server"), "backend");
    }

    #[test]
    fn classifies_explanation_intents() {
        for msg in ["explain what this does", "how does X work", "why is this failing", "describe the architecture"] {
            assert_eq!(classify_task(msg), "explanation", "msg: {msg}");
        }
    }

    #[test]
    fn classifies_search_intents() {
        for msg in ["find the auth code", "search for foo", "where is bar defined"] {
            assert_eq!(classify_task(msg), "search", "msg: {msg}");
        }
    }

    #[test]
    fn classifies_editing_intents() {
        for msg in ["fix the bug", "rename foo to bar", "patch this function"] {
            // These match EDITING/DEBUGGING; "fix" hits debugging-style words too,
            // but EDITING comes first in the chain.
            let r = classify_task(msg);
            assert!(r == "editing" || r == "debugging", "msg={msg} got {r}");
        }
    }

    #[test]
    fn default_is_coding_for_uncategorized() {
        // Generic short messages with no triggers fall through to default.
        assert_eq!(classify_task("hmm interesting"), "coding");
    }

    // ── pick_decompose_strategy: branches ────────────────────────────────

    #[test]
    fn decompose_picks_split_file_for_large_files() {
        let big = (0..120).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let s = pick_decompose_strategy(&big, &["x".into()], "huge.py");
        assert_eq!(s.kind, "split_file",
            "files >80 lines must use split_file; got {}", s.kind);
        assert!(s.instruction.contains("huge.py"));
    }

    #[test]
    fn decompose_picks_one_error_at_a_time_for_multiple_errors() {
        let small = "def f():\n    pass\n";
        let errs = vec!["err A".to_string(), "err B".to_string(), "err C".to_string()];
        let s = pick_decompose_strategy(small, &errs, "small.py");
        assert_eq!(s.kind, "one_error_at_a_time",
            "small file with >1 errors must use one_error_at_a_time; got {}", s.kind);
        assert!(s.instruction.contains("err A"),
            "instruction must include the first error");
    }

    #[test]
    fn decompose_picks_rewrite_section_for_single_persistent_error() {
        let small = "def f():\n    pass\n";
        let errs = vec!["only error".to_string()];
        let s = pick_decompose_strategy(small, &errs, "small.py");
        assert_eq!(s.kind, "rewrite_section",
            "small file with one error must use rewrite_section; got {}", s.kind);
        assert!(s.instruction.contains("only error"));
    }

    // ── ToolScore confidence math ─────────────────────────────────────────

    #[test]
    fn record_success_raises_confidence_above_default() {
        // Use Default to avoid loading from disk; we only test in-memory math.
        let mut s = ToolScorer::default();
        for _ in 0..5 {
            s.record_success("bash", "coding", 100);
        }
        let score = s.get_score("bash", "coding");
        assert!(score > 0.5, "5 successes should push score above 0.5, got {score}");
    }

    #[test]
    fn record_failure_lowers_confidence() {
        let mut s = ToolScorer::default();
        for _ in 0..5 {
            s.record_failure("bash", "coding", "boom");
        }
        let score = s.get_score("bash", "coding");
        assert!(score < 0.5, "5 failures should push score below 0.5, got {score}");
    }

    #[test]
    fn should_avoid_only_triggers_after_three_calls() {
        let mut s = ToolScorer::default();
        // 2 failures: not enough samples to avoid yet.
        s.record_failure("bash", "coding", "x");
        s.record_failure("bash", "coding", "x");
        assert!(!s.should_avoid("bash", "coding"),
            "should_avoid requires >=3 calls AND confidence<0.35; 2 samples shouldn't trigger");

        // 3rd+ failure: now confidence sinks low enough.
        s.record_failure("bash", "coding", "x");
        s.record_failure("bash", "coding", "x");
        assert!(s.should_avoid("bash", "coding"),
            "4 failures must trigger should_avoid");
    }

    #[test]
    fn get_score_returns_default_for_unknown_tool() {
        let s = ToolScorer::default();
        assert_eq!(s.get_score("never_called", "coding"), 0.65,
            "unknown tool/task must return the 0.65 default");
    }

    #[test]
    fn get_score_caps_at_95() {
        let mut s = ToolScorer::default();
        for _ in 0..1000 {
            s.record_success("bash", "coding", 1);
        }
        let score = s.get_score("bash", "coding");
        assert!(score <= 0.95,
            "score must cap at 0.95, got {score}");
    }
}
