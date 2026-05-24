//! Bootstrap detection.
//!
//! On first turn, scan the workspace for key config files and produce a
//! 1-2 line project summary suitable for injection into the system
//! prompt. Without this, small models waste 3-5 tool calls just to
//! establish "this is a Node 18 project with npm test, entry at
//! src/index.js".
//!
//! Output is a compact one-liner like:
//!   `Node 20 (npm) — Next.js — entry: src/app.js — build: \`npm run build\`, test: \`npm test\``
//!
//! Configuration:
//!   * `ITSY_BOOTSTRAP=false`   — disable entirely
//!   * `ITSY_BOOTSTRAP_MAX=200` — max chars of the summary injected

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

const DEFAULT_MAX: usize = 200;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootstrapParts {
    pub runtime: Option<String>,
    pub pm: Option<String>,
    pub framework: Option<String>,
    pub entry: Option<String>,
    pub build: Option<String>,
    pub test: Option<String>,
    pub start: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResult {
    pub summary: String,
    pub parts: BootstrapParts,
}

#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    pub workdir: PathBuf,
    pub disable: bool,
    pub max_chars: usize,
}

impl BootstrapOptions {
    pub fn for_dir(workdir: PathBuf) -> Self {
        let s = crate::settings::get();
        let max_chars = if s.bootstrap_max_chars > 0 {
            s.bootstrap_max_chars
        } else {
            DEFAULT_MAX
        };
        Self { workdir, disable: !s.bootstrap, max_chars }
    }
}

pub struct BootstrapDetector {
    opts: BootstrapOptions,
    cache: Mutex<Option<Option<BootstrapResult>>>,
}

impl BootstrapDetector {
    pub fn new(opts: BootstrapOptions) -> Self {
        Self { opts, cache: Mutex::new(None) }
    }

    /// Run detection. Cached. Returns `None` when disabled or when no
    /// known runtime/marker is present.
    pub fn detect(&self) -> Option<BootstrapResult> {
        if self.opts.disable {
            return None;
        }
        {
            let cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.as_ref() {
                return cached.clone();
            }
        }
        let result = scan(&self.opts.workdir);
        *self.cache.lock().unwrap() = Some(result.clone());
        result
    }

    /// Format for system prompt injection. Returns `""` if nothing found.
    pub fn format_for_prompt(&self) -> String {
        let Some(r) = self.detect() else { return String::new() };
        let s = if r.summary.chars().count() > self.opts.max_chars {
            let mut out: String =
                r.summary.chars().take(self.opts.max_chars.saturating_sub(1)).collect();
            out.push('…');
            out
        } else {
            r.summary
        };
        format!("\n\nProject: {s}")
    }

    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }
}

/// Convenience entry-point used by `Session::start`. Generates the
/// "Project:" prefix for the first turn. Empty string on no-detection.
pub fn bootstrap_context(cwd: &Path) -> String {
    BootstrapDetector::new(BootstrapOptions::for_dir(cwd.to_path_buf())).format_for_prompt()
}

// ── scanning ────────────────────────────────────────────────────────────────

fn exists(cwd: &Path, f: &str) -> bool {
    cwd.join(f).exists()
}
fn read(cwd: &Path, f: &str) -> String {
    fs::read_to_string(cwd.join(f)).unwrap_or_default()
}
fn read_json(cwd: &Path, f: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&read(cwd, f)).ok()
}

fn scan(cwd: &Path) -> Option<BootstrapResult> {
    let mut parts = BootstrapParts::default();

    if exists(cwd, "package.json") {
        scan_node(cwd, &mut parts);
    } else if exists(cwd, "pyproject.toml")
        || exists(cwd, "setup.py")
        || exists(cwd, "setup.cfg")
        || exists(cwd, "requirements.txt")
    {
        scan_python(cwd, &mut parts);
    } else if exists(cwd, "Cargo.toml") {
        scan_rust(cwd, &mut parts);
    } else if exists(cwd, "go.mod") {
        scan_go(cwd, &mut parts);
    } else if exists(cwd, "global.json")
        || !glob_ext(cwd, "sln").is_empty()
        || !glob_ext(cwd, "csproj").is_empty()
    {
        parts.runtime = Some(".NET".into());
        parts.build = Some("dotnet build".into());
        parts.test = Some("dotnet test".into());
    } else if exists(cwd, "pom.xml") {
        parts.runtime = Some("Java (Maven)".into());
        parts.build = Some("mvn package -q".into());
        parts.test = Some("mvn test -q".into());
    } else if exists(cwd, "build.gradle") || exists(cwd, "build.gradle.kts") {
        parts.runtime = Some("Java (Gradle)".into());
        let gradlew = if exists(cwd, "gradlew") { "./gradlew" } else { "gradle" };
        parts.build = Some(format!("{gradlew} build"));
        parts.test = Some(format!("{gradlew} test"));
    } else if exists(cwd, "Gemfile") {
        scan_ruby(cwd, &mut parts);
    }

    parts.runtime.as_ref()?;
    let summary = compose(&parts);
    Some(BootstrapResult { summary, parts })
}

