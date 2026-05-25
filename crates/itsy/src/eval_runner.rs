//! Prompt evaluation runner — `--eval <suite>` CLI mode. Three built-in
//! suites: classify_accuracy, tool_selection, response_quality. Mirrors
//! `bin/eval_runner.js`.

use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::Value;

use crate::Config;

#[derive(Debug, Clone)]
pub struct ClassifyCase {
    pub input: &'static str,
    pub expected: &'static str,
    pub tolerance: &'static [&'static str],
}

#[derive(Debug, Clone)]
pub struct ToolCase {
    pub input: &'static str,
    pub expected_tool: &'static str,
}

#[derive(Debug, Clone)]
pub struct QualityCase {
    pub input: &'static str,
    pub check: &'static str, // "length>50" or "contains:foo|bar"
    pub desc: &'static str,
}

pub static CLASSIFY_ACCURACY: Lazy<Vec<ClassifyCase>> = Lazy::new(|| {
    vec![
        ClassifyCase { input: "fix the typo in main.ts", expected: "coding", tolerance: &["coding", "editing", "refactoring"] },
        ClassifyCase { input: "explain what this function does", expected: "explanation", tolerance: &["explanation"] },
        ClassifyCase { input: "refactor the database module", expected: "editing", tolerance: &["editing", "coding", "refactoring"] },
        ClassifyCase { input: "write unit tests for auth", expected: "coding", tolerance: &["coding"] },
        ClassifyCase { input: "deploy to production", expected: "shell", tolerance: &["shell", "coding"] },
        ClassifyCase { input: "what is dependency injection?", expected: "explanation", tolerance: &["explanation"] },
        ClassifyCase { input: "add error handling to the API", expected: "coding", tolerance: &["coding"] },
        ClassifyCase { input: "why is the build failing?", expected: "debugging", tolerance: &["debugging"] },
        ClassifyCase { input: "rename getUserData to fetchUser", expected: "editing", tolerance: &["editing", "coding", "refactoring"] },
        ClassifyCase { input: "create a new React component", expected: "coding", tolerance: &["coding"] },
    ]
});

pub static TOOL_SELECTION: Lazy<Vec<ToolCase>> = Lazy::new(|| {
    vec![
        ToolCase { input: "read the contents of package.json", expected_tool: "read_file" },
        ToolCase { input: "find all uses of useState", expected_tool: "search" },
        ToolCase { input: "create a new file called utils.ts", expected_tool: "write_file" },
        ToolCase { input: "run the test suite", expected_tool: "bash" },
        ToolCase { input: "change the function name from foo to bar", expected_tool: "patch" },
        ToolCase { input: "list all files in the project", expected_tool: "list_projects" },
        ToolCase { input: "search for the error message", expected_tool: "search" },
        ToolCase { input: "install a new package", expected_tool: "bash" },
    ]
});

pub static RESPONSE_QUALITY: Lazy<Vec<QualityCase>> = Lazy::new(|| {
    vec![
        QualityCase { input: "explain closures in javascript", check: "length>50", desc: "response should be substantial" },
        QualityCase { input: "fix: const x = 1; x = 2;", check: "contains:const|let", desc: "should suggest const→let" },
        QualityCase { input: "2 + 2", check: "contains:4", desc: "basic math" },
    ]
});

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub input: String,
    pub passed: bool,
    pub got: String,
    pub expected: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteResult {
    pub suite: String,
    pub name: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub score: String,
    pub cases: Vec<CaseResult>,
}

pub struct EvalRunner<'a> {
    pub config: &'a Config,
    pub results: Vec<SuiteResult>,
}

