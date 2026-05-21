//! Helper for detecting and invoking the
//! project's test runner.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRunner {
    NpmTest,
    Pytest,
    CargoTest,
    GoTest,
    None,
}

pub fn detect(cwd: &Path) -> TestRunner {
    if cwd.join("package.json").exists() {
        return TestRunner::NpmTest;
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("requirements.txt").exists() {
        return TestRunner::Pytest;
    }
    if cwd.join("Cargo.toml").exists() {
        return TestRunner::CargoTest;
    }
    if cwd.join("go.mod").exists() {
        return TestRunner::GoTest;
    }
    TestRunner::None
}

pub fn run(runner: TestRunner, cwd: &Path) -> Result<String, String> {
    let (cmd, args): (&str, Vec<&str>) = match runner {
        TestRunner::NpmTest => ("npm", vec!["test"]),
        TestRunner::Pytest => ("pytest", vec!["-x"]),
        TestRunner::CargoTest => ("cargo", vec!["test"]),
        TestRunner::GoTest => ("go", vec!["test", "./..."]),
        TestRunner::None => return Err("no test runner detected".into()),
    };
    let output = Command::new(cmd).args(args).current_dir(cwd).output().map_err(|e| e.to_string())?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(combined)
    } else {
        Err(combined)
    }
}
