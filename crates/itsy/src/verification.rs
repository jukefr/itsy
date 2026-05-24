//! Project-native verification target discovery.
//!
//! The agent should prefer the repository's own tests / verifier scripts over
//! ad-hoc spot checks when certifying success. This module discovers a small,
//! high-confidence set of verification assets and commands from the working
//! tree plus common external verifier mounts.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationTargets {
    assets: Vec<String>,
    commands: Vec<VerificationCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerificationCommand {
    display: String,
    prefixes: Vec<String>,
    external: bool,
}

impl VerificationTargets {
    pub fn has_authoritative_commands(&self) -> bool {
        !self.commands.is_empty()
    }

    pub fn has_external_authoritative_commands(&self) -> bool {
        self.commands.iter().any(|c| c.external)
    }

    pub fn recommended_commands(&self) -> Vec<String> {
        self.commands.iter().map(|c| c.display.clone()).collect()
    }

    pub fn matches_command(&self, command: &str) -> bool {
        matches_any_command(self.commands.iter(), command)
    }

    pub fn matches_external_command(&self, command: &str) -> bool {
        matches_any_command(self.commands.iter().filter(|c| c.external), command)
    }

    pub fn prompt_block(&self) -> Option<String> {
        if self.assets.is_empty() && self.commands.is_empty() {
            return None;
        }
        let mut parts = Vec::with_capacity(3);
        if !self.assets.is_empty() {
            parts.push(format!(
                "verification assets: {}",
                self.assets.join(", ")
            ));
        }
        if !self.commands.is_empty() {
            parts.push(format!(
                "preferred commands: {}",
                self.recommended_commands().join(" | ")
            ));
        }
        Some(format!(
            "Project-native verification detected — {}. Prefer these over ad-hoc spot checks before marking `passed` or closing the contract.",
            parts.join("; ")
        ))
    }

    pub fn close_gate_message(&self) -> Option<String> {
        if !self.has_external_authoritative_commands() {
            return None;
        }
        let commands: Vec<String> = self
            .commands
            .iter()
            .filter(|c| c.external)
            .map(|c| c.display.clone())
            .collect();
        Some(format!(
            "External verifier commands are available: {}. Run one of them on the final state and capture its exact output in `mark_assertion` before closing the contract.",
            commands.join(" | ")
        ))
    }
}

pub fn discover(cwd: &Path) -> VerificationTargets {
    let mut out = Collector::default();
    collect_root(cwd, false, &mut out);

    // Some harnesses mount authoritative verifier assets outside the writable
    // project tree. `/tests` is the most common convention; include it when
    // present so the agent can align to the real oracle instead of a local copy.
    let external_tests = Path::new("/tests");
    if external_tests.is_dir() && normalize(external_tests) != normalize(cwd) {
        collect_root(external_tests, true, &mut out);
    }

    out.finish()
}

#[derive(Default)]
struct Collector {
    assets: Vec<String>,
    commands: Vec<VerificationCommand>,
}

impl Collector {
    fn add_asset(&mut self, asset: String) {
        if !self.assets.iter().any(|existing| existing == &asset) {
            self.assets.push(asset);
        }
    }

    fn add_command<S: Into<String>>(&mut self, display: S, prefixes: Vec<String>, external: bool) {
        let display = display.into();
        if self.commands.iter().any(|existing| existing.display == display) {
            return;
        }
        self.commands.push(VerificationCommand {
            display,
            prefixes,
            external,
        });
    }

    fn finish(mut self) -> VerificationTargets {
        self.assets.sort();
        self.commands.sort_by(|a, b| a.display.cmp(&b.display));
        VerificationTargets {
            assets: self.assets,
            commands: self.commands,
        }
    }
}