impl<'a> EvalRunner<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config, results: Vec::new() }
    }

    /// Run the classify_accuracy suite (deterministic — no model call).
    pub fn run_classify(&mut self) -> SuiteResult {
        let mut cases = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        for c in CLASSIFY_ACCURACY.iter() {
            let got = crate::governor::classify_task(c.input);
            let ok = c.tolerance.contains(&got);
            if ok { passed += 1 } else { failed += 1 }
            cases.push(CaseResult {
                input: c.input.into(),
                passed: ok,
                got: got.into(),
                expected: Some(c.expected.into()),
            });
        }
        let total = CLASSIFY_ACCURACY.len();
        let score = format!("{passed}/{total} ({}%)", passed * 100 / total.max(1));
        let r = SuiteResult {
            suite: "classify_accuracy".into(),
            name: "Task Classification Accuracy".into(),
            total,
            passed,
            failed,
            score,
            cases,
        };
        self.results.push(r.clone());
        r
    }

    /// Run the tool_selection suite — requires a `chat_fn` that sends a
    /// message to the model and returns its first tool call name.
    pub async fn run_tool_selection<F, Fut>(&mut self, mut chat_fn: F) -> SuiteResult
    where
        F: FnMut(&Config, &str) -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        let mut cases = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        for c in TOOL_SELECTION.iter() {
            let got = chat_fn(self.config, c.input).await.unwrap_or_else(|| "(no tool)".into());
            let ok = got == c.expected_tool;
            if ok { passed += 1 } else { failed += 1 }
            cases.push(CaseResult {
                input: c.input.into(),
                passed: ok,
                got,
                expected: Some(c.expected_tool.into()),
            });
        }
        let total = TOOL_SELECTION.len();
        let score = format!("{passed}/{total} ({}%)", passed * 100 / total.max(1));
        let r = SuiteResult {
            suite: "tool_selection".into(),
            name: "Tool Selection Quality".into(),
            total,
            passed,
            failed,
            score,
            cases,
        };
        self.results.push(r.clone());
        r
    }

    /// Run the response_quality suite — requires a `chat_fn` that returns
    /// the model's text reply.
    pub async fn run_response_quality<F, Fut>(&mut self, mut chat_fn: F) -> SuiteResult
    where
        F: FnMut(&Config, &str) -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        let mut cases = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        for c in RESPONSE_QUALITY.iter() {
            let content = chat_fn(self.config, c.input).await.unwrap_or_default();
            let ok = if let Some(rest) = c.check.strip_prefix("length>") {
                rest.parse::<usize>().map(|n| content.len() > n).unwrap_or(false)
            } else if let Some(rest) = c.check.strip_prefix("contains:") {
                let needles: Vec<&str> = rest.split('|').collect();
                let lc = content.to_lowercase();
                needles.iter().any(|n| lc.contains(n))
            } else {
                false
            };
            if ok { passed += 1 } else { failed += 1 }
            cases.push(CaseResult {
                input: c.input.into(),
                passed: ok,
                got: content.chars().take(100).collect(),
                expected: Some(c.desc.into()),
            });
        }
        let total = RESPONSE_QUALITY.len();
        let score = format!("{passed}/{total} ({}%)", passed * 100 / total.max(1));
        let r = SuiteResult {
            suite: "response_quality".into(),
            name: "Response Quality".into(),
            total,
            passed,
            failed,
            score,
            cases,
        };
        self.results.push(r.clone());
        r
    }
}

/// Pretty-print a suite result with ANSI colours.
pub fn format_results(r: &SuiteResult) -> String {
    let mut lines = vec![
        format!("  {}", r.name),
        format!("  Score: {}", r.score),
        format!("  {}", "─".repeat(40)),
    ];
    for c in &r.cases {
        let (mark, color) = if c.passed { ("✓", "\x1b[32m") } else { ("✗", "\x1b[31m") };
        let exp = c.expected.as_deref().map(|e| format!(" (exp: {e})")).unwrap_or_default();
        let input_short: String = c.input.chars().take(50).collect();
        lines.push(format!("  {color}{mark}\x1b[0m {input_short} → {}{exp}", c.got));
    }
    lines.join("\n")
}

/// Look up a suite by name. Returns an error string with available names.
pub fn known_suite(name: &str) -> Result<&'static str, String> {
    match name {
        "classify_accuracy" => Ok("classify_accuracy"),
        "tool_selection" => Ok("tool_selection"),
        "response_quality" => Ok("response_quality"),
        other => Err(format!(
            "Unknown suite: {other}. Available: classify_accuracy, tool_selection, response_quality"
        )),
    }
}

