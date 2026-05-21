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

const SCORES_FILE_REL: &str = ".itsy/tool_scores.json";

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
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(SCORES_FILE_REL)
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
    let ext = Path::new(file_path).extension().and_then(|s| s.to_str()).unwrap_or("");
    let mut result = VerifyResult {
        passed: false,
        confidence: 0.0,
        compiled: false,
        executed: false,
        errors: Vec::new(),
    };

    let compile_exec: Option<(&str, Vec<String>)> = match ext {
        "py" => Some(("python", vec!["-m".into(), "py_compile".into(), full.to_string_lossy().into()])),
        "js" | "mjs" => Some(("node", vec!["--check".into(), full.to_string_lossy().into()])),
        "ts" | "tsx" => Some(("npx", vec!["tsc".into(), "--noEmit".into(), full.to_string_lossy().into()])),
        "go" => Some(("go", vec!["build".into(), full.to_string_lossy().into()])),
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
        match run_with_timeout(cmd, &args, &cwd, Duration::from_secs(15)) {
            Ok(()) => result.compiled = true,
            Err(e) => result.errors.push(truncate(&e, 500)),
        }
    }

    if result.compiled && (ext == "py" || ext == "js") {
        if let Ok(content) = fs::read_to_string(&full) {
            let has_main_guard = content.contains("__name__") || content.contains("main()") || content.contains("console.log");
            if has_main_guard {
                let (cmd, args) = if ext == "py" {
                    ("python", vec![full.to_string_lossy().to_string()])
                } else {
                    ("node", vec![full.to_string_lossy().to_string()])
                };
                match run_with_timeout(cmd, &args, &cwd, Duration::from_secs(10)) {
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

fn run_with_timeout(cmd: &str, args: &[String], cwd: &Path, _timeout: Duration) -> Result<(), String> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Err(combined)
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
    Regex::new(r"(?i)\b(python|django|fastapi|flask|go|golang|rust|actix|axum|ruby|rails|php|laravel|java|spring|c#|dotnet|asp\.net|elixir|phoenix)\b").unwrap()
});
static BACKEND_A: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(api|backend|server|rest|crud|auth|database|endpoint|express|fastify|node|typescript|ts)\b.*\b(create|build|make|implement|set up)\b").unwrap());
static BACKEND_B: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(create|build|make)\b.*\b(api|backend|server|rest|crud|endpoint)\b").unwrap());
static BACKEND_C: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(node|typescript|ts|express|fastify)\b.*\b(api|backend|server|rest|crud)\b").unwrap());
static CODING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(create|write|build|make|implement|add)\b.*\b(file|function|class|module|component|api|server)\b").unwrap());
static EDITING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(fix|patch|edit|change|update|modify|replace|rename)\b").unwrap());
static SEARCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(find|search|grep|where|which|look for)\b").unwrap());
static SHELL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(run|execute|test|install|build|compile|deploy)\b").unwrap());
static EXPLANATION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(explain|what|how|why|describe|show me)\b").unwrap());
static MULTI_STEP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(and then|then|after that|also|plus|step)\b.*\b(and then|then|also)\b").unwrap());
static DEBUGGING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(debug|fix|error|bug|crash|broken|failing)\b").unwrap());

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
