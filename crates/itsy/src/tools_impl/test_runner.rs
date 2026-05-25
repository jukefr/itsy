//! Test-Runner Auto-Discovery.
//!
//! Detects the project's test runner from config files and provides a stable
//! command for running tests. Mirrors the upstream JS implementation:
//!
//!   Node.js:    package.json scripts.test, then vitest/jest/mocha/tap devdeps
//!   Python:     pyproject.toml [tool.pytest] / pytest.ini / manage.py / unittest
//!   Rust:       Cargo.toml → `cargo test`
//!   Go:         go.mod → `go test ./...`
//!   .NET:       *.sln / *.csproj → `dotnet test`
//!   Java:       Gradle / Maven
//!   Ruby:       rspec / rake
//!
//! Configuration:
//!   ITSY_TEST_RUNNER=<cmd>     override detected command
//!   ITSY_TEST_DISABLE=true     turn off entirely

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Backwards-compatible coarse enum kept for older call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRunner {
    NpmTest,
    Pytest,
    CargoTest,
    GoTest,
    Dotnet,
    Gradle,
    Maven,
    Rspec,
    Rake,
    NodeTest,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedRunner {
    pub command: String,
    pub framework: String,
    pub lang: String,
    pub confidence: f32,
}

/// Structured result of running a test suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub success: bool,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub duration_ms: u128,
    pub command: String,
    pub framework: String,
    pub output: String,
    /// True if the run was killed by the configured timeout.
    pub timed_out: bool,
}

/// Coarse classifier from the structured detection.
pub fn classify(d: &DetectedRunner) -> TestRunner {
    match d.framework.as_str() {
        "cargo-test" => TestRunner::CargoTest,
        "go-test" => TestRunner::GoTest,
        "pytest" | "hatch+pytest" | "unittest" | "django" => TestRunner::Pytest,
        "dotnet" => TestRunner::Dotnet,
        "gradle" => TestRunner::Gradle,
        "maven" => TestRunner::Maven,
        "rspec" => TestRunner::Rspec,
        "rake" => TestRunner::Rake,
        "node-test" => TestRunner::NodeTest,
        "" => TestRunner::None,
        _ => TestRunner::NpmTest, // jest, vitest, mocha, tap, ava, ...
    }
}

/// Backwards-compatible coarse detector.
pub fn detect(cwd: &Path) -> TestRunner {
    match detect_full(cwd) {
        Some(d) => classify(&d),
        None => TestRunner::None,
    }
}

/// Full structured detection. Returns `None` if nothing is found or detection
/// has been disabled via `[tests].disable = true`.
pub fn detect_full(cwd: &Path) -> Option<DetectedRunner> {
    let s = crate::settings::get();
    if s.test_disable {
        return None;
    }
    if let Ok(cmd) = std::env::var("ITSY_TEST_RUNNER") {
        if !cmd.is_empty() {
            return Some(DetectedRunner {
                command: cmd,
                framework: "custom".into(),
                lang: "custom".into(),
                confidence: 1.0,
            });
        }
    }
    if let Some(override_cmd) = s.test_runner.as_deref() {
        if !override_cmd.is_empty() {
            return Some(DetectedRunner {
                command: override_cmd.to_string(),
                framework: "custom".into(),
                lang: "custom".into(),
                confidence: 1.0,
            });
        }
    }
    scan(cwd)
}

fn exists(cwd: &Path, f: &str) -> bool {
    cwd.join(f).exists()
}

fn read(cwd: &Path, f: &str) -> String {
    std::fs::read_to_string(cwd.join(f)).unwrap_or_default()
}