// Surface a richer value type when called from CLI paths that need it.
pub fn suite_to_value(r: &SuiteResult) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `known_suite` returns the canonical name for valid suites.
    #[test]
    fn known_suite_recognises_canonical_names() {
        assert_eq!(known_suite("classify_accuracy"), Ok("classify_accuracy"));
        assert_eq!(known_suite("tool_selection"), Ok("tool_selection"));
        assert_eq!(known_suite("response_quality"), Ok("response_quality"));
    }

    /// `known_suite` returns an error with the available list for unknown suites.
    #[test]
    fn known_suite_lists_available_on_unknown() {
        let err = known_suite("nonexistent").unwrap_err();
        assert!(err.contains("Unknown suite: nonexistent"));
        assert!(err.contains("classify_accuracy"));
        assert!(err.contains("tool_selection"));
        assert!(err.contains("response_quality"));
    }

    /// `format_results` renders the suite name, score, and per-case marks.
    #[test]
    fn format_results_renders_pass_fail_marks() {
        let r = SuiteResult {
            suite: "test".into(),
            name: "Test Suite".into(),
            total: 2,
            passed: 1,
            failed: 1,
            score: "50.0%".into(),
            cases: vec![
                CaseResult { input: "input one".into(), passed: true, got: "ok".into(), expected: Some("ok".into()) },
                CaseResult { input: "input two".into(), passed: false, got: "wrong".into(), expected: Some("right".into()) },
            ],
        };
        let out = format_results(&r);
        assert!(out.contains("Test Suite"));
        assert!(out.contains("50.0%"));
        assert!(out.contains("✓"));
        assert!(out.contains("✗"));
        assert!(out.contains("exp: right"));
        assert!(out.contains("input one"));
    }

    /// `format_results` truncates long inputs (~50 char cap).
    #[test]
    fn format_results_truncates_long_inputs() {
        let long_in = "x".repeat(500);
        let r = SuiteResult {
            suite: "t".into(),
            name: "N".into(),
            total: 1,
            passed: 1,
            failed: 0,
            score: "100%".into(),
            cases: vec![CaseResult { input: long_in, passed: true, got: "ok".into(), expected: None }],
        };
        let out = format_results(&r);
        let x_run = out.matches('x').count();
        assert!(x_run <= 60, "must truncate long input; got {x_run} xs");
    }

    /// `suite_to_value` returns valid JSON matching the SuiteResult shape.
    #[test]
    fn suite_to_value_round_trips() {
        let r = SuiteResult {
            suite: "test".into(),
            name: "Test".into(),
            total: 3,
            passed: 2,
            failed: 1,
            score: "66.7%".into(),
            cases: vec![],
        };
        let v = suite_to_value(&r);
        assert_eq!(v["suite"], "test");
        assert_eq!(v["passed"], 2);
        assert_eq!(v["failed"], 1);
        assert_eq!(v["total"], 3);
        assert_eq!(v["score"], "66.7%");
    }

    /// `run_classify` returns deterministic counts based on the regex classifier.
    #[test]
    fn run_classify_returns_deterministic_results() {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        let cfg = Config {
            model: ModelConfig { provider: "x".into(), name: "y".into(), base_url: "http://localhost".into(), timeout: 60, api_key: None },
            context: ContextConfig { max_budget_pct: 70, detected_window: 32_768, working_memory_tokens: 8192, summary_threshold: 8000 },
            tools: ToolsConfig { bash_timeout: 30, tool_routing: "direct".into(), web_browse: false, shell_persist: true, shell_contain: false, rtk: true },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: Default::default(), models: None, limits: Default::default(),
            security: Default::default(), diff: Default::default(), filetree: Default::default(),
            snapshots: Default::default(), code_graph: Default::default(),
            tests: Default::default(), traces: Default::default(),
            dedup: Default::default(), evidence: Default::default(),
            plugins: Default::default(), diag: Default::default(),
            second_opinion: Default::default(),
        };
        let mut r = EvalRunner::new(&cfg);
        let result = r.run_classify();
        assert!(result.total > 0);
        assert_eq!(result.total, result.passed + result.failed,
            "total must equal passed + failed");
        assert!(result.score.contains('%'));
        // Results stored.
        assert_eq!(r.results.len(), 1);
    }

    /// `run_tool_selection` calls the chat_fn for each test case.
    #[tokio::test]
    async fn run_tool_selection_invokes_chat_fn() {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        let cfg = Config {
            model: ModelConfig { provider: "x".into(), name: "y".into(), base_url: "http://localhost".into(), timeout: 60, api_key: None },
            context: ContextConfig { max_budget_pct: 70, detected_window: 32_768, working_memory_tokens: 8192, summary_threshold: 8000 },
            tools: ToolsConfig { bash_timeout: 30, tool_routing: "direct".into(), web_browse: false, shell_persist: true, shell_contain: false, rtk: true },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: Default::default(), models: None, limits: Default::default(),
            security: Default::default(), diff: Default::default(), filetree: Default::default(),
            snapshots: Default::default(), code_graph: Default::default(),
            tests: Default::default(), traces: Default::default(),
            dedup: Default::default(), evidence: Default::default(),
            plugins: Default::default(), diag: Default::default(),
            second_opinion: Default::default(),
        };
        let mut r = EvalRunner::new(&cfg);
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = counter.clone();
        // Always return `bash` — all cases will either pass (when bash IS expected)
        // or fail. The point is the chat_fn must get invoked N times.
        let result = r.run_tool_selection(|_cfg, _msg| {
            let c = c2.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some("bash".to_string())
            }
        }).await;
        assert!(result.total > 0);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), result.total as u32,
            "chat_fn must be invoked once per test case");
    }

    /// `run_response_quality` evaluates length and contains checks.
    #[tokio::test]
    async fn run_response_quality_evaluates_checks() {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        let cfg = Config {
            model: ModelConfig { provider: "x".into(), name: "y".into(), base_url: "http://localhost".into(), timeout: 60, api_key: None },
            context: ContextConfig { max_budget_pct: 70, detected_window: 32_768, working_memory_tokens: 8192, summary_threshold: 8000 },
            tools: ToolsConfig { bash_timeout: 30, tool_routing: "direct".into(), web_browse: false, shell_persist: true, shell_contain: false, rtk: true },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: Default::default(), models: None, limits: Default::default(),
            security: Default::default(), diff: Default::default(), filetree: Default::default(),
            snapshots: Default::default(), code_graph: Default::default(),
            tests: Default::default(), traces: Default::default(),
            dedup: Default::default(), evidence: Default::default(),
            plugins: Default::default(), diag: Default::default(),
            second_opinion: Default::default(),
        };
        let mut r = EvalRunner::new(&cfg);
        // Return a long enough reply that contains "4" — should pass length>50
        // and "contains:4" checks; "contains:const|let" fails.
        let result = r.run_response_quality(|_cfg, _msg| async move {
            Some("4 is the answer. ".repeat(20))
        }).await;
        assert!(result.total > 0);
        // Some cases pass, some fail — but no panic on either check shape.
        assert_eq!(result.total, result.passed + result.failed);
    }

    /// EvalRunner::new starts with an empty results list.
    #[test]
    fn eval_runner_new_starts_empty() {
        use crate::config::{
            ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
        };
        let cfg = Config {
            model: ModelConfig { provider: "x".into(), name: "y".into(), base_url: "http://localhost".into(), timeout: 60, api_key: None },
            context: ContextConfig { max_budget_pct: 70, detected_window: 32_768, working_memory_tokens: 8192, summary_threshold: 8000 },
            tools: ToolsConfig { bash_timeout: 30, tool_routing: "direct".into(), web_browse: false, shell_persist: true, shell_contain: false, rtk: true },
            tui: TuiConfig { show_token_usage: false, auto_approve: false, theme: "dark".into(), classic: false },
            git: GitConfig { auto_commit: false },
            features: Default::default(), models: None, limits: Default::default(),
            security: Default::default(), diff: Default::default(), filetree: Default::default(),
            snapshots: Default::default(), code_graph: Default::default(),
            tests: Default::default(), traces: Default::default(),
            dedup: Default::default(), evidence: Default::default(),
            plugins: Default::default(), diag: Default::default(),
            second_opinion: Default::default(),
        };
        let r = EvalRunner::new(&cfg);
        assert!(r.results.is_empty());
    }
}