fn scan_node(cwd: &Path, parts: &mut BootstrapParts) {
    let Some(pkg) = read_json(cwd, "package.json") else { return };

    let nvmrc = read(cwd, ".nvmrc");
    let node_version_file = read(cwd, ".node-version");
    let tool_versions = read(cwd, ".tool-versions");

    let from_file = trim_nonempty(&nvmrc)
        .or_else(|| trim_nonempty(&node_version_file))
        .or_else(|| parse_tool_versions(&tool_versions, "nodejs"))
        .or_else(|| parse_tool_versions(&tool_versions, "node"));

    let from_engines = pkg
        .get("engines")
        .and_then(|e| e.get("node"))
        .and_then(|v| v.as_str())
        .map(strip_version);

    let node_ver = from_file.or(from_engines);

    parts.runtime = Some(match node_ver.as_deref().and_then(major) {
        Some(major) => format!("Node {major}"),
        None => "Node".to_string(),
    });

    parts.pm = Some(if exists(cwd, "pnpm-lock.yaml") {
        "pnpm".into()
    } else if exists(cwd, "yarn.lock") {
        "yarn".into()
    } else {
        "npm".into()
    });
    let pm = parts.pm.clone().unwrap();

    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("build") {
            parts.build = Some(format!("{pm} run build"));
        }
        if let Some(t) = scripts.get("test").and_then(|v| v.as_str()) {
            if !t.contains("no test specified") {
                parts.test = Some(format!("{pm} test"));
            }
        }
        if scripts.contains_key("start") {
            parts.start = Some(format!("{pm} start"));
        } else if scripts.contains_key("dev") {
            parts.start = Some(format!("{pm} run dev"));
        }
    }

    // Prefer first bin entry over main field.
    if let Some(bin) = pkg.get("bin") {
        let first = match bin {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(m) => {
                m.values().next().and_then(|v| v.as_str()).map(str::to_string)
            }
            _ => None,
        };
        if let Some(p) = first {
            parts.entry = Some(p);
        }
    }
    if parts.entry.is_none() {
        if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
            parts.entry = Some(main.to_string());
        }
    }

    // Merge dependencies + devDependencies and pick first known framework.
    let mut deps: BTreeMap<String, ()> = BTreeMap::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(obj) = pkg.get(key).and_then(|v| v.as_object()) {
            for k in obj.keys() {
                deps.insert(k.clone(), ());
            }
        }
    }
    if let Some(fw) = node_framework(&deps) {
        parts.framework = Some(fw.to_string());
    }
}

fn scan_python(cwd: &Path, parts: &mut BootstrapParts) {
    let pyver_file = read(cwd, ".python-version");
    let pyver = trim_nonempty(&pyver_file)
        .or_else(|| parse_tool_versions(&read(cwd, ".tool-versions"), "python"));
    parts.runtime = Some(match pyver.as_deref() {
        Some(v) => {
            let parts_v: Vec<&str> = v.split('.').take(2).collect();
            format!("Python {}", parts_v.join("."))
        }
        None => "Python".to_string(),
    });
    parts.pm = Some(if exists(cwd, "poetry.lock") {
        "poetry".into()
    } else if exists(cwd, "Pipfile.lock") {
        "pipenv".into()
    } else {
        "pip".into()
    });

    let blob = format!("{}\n{}", read(cwd, "pyproject.toml"), read(cwd, "requirements.txt"));
    if let Some(fw) = python_framework(&blob) {
        parts.framework = Some(fw.into());
    }

    if exists(cwd, "manage.py") {
        parts.entry = Some("manage.py".into());
        if parts.framework.is_none() {
            parts.framework = Some("django".into());
        }
    } else if exists(cwd, "app.py") {
        parts.entry = Some("app.py".into());
    } else if exists(cwd, "main.py") {
        parts.entry = Some("main.py".into());
    }
}