fn scan(cwd: &Path) -> Option<DetectedRunner> {
    // ── Node.js ──────────────────────────────────────────────────────────
    if exists(cwd, "package.json") {
        if let Ok(pkg) = serde_json::from_str::<Value>(&read(cwd, "package.json")) {
            let script = pkg
                .get("scripts")
                .and_then(|s| s.get("test"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            if !script.is_empty() && script != "echo \"Error: no test specified\" && exit 1" {
                let cmd = node_test_cmd(script);
                return Some(DetectedRunner {
                    command: cmd,
                    framework: detect_node_framework(script).into(),
                    lang: "javascript".into(),
                    confidence: 0.95,
                });
            }
            let dev = pkg.get("devDependencies");
            let dep = pkg.get("dependencies");
            let has_dep = |k: &str| {
                dev.and_then(|v| v.get(k)).is_some() || dep.and_then(|v| v.get(k)).is_some()
            };
            if has_dep("vitest") {
                return Some(node("npx vitest run", "vitest", 0.8));
            }
            if has_dep("jest") {
                return Some(node("npx jest --passWithNoTests", "jest", 0.8));
            }
            if has_dep("mocha") {
                return Some(node("npx mocha", "mocha", 0.75));
            }
            if has_dep("tap") {
                return Some(node("npx tap", "tap", 0.7));
            }
            if has_dep("ava") {
                return Some(node("npx ava", "ava", 0.7));
            }
            if has_dep("jasmine") {
                return Some(node("npx jasmine", "jasmine", 0.7));
            }
            if exists(cwd, "test") || exists(cwd, "tests") || exists(cwd, "__tests__") {
                return Some(node("node --test", "node-test", 0.5));
            }
        }
    }

    // ── Python ───────────────────────────────────────────────────────────
    let has_pyproject = exists(cwd, "pyproject.toml");
    let has_pytest_ini = exists(cwd, "pytest.ini") || exists(cwd, "setup.cfg");
    let has_py_tests = exists(cwd, "tests") || exists(cwd, "test") || dir_has_test_py(cwd);
    let any_py_evidence = has_pyproject
        || has_pytest_ini
        || dir_has_py(cwd)
        || exists(cwd, "manage.py")
        || exists(cwd, "requirements.txt")
        || exists(cwd, "setup.py");
    if any_py_evidence {
        if has_pyproject {
            let ppc = read(cwd, "pyproject.toml");
            if ppc.contains("[tool.pytest") || ppc.contains("[tool.pytest.ini_options]") {
                return Some(py("python -m pytest", "pytest", 0.95));
            }
            if ppc.contains("[tool.hatch") && ppc.contains("pytest") {
                return Some(py("hatch test", "hatch+pytest", 0.85));
            }
        }
        if has_pytest_ini {
            return Some(py("python -m pytest", "pytest", 0.9));
        }
        if has_py_tests {
            return Some(py("python -m pytest", "pytest", 0.7));
        }
        if exists(cwd, "manage.py") {
            return Some(py("python manage.py test", "django", 0.8));
        }
        if exists(cwd, "tests") || exists(cwd, "test") {
            return Some(py("python -m unittest discover", "unittest", 0.5));
        }
    }

    // ── Rust ─────────────────────────────────────────────────────────────
    if exists(cwd, "Cargo.toml") {
        return Some(DetectedRunner {
            command: "cargo test".into(),
            framework: "cargo-test".into(),
            lang: "rust".into(),
            confidence: 0.95,
        });
    }

    // ── Go ───────────────────────────────────────────────────────────────
    if exists(cwd, "go.mod") {
        return Some(DetectedRunner {
            command: "go test ./...".into(),
            framework: "go-test".into(),
            lang: "go".into(),
            confidence: 0.95,
        });
    }

    // ── .NET / C# ────────────────────────────────────────────────────────
    if shallow_glob(cwd, |s| s.ends_with(".sln")) {
        return Some(DetectedRunner {
            command: "dotnet test".into(),
            framework: "dotnet".into(),
            lang: "csharp".into(),
            confidence: 0.9,
        });
    }
    if shallow_glob(cwd, |s| s.ends_with(".csproj")) {
        return Some(DetectedRunner {
            command: "dotnet test".into(),
            framework: "dotnet".into(),
            lang: "csharp".into(),
            confidence: 0.85,
        });
    }

    // ── Java/Gradle ──────────────────────────────────────────────────────
    if exists(cwd, "build.gradle") || exists(cwd, "build.gradle.kts") {
        let gradlew = if exists(cwd, "gradlew") { "./gradlew" } else { "gradle" };
        return Some(DetectedRunner {
            command: format!("{gradlew} test"),
            framework: "gradle".into(),
            lang: "java".into(),
            confidence: 0.9,
        });
    }

    // ── Java/Maven ───────────────────────────────────────────────────────
    if exists(cwd, "pom.xml") {
        return Some(DetectedRunner {
            command: "mvn test -q".into(),
            framework: "maven".into(),
            lang: "java".into(),
            confidence: 0.9,
        });
    }

    // ── Ruby ─────────────────────────────────────────────────────────────
    if exists(cwd, ".rspec") || exists(cwd, "spec") {
        let cmd = if exists(cwd, "Gemfile.lock") { "bundle exec rspec" } else { "rspec" };
        return Some(DetectedRunner {
            command: cmd.into(),
            framework: "rspec".into(),
            lang: "ruby".into(),
            confidence: 0.85,
        });
    }
    if exists(cwd, "Rakefile") {
        return Some(DetectedRunner {
            command: "rake test".into(),
            framework: "rake".into(),
            lang: "ruby".into(),
            confidence: 0.7,
        });
    }

    // ── PHP / PHPUnit ────────────────────────────────────────────────────
    if exists(cwd, "phpunit.xml") || exists(cwd, "phpunit.xml.dist") {
        let cmd = if exists(cwd, "vendor/bin/phpunit") {
            "./vendor/bin/phpunit"
        } else {
            "phpunit"
        };
        return Some(DetectedRunner {
            command: cmd.into(),
            framework: "phpunit".into(),
            lang: "php".into(),
            confidence: 0.85,
        });
    }
    if exists(cwd, "composer.json") {
        return Some(DetectedRunner {
            command: "composer test".into(),
            framework: "composer".into(),
            lang: "php".into(),
            confidence: 0.5,
        });
    }

    None
}

fn node(cmd: &str, fw: &str, conf: f32) -> DetectedRunner {
    DetectedRunner {
        command: cmd.into(),
        framework: fw.into(),
        lang: "javascript".into(),
        confidence: conf,
    }
}

fn py(cmd: &str, fw: &str, conf: f32) -> DetectedRunner {
    DetectedRunner {
        command: cmd.into(),
        framework: fw.into(),
        lang: "python".into(),
        confidence: conf,
    }
}

fn detect_node_framework(script: &str) -> &'static str {
    if script.contains("vitest") { "vitest" }
    else if script.contains("jest") { "jest" }
    else if script.contains("mocha") { "mocha" }
    else if script.contains("tap") { "tap" }
    else if script.contains("ava") { "ava" }
    else if script.contains("jasmine") { "jasmine" }
    else if script.contains("playwright") { "playwright" }
    else if script.contains("cypress") { "cypress" }
    else if script.contains("pytest") { "pytest" }
    else if script.contains("node --test") { "node-test" }
    else { "npm-test" }
}

/// Adjust a node test script so it runs once (no watch mode).
fn node_test_cmd(script: &str) -> String {
    if script.contains("vitest") && !script.contains("--run") {
        return format!("{script} --run");
    }
    if script.contains("jest") && (script.contains("--watch") || script.contains("--watchAll")) {
        return script.replace("--watch", "").replace("--watchAll", "").trim().to_string();
    }
    if script.starts_with("npm ") || script.starts_with("node ") || script.starts_with("npx ") {
        script.to_string()
    } else {
        "npm test".to_string()
    }
}

fn shallow_glob<F: Fn(&str) -> bool>(dir: &Path, pred: F) -> bool {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().any(|e| pred(&e.file_name().to_string_lossy())),
        Err(_) => false,
    }
}