fn collect_root(root: &Path, external: bool, out: &mut Collector) {
    let root = normalize(root);

    if root.join("Cargo.toml").is_file() {
        out.add_asset(display_path(&root.join("Cargo.toml")));
        out.add_command("cargo test", vec![normalize_command("cargo test")], external);
    }

    if root.join("go.mod").is_file() {
        out.add_asset(display_path(&root.join("go.mod")));
        out.add_command("go test ./...", vec![normalize_command("go test ./...")], external);
    }

    if let Some(package_json) = read_json(&root.join("package.json")) {
        let test_script = package_json
            .get("scripts")
            .and_then(|v| v.as_object())
            .and_then(|scripts| scripts.get("test"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if test_script.is_some() {
            out.add_asset(display_path(&root.join("package.json")));
            let pm = detect_package_manager(&root);
            let prefixes = vec![normalize_command(&format!("{pm} test"))];
            out.add_command(format!("{pm} test"), prefixes, external);
        }
    }

    if let Some(makefile) = first_existing(&[
        root.join("Makefile"),
        root.join("makefile"),
        root.join("GNUmakefile"),
    ]) {
        if file_contains_target(&makefile, "test") {
            out.add_asset(display_path(&makefile));
            out.add_command("make test", vec![normalize_command("make test")], external);
        } else if file_contains_target(&makefile, "check") {
            out.add_asset(display_path(&makefile));
            out.add_command("make check", vec![normalize_command("make check")], external);
        }
    }

    if let Some(justfile) = first_existing(&[root.join("justfile"), root.join("Justfile")]) {
        if file_contains_recipe(&justfile, "test") {
            out.add_asset(display_path(&justfile));
            out.add_command("just test", vec![normalize_command("just test")], external);
        }
    }

    let tests_dir = root.join("tests");
    let has_pytest = root.join("pytest.ini").is_file()
        || root.join("tox.ini").is_file()
        || pyproject_mentions_pytest(&root.join("pyproject.toml"))
        || directory_contains_py_tests(&tests_dir)
        || directory_contains_py_tests(&root);
    if has_pytest {
        if tests_dir.is_dir() {
            out.add_asset(display_path(&tests_dir));
        }
        if root.join("pytest.ini").is_file() {
            out.add_asset(display_path(&root.join("pytest.ini")));
        }
        out.add_command(
            "pytest -q",
            vec![
                normalize_command("pytest -q"),
                normalize_command("pytest"),
                normalize_command("python -m pytest"),
                normalize_command("python3 -m pytest"),
                normalize_command("uv run pytest"),
            ],
            external,
        );
    }

    for rel in ["tests/test_outputs.py", "test_outputs.py"] {
        let file = root.join(rel);
        if file.is_file() {
            let shown = display_path(&file);
            out.add_asset(shown.clone());
            out.add_command(
                format!("python3 {shown}"),
                vec![
                    normalize_command(&format!("python {shown}")),
                    normalize_command(&format!("python3 {shown}")),
                    normalize_command(&format!("pytest {shown}")),
                    normalize_command(&format!("pytest -q {shown}")),
                ],
                external,
            );
        }
    }

    for rel in ["tests/filter.py", "filter.py"] {
        let file = root.join(rel);
        if file.is_file() {
            out.add_asset(display_path(&file));
        }
    }
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.is_file()).cloned()
}

fn detect_package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        "bun"
    } else {
        "npm"
    }
}

fn directory_contains_py_tests(dir: &Path) -> bool {
    directory_contains_py_tests_inner(dir, 0)
}

fn directory_contains_py_tests_inner(dir: &Path, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return directory_contains_py_tests_inner(&path, depth + 1);
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        name.ends_with("_test.py") || (name.starts_with("test") && name.ends_with(".py"))
    })
}

fn file_contains_target(path: &Path, target: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| line.trim_start().starts_with(&format!("{target}:")))
}

fn file_contains_recipe(path: &Path, recipe: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| line.trim_start().starts_with(&format!("{recipe}:")))
}

fn pyproject_mentions_pytest(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.contains("[tool.pytest") || text.contains("pytest")
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn matches_any_command<'a>(
    mut commands: impl Iterator<Item = &'a VerificationCommand>,
    command: &str,
) -> bool {
    let norm = normalize_command(command);
    commands.any(|candidate| {
        candidate.prefixes.iter().any(|prefix| {
            norm == *prefix
                || norm
                    .strip_prefix(prefix)
                    .map(|rest| rest.starts_with(' '))
                    .unwrap_or(false)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir() -> PathBuf {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("itsy-verification-{uniq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_cargo_and_pytest_targets() {
        let dir = tmp_dir();
        fs::write(dir.join("Cargo.toml"), "[package]\nname='x'\nversion='0.1.0'\n").unwrap();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/test_outputs.py"), "print('ok')\n").unwrap();

        let targets = discover(&dir);
        assert!(targets.has_authoritative_commands());
        let commands = targets.recommended_commands();
        assert!(commands.iter().any(|c| c == "cargo test"));
        assert!(commands.iter().any(|c| c == &format!("python3 {}/tests/test_outputs.py", dir.display())));
        assert!(targets.matches_command("cargo test -- --nocapture"));
        assert!(targets.matches_command(&format!("python3 {}/tests/test_outputs.py", dir.display())));
    }

    #[test]
    fn detects_package_manager_test_command() {
        let dir = tmp_dir();
        fs::write(dir.join("package.json"), "{\"scripts\":{\"test\":\"vitest\"}}\n").unwrap();
        fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

        let targets = discover(&dir);
        assert!(targets.recommended_commands().iter().any(|c| c == "pnpm test"));
        assert!(targets.matches_command("pnpm test -- --runInBand"));
        assert!(!targets.matches_command("vitest run"));
    }

    #[test]
    fn detects_nested_pytest_layout() {
        let dir = tmp_dir();
        fs::create_dir_all(dir.join("tests/unit")).unwrap();
        fs::write(dir.join("tests/unit/test_api.py"), "def test_ok():\n    assert True\n").unwrap();

        let targets = discover(&dir);
        assert!(targets.recommended_commands().iter().any(|c| c == "pytest -q"));
    }

    #[test]
    fn prompt_block_mentions_assets_and_commands() {
        let dir = tmp_dir();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/test_outputs.py"), "print('ok')\n").unwrap();

        let block = discover(&dir).prompt_block().unwrap();
        assert!(block.contains("verification assets"));
        assert!(block.contains("preferred commands"));
    }
}