fn scan_rust(cwd: &Path, parts: &mut BootstrapParts) {
    parts.runtime = Some("Rust".into());
    parts.build = Some("cargo build".into());
    parts.test = Some("cargo test".into());
    let cargo = read(cwd, "Cargo.toml");
    static BIN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"\[\[bin\]\]\s*\nname\s*=\s*"([^"]+)""#).expect("valid regex literal"));
    if let Some(c) = BIN_RE.captures(&cargo) {
        parts.entry = Some(format!("src/{}.rs", &c[1]));
    } else if exists(cwd, "src/main.rs") {
        parts.entry = Some("src/main.rs".into());
    } else if exists(cwd, "src/lib.rs") {
        parts.entry = Some("src/lib.rs".into());
    }
}

fn scan_go(cwd: &Path, parts: &mut BootstrapParts) {
    let gomod = read(cwd, "go.mod");
    static GO_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^go\s+(\d+\.\d+)").expect("valid regex literal"));
    let ver = GO_RE.captures(&gomod).map(|c| c[1].to_string());
    parts.runtime = Some(match ver {
        Some(v) => format!("Go {v}"),
        None => "Go".into(),
    });
    parts.build = Some("go build ./...".into());
    parts.test = Some("go test ./...".into());
    if exists(cwd, "main.go") {
        parts.entry = Some("main.go".into());
    } else if exists(cwd, "cmd") {
        parts.entry = Some("cmd/".into());
    }
}

fn scan_ruby(cwd: &Path, parts: &mut BootstrapParts) {
    let rbver = trim_nonempty(&read(cwd, ".ruby-version"))
        .or_else(|| parse_tool_versions(&read(cwd, ".tool-versions"), "ruby"));
    parts.runtime = Some(match rbver.as_deref() {
        Some(v) => {
            let parts_v: Vec<&str> = v.split('.').take(2).collect();
            format!("Ruby {}", parts_v.join("."))
        }
        None => "Ruby".into(),
    });
    parts.pm = Some("bundler".into());
    if exists(cwd, ".rspec") || exists(cwd, "spec") {
        parts.test = Some("bundle exec rspec".into());
    } else if exists(cwd, "Rakefile") {
        parts.test = Some("rake test".into());
    }
}

fn compose(p: &BootstrapParts) -> String {
    let mut segs: Vec<String> = Vec::new();
    let runtime = p.runtime.clone().unwrap_or_default();
    match &p.pm {
        Some(pm) => segs.push(format!("{runtime} ({pm})")),
        None => segs.push(runtime),
    }
    if let Some(fw) = &p.framework {
        segs.push(fw.clone());
    }
    if let Some(entry) = &p.entry {
        segs.push(format!("entry: {entry}"));
    }
    let mut cmds: Vec<String> = Vec::new();
    if let Some(b) = &p.build {
        cmds.push(format!("build: `{b}`"));
    }
    if let Some(t) = &p.test {
        cmds.push(format!("test: `{t}`"));
    }
    if let Some(s) = &p.start {
        cmds.push(format!("run: `{s}`"));
    }
    if !cmds.is_empty() {
        segs.push(cmds.join(", "));
    }
    segs.join(" — ")
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn glob_ext(dir: &Path, ext: &str) -> Vec<String> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    read.filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if Path::new(&name).extension().map(|x| x == ext).unwrap_or(false) {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

fn parse_tool_versions(content: &str, tool: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let pat = format!(r"(?im)^{tool}\s+([\d.]+)");
    Regex::new(&pat).ok().and_then(|re| re.captures(content)).map(|c| c[1].to_string())
}

fn trim_nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

fn strip_version(v: &str) -> String {
    v.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect()
}

fn major(v: &str) -> Option<String> {
    v.split('.').next().filter(|s| !s.is_empty()).map(str::to_string)
}

fn node_framework<K: AsRef<str>>(deps: &BTreeMap<K, ()>) -> Option<&'static str> {
    let has = |name: &str| deps.keys().any(|k| k.as_ref() == name);
    if has("next") {
        Some("Next.js")
    } else if has("nuxt") {
        Some("Nuxt.js")
    } else if has("@angular/core") {
        Some("Angular")
    } else if has("react") {
        Some("React")
    } else if has("vue") {
        Some("Vue")
    } else if has("svelte") {
        Some("Svelte")
    } else if has("express") {
        Some("Express")
    } else if has("fastify") {
        Some("Fastify")
    } else if has("koa") {
        Some("Koa")
    } else if has("nestjs") || has("@nestjs/core") {
        Some("NestJS")
    } else if has("electron") {
        Some("Electron")
    } else {
        None
    }
}

fn python_framework(content: &str) -> Option<&'static str> {
    let lc = content.to_lowercase();
    if lc.contains("fastapi") {
        Some("FastAPI")
    } else if lc.contains("django") {
        Some("Django")
    } else if lc.contains("flask") {
        Some("Flask")
    } else if lc.contains("starlette") {
        Some("Starlette")
    } else if lc.contains("aiohttp") {
        Some("aiohttp")
    } else if lc.contains("tornado") {
        Some("Tornado")
    } else {
        None
    }
}