fn dir_has_test_py(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("test_") && n.ends_with(".py")
        }),
        Err(_) => false,
    }
}

fn dir_has_py(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".py")),
        Err(_) => false,
    }
}

/// Format a brief one-liner for the system prompt.
pub fn format_for_prompt(cwd: &Path) -> String {
    match detect_full(cwd) {
        Some(d) => format!(
            "\n\nTest runner ({}): `{}`  — run this after edits to verify changes.",
            d.framework, d.command
        ),
        None => String::new(),
    }
}

// ─── Execution ─────────────────────────────────────────────────────────────

/// Backwards-compatible coarse runner. Kept for older call sites.
pub fn run(runner: TestRunner, cwd: &Path) -> Result<String, String> {
    let (cmd, args): (&str, Vec<&str>) = match runner {
        TestRunner::NpmTest => ("npm", vec!["test"]),
        TestRunner::Pytest => ("pytest", vec!["-x"]),
        TestRunner::CargoTest => ("cargo", vec!["test"]),
        TestRunner::GoTest => ("go", vec!["test", "./..."]),
        TestRunner::Dotnet => ("dotnet", vec!["test"]),
        TestRunner::Gradle => ("gradle", vec!["test"]),
        TestRunner::Maven => ("mvn", vec!["test", "-q"]),
        TestRunner::Rspec => ("rspec", vec![]),
        TestRunner::Rake => ("rake", vec!["test"]),
        TestRunner::NodeTest => ("node", vec!["--test"]),
        TestRunner::None => return Err("no test runner detected".into()),
    };
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() { Ok(combined) } else { Err(combined) }
}

/// Run the detected test command and return a structured `TestResult`.
///
/// `pattern` is an optional test-name filter that is appended in a framework-
/// appropriate flag form (best-effort — falls back to a raw positional arg).
/// `timeout` caps wall time; if exceeded, the child is killed and
/// `timed_out=true` is set on the result.
pub fn run_detected(
    cwd: &Path,
    pattern: Option<&str>,
    timeout: Option<Duration>,
) -> Result<TestResult, String> {
    let detected = detect_full(cwd).ok_or_else(|| "no test runner detected".to_string())?;
    let cmd_str = apply_pattern(&detected, pattern);

    let mut child = build_command(&cmd_str, cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let start = Instant::now();
    let mut timed_out = false;
    let status = if let Some(limit) = timeout {
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(s) => break s,
                None => {
                    if start.elapsed() >= limit {
                        let _ = child.kill();
                        timed_out = true;
                        break child.wait().map_err(|e| e.to_string())?;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    } else {
        child.wait().map_err(|e| e.to_string())?
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read;
        let _ = o.read_to_string(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut stderr);
    }
    let output = format!("{stdout}{stderr}");

    let (passed, failed, skipped) = parse_counts(&detected.framework, &output);
    let success = status.success() && !timed_out;

    Ok(TestResult {
        success,
        passed,
        failed,
        skipped,
        duration_ms: start.elapsed().as_millis(),
        command: cmd_str,
        framework: detected.framework,
        output,
        timed_out,
    })
}

fn build_command(cmd_str: &str, cwd: &Path) -> Command {
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    let (program, args) = parts.split_first().map(|(p, r)| (*p, r.to_vec())).unwrap_or(("sh", vec![]));
    let mut c = Command::new(program);
    c.args(args).current_dir(cwd);
    c
}

fn apply_pattern(d: &DetectedRunner, pattern: Option<&str>) -> String {
    let Some(p) = pattern.filter(|p| !p.is_empty()) else {
        return d.command.clone();
    };
    match d.framework.as_str() {
        "pytest" | "hatch+pytest" => format!("{} -k {}", d.command, shell_escape(p)),
        "cargo-test" => format!("{} {}", d.command, shell_escape(p)),
        "go-test" => format!("{} -run {}", d.command, shell_escape(p)),
        "jest" => format!("{} -t {}", d.command, shell_escape(p)),
        "vitest" => format!("{} -t {}", d.command, shell_escape(p)),
        "mocha" => format!("{} --grep {}", d.command, shell_escape(p)),
        "rspec" => format!("{} -e {}", d.command, shell_escape(p)),
        "dotnet" => format!("{} --filter {}", d.command, shell_escape(p)),
        _ => format!("{} {}", d.command, shell_escape(p)),
    }
}

fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '-' || c == '.') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Parse pass/fail/skip counts from typical framework output.
fn parse_counts(framework: &str, output: &str) -> (u32, u32, u32) {
    use regex::Regex;
    let lc = framework.to_lowercase();

    // Cargo: "test result: ok. 12 passed; 0 failed; 1 ignored"
    if lc == "cargo-test" {
        let re = Regex::new(r"(?m)test result:[^\n]*?(\d+)\s+passed;\s*(\d+)\s+failed;\s*(\d+)\s+ignored").expect("valid regex literal");
        let (mut p, mut f, mut s) = (0u32, 0u32, 0u32);
        for cap in re.captures_iter(output) {
            p += cap[1].parse::<u32>().unwrap_or(0);
            f += cap[2].parse::<u32>().unwrap_or(0);
            s += cap[3].parse::<u32>().unwrap_or(0);
        }
        return (p, f, s);
    }
    // Pytest: "5 passed, 1 failed, 2 skipped in 3.21s"
    if lc.contains("pytest") || lc == "django" || lc == "unittest" {
        let p = single(r"(\d+)\s+passed", output);
        let f = single(r"(\d+)\s+failed", output) + single(r"(\d+)\s+errors?", output);
        let s = single(r"(\d+)\s+skipped", output);
        return (p, f, s);
    }
    // Jest/Vitest: "Tests:       1 failed, 2 passed, 3 total" etc.
    if matches!(lc.as_str(), "jest" | "vitest") {
        let p = single(r"(\d+)\s+passed", output);
        let f = single(r"(\d+)\s+failed", output);
        let s = single(r"(\d+)\s+(?:skipped|todo)", output);
        return (p, f, s);
    }
    // Mocha: "  10 passing" / "  1 failing" / "  2 pending"
    if lc == "mocha" {
        let p = single(r"(\d+)\s+passing", output);
        let f = single(r"(\d+)\s+failing", output);
        let s = single(r"(\d+)\s+pending", output);
        return (p, f, s);
    }
    // Go: "--- PASS" / "--- FAIL" / "--- SKIP" lines
    if lc == "go-test" {
        let p = count_matches(r"(?m)^--- PASS", output);
        let f = count_matches(r"(?m)^--- FAIL", output);
        let s = count_matches(r"(?m)^--- SKIP", output);
        return (p, f, s);
    }
    // RSpec: "5 examples, 1 failure, 2 pending"
    if lc == "rspec" {
        let total = single(r"(\d+)\s+examples?", output);
        let f = single(r"(\d+)\s+failures?", output);
        let s = single(r"(\d+)\s+pending", output);
        let p = total.saturating_sub(f + s);
        return (p, f, s);
    }
    // PHPUnit / dotnet / generic
    let p = single(r"(\d+)\s+passed", output);
    let f = single(r"(\d+)\s+failed", output);
    let s = single(r"(\d+)\s+skipped", output);
    (p, f, s)
}

fn single(pat: &str, text: &str) -> u32 {
    regex::Regex::new(pat)
        .ok()
        .and_then(|re| re.captures(text))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or(0)
}

fn count_matches(pat: &str, text: &str) -> u32 {
    regex::Regex::new(pat)
        .map(|re| re.find_iter(text).count() as u32)
        .unwrap_or(0)
}

// Silence "unused" if no callers use these helpers yet.
#[allow(dead_code)]
fn _keep_pathbuf() -> Option<PathBuf> { None }

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise env-mutating tests so the parallel test runner can't catch
    /// `override_env` mid-flight from a sibling test.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn detects_cargo() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let d = detect_full(tmp.path()).unwrap();
        assert_eq!(d.framework, "cargo-test");
        assert_eq!(classify(&d), TestRunner::CargoTest);
    }

    #[test]
    fn detects_go() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x").unwrap();
        let d = detect_full(tmp.path()).unwrap();
        assert_eq!(d.framework, "go-test");
    }

    #[test]
    fn override_env() {
        // SAFETY: env var mutation is unsafe re: parallel test threads; the
        // ENV_LOCK above serialises every test that reads or writes
        // `ITSY_TEST_RUNNER`.
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("ITSY_TEST_RUNNER", "my-cmd --foo"); }
        let tmp = tempfile::tempdir().unwrap();
        let d = detect_full(tmp.path()).unwrap();
        assert_eq!(d.command, "my-cmd --foo");
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
    }

    #[test]
    fn parse_pytest_output() {
        let out = "===== 5 passed, 1 failed, 2 skipped in 3.21s =====";
        assert_eq!(parse_counts("pytest", out), (5, 1, 2));
    }

    #[test]
    fn parse_cargo_output() {
        let out = "test result: ok. 12 passed; 0 failed; 1 ignored; 0 measured";
        assert_eq!(parse_counts("cargo-test", out), (12, 0, 1));
    }

    /// Cargo: multiple `test result:` lines (one per crate in a workspace)
    /// are summed correctly.
    #[test]
    fn parse_cargo_workspace_sums_multiple_result_lines() {
        let out = "running 5 tests\ntest result: ok. 5 passed; 0 failed; 0 ignored\n\
                   running 7 tests\ntest result: ok. 6 passed; 1 failed; 2 ignored";
        assert_eq!(parse_counts("cargo-test", out), (11, 1, 2));
    }

    /// Jest counts respect "passed" / "failed" / "skipped" or "todo".
    #[test]
    fn parse_jest_output() {
        let out = "Tests: 1 failed, 2 passed, 1 todo, 4 total";
        assert_eq!(parse_counts("jest", out), (2, 1, 1));
    }

    /// Vitest output uses the same shape as Jest.
    #[test]
    fn parse_vitest_output() {
        let out = "Tests:       8 passed, 2 failed, 1 skipped (11 total)";
        assert_eq!(parse_counts("vitest", out), (8, 2, 1));
    }

    /// Mocha uses passing/failing/pending wording.
    #[test]
    fn parse_mocha_output() {
        let out = "  10 passing (1s)\n  2 failing\n  3 pending";
        assert_eq!(parse_counts("mocha", out), (10, 2, 3));
    }

    /// Go test output: count `--- PASS/FAIL/SKIP` lines per subtest.
    #[test]
    fn parse_go_output() {
        let out = "--- PASS: TestA\n--- PASS: TestB\n--- FAIL: TestC\n--- SKIP: TestD\n--- PASS: TestE\n";
        assert_eq!(parse_counts("go-test", out), (3, 1, 1));
    }

    /// RSpec: total - failures - pending = passed.
    #[test]
    fn parse_rspec_output() {
        let out = "Finished in 0.1 seconds\n10 examples, 2 failures, 3 pending";
        // 10 total - 2 fail - 3 pending = 5 passed
        assert_eq!(parse_counts("rspec", out), (5, 2, 3));
    }

    /// Pytest: "errors" count is folded into the failure tally.
    /// Anti-regression: a collection error must not be silently treated as success.
    #[test]
    fn parse_pytest_collection_errors_count_as_failures() {
        let out = "===== 3 passed, 1 errors in 2.0s =====";
        let (p, f, _s) = parse_counts("pytest", out);
        assert_eq!(p, 3);
        assert_eq!(f, 1, "pytest errors must be counted as failures, not ignored");
    }

    /// Unknown framework falls through to the generic passed/failed/skipped parse.
    #[test]
    fn parse_unknown_framework_uses_generic() {
        let out = "5 passed 1 failed 0 skipped";
        let counts = parse_counts("phpunit", out);
        assert!(counts.0 == 5 && counts.1 == 1);
    }

    /// Empty output → all zeros (don't panic).
    #[test]
    fn parse_empty_output_is_zeros() {
        assert_eq!(parse_counts("pytest", ""), (0, 0, 0));
        assert_eq!(parse_counts("cargo-test", ""), (0, 0, 0));
    }

    // ── Framework detection ────────────────────────────────────────────────

    /// pytest detected via `pyproject.toml [tool.pytest.ini_options]`.
    #[test]
    fn detects_pytest_via_pyproject() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\ntestpaths=[\"tests\"]").unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        std::fs::write(tmp.path().join("tests/test_x.py"), "def test_ok(): pass").unwrap();
        let d = detect_full(tmp.path()).expect("pyproject pytest should be detected");
        assert!(d.framework.contains("pytest"),
            "framework should mention pytest; got {}", d.framework);
    }

    /// pytest detected via standalone pytest.ini.
    #[test]
    fn detects_pytest_via_ini() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pytest.ini"), "[pytest]\n").unwrap();
        let d = detect_full(tmp.path()).expect("pytest.ini should be detected");
        assert!(d.framework.contains("pytest"));
    }

    /// Node package.json `test` script picks the right framework
    /// (jest/vitest/mocha) from the script line.
    #[test]
    fn detects_node_jest_from_package_json() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"),
            "{\"scripts\":{\"test\":\"jest\"}}").unwrap();
        let d = detect_full(tmp.path()).expect("node project should be detected");
        assert_eq!(d.framework, "jest");
    }

    #[test]
    fn detects_node_vitest_from_package_json() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"),
            "{\"scripts\":{\"test\":\"vitest run\"}}").unwrap();
        let d = detect_full(tmp.path()).expect("node project should be detected");
        assert_eq!(d.framework, "vitest");
    }

    #[test]
    fn detects_node_mocha_from_package_json() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"),
            "{\"scripts\":{\"test\":\"mocha test/**/*.js\"}}").unwrap();
        let d = detect_full(tmp.path()).expect("node project should be detected");
        assert_eq!(d.framework, "mocha");
    }

    /// Empty directory → no runner detected.
    #[test]
    fn detects_nothing_in_empty_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect_full(tmp.path()).is_none(),
            "empty dir must not produce a fake runner");
    }

    /// `format_for_prompt` produces a non-empty marker when a runner is detected.
    #[test]
    fn format_for_prompt_mentions_detected_runner() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("ITSY_TEST_RUNNER"); }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let out = format_for_prompt(tmp.path());
        assert!(!out.is_empty(), "format_for_prompt must return content");
        assert!(out.contains("cargo"), "must mention cargo; got: {out}");
    }
}
