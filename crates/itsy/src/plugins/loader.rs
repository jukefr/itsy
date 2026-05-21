//! Plugin loader for itsy.
//!
//! Plugins extend itsy with:
//!   - New tools (custom tool definitions + executors)
//!   - System-prompt injections (always or conditional)
//!   - Event hooks (pre/post tool, session start/end)
//!   - Custom commands (/slash commands)
//!
//! Plugin locations:
//!   `.itsy/plugins/`                    — project-level
//!   `~/.config/itsy/plugins/`           — user-level (global)
//!
//! Plugin format: a directory with `plugin.json` manifest + executable handlers,
//! or a single `*.json` manifest (typically containing only prompt injections).
//!
//! `plugin.json` schema (subset honored by the Rust port):
//! ```json
//! {
//!   "name": "my-plugin",
//!   "version": "1.0.0",
//!   "description": "What this plugin does",
//!   "enabled": true,
//!   "tools": [{
//!     "name": "my_tool",
//!     "description": "...",
//!     "parameters": { "type": "object", "properties": {} },
//!     "cmd": ["./bin/handler", "--arg"]
//!   }],
//!   "prompts": [{ "inject": "always|backend|coding|debugging", "content": "..." }],
//!   "commands": [{ "name": "/mycmd", "description": "...", "cmd": ["./cmd"] }],
//!   "hooks": [{ "event": "post_tool", "filter": ["write_file"], "cmd": ["./hook"] }]
//! }
//! ```
//!
//! Plugins that ship only a JS `handler` field are gracefully skipped in the
//! Rust port (the Node runtime isn't available) — their schemas are still
//! exposed to the model, but invocation returns a friendly no-op error.
//!
//! Env knobs:
//!   `ITSY_PLUGINS=false`              disable all plugin loading
//!   `ITSY_PLUGINS_DISABLE=a,b`        comma-separated plugins to skip
//!   `ITSY_PLUGINS_ENABLE=a,b`         if set, only these plugins are loaded
//!   `ITSY_PLUGIN_TIMEOUT_SECS=15`     per-call timeout for subprocess plugins

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_params")]
    pub parameters: Value,
    /// Subprocess invocation: argv. Plugin receives JSON args on stdin and is
    /// expected to print a JSON result on stdout.
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    /// JS handler path — not supported in the Rust port (logged + skipped).
    #[serde(default)]
    pub handler: Option<String>,
}

fn default_params() -> Value {
    json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptInjection {
    /// `"always"`, or a task type: `backend`, `coding`, `debugging`, etc.
    #[serde(default = "default_inject")]
    pub inject: String,
    #[serde(default)]
    pub content: String,
}

fn default_inject() -> String {
    "always".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub handler: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub event: String,
    #[serde(default)]
    pub filter: Vec<String>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub handler: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub prompts: Vec<PromptInjection>,
    #[serde(default)]
    pub commands: Vec<CommandDef>,
    #[serde(default)]
    pub hooks: Vec<HookDef>,
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

/// Summary entry returned by [`PluginLoader::list`].
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub tools: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PluginLoader {
    pub plugins: Vec<PluginManifest>,
    pub tools: Vec<ToolDef>,
    pub commands: HashMap<String, (CommandDef, String)>, // name -> (def, plugin)
    pub prompts: Vec<(PromptInjection, String)>,
    pub hooks: Vec<(HookDef, String)>,
    tool_owner: HashMap<String, String>, // tool name -> plugin name
}

impl PluginLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load all plugins from `.itsy/plugins` under `root` and from the
    /// user-level `~/.config/itsy/plugins` directory.
    pub fn load_all(&mut self, root: &Path) {
        if !env_enabled() {
            return;
        }
        let enable_only = env_set("ITSY_PLUGINS_ENABLE");
        let disabled = env_set_or_empty("ITSY_PLUGINS_DISABLE");

        let mut dirs: Vec<PathBuf> = vec![root.join(".itsy").join("plugins")];
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".config").join("itsy").join("plugins"));
        }

        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if file_type.is_dir() {
                    self.load_plugin_dir(&path, &enable_only, &disabled);
                } else if path.extension().and_then(|s| s.to_str()) == Some("json")
                    && path.file_name().and_then(|s| s.to_str()) != Some("package.json")
                {
                    self.load_single_file(&path, &enable_only, &disabled);
                }
            }
        }
    }

    /// Backward-compatible alias used by the older Rust API.
    pub fn load_from(&mut self, root: &Path) {
        self.load_all(root);
    }

    fn load_plugin_dir(
        &mut self,
        plugin_dir: &Path,
        enable_only: &Option<HashSet<String>>,
        disabled: &HashSet<String>,
    ) {
        let manifest_path = plugin_dir.join("plugin.json");
        if !manifest_path.exists() {
            return;
        }
        let content = match fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut manifest: PluginManifest = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => return, // silently skip broken plugins (parity with JS)
        };
        if manifest.name.is_empty() {
            manifest.name = plugin_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string();
        }
        manifest.dir = Some(plugin_dir.to_path_buf());
        self.register(manifest, enable_only, disabled);
    }

    fn load_single_file(
        &mut self,
        path: &Path,
        enable_only: &Option<HashSet<String>>,
        disabled: &HashSet<String>,
    ) {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut manifest: PluginManifest = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => return,
        };
        if manifest.name.is_empty() {
            manifest.name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string();
        }
        manifest.dir = path.parent().map(|p| p.to_path_buf());
        // Single-file manifests typically only carry prompt injections; we
        // still honor any other declared field for parity.
        self.register(manifest, enable_only, disabled);
    }

    fn register(
        &mut self,
        manifest: PluginManifest,
        enable_only: &Option<HashSet<String>>,
        disabled: &HashSet<String>,
    ) {
        if !manifest.enabled {
            return;
        }
        if disabled.contains(&manifest.name) {
            return;
        }
        if let Some(only) = enable_only {
            if !only.contains(&manifest.name) {
                return;
            }
        }
        if !validate_manifest(&manifest) {
            return;
        }

        let plugin_name = manifest.name.clone();

        for tool in &manifest.tools {
            self.tools.push(tool.clone());
            self.tool_owner.insert(tool.name.clone(), plugin_name.clone());
        }
        for prompt in &manifest.prompts {
            self.prompts.push((prompt.clone(), plugin_name.clone()));
        }
        for cmd in &manifest.commands {
            self.commands
                .insert(cmd.name.clone(), (cmd.clone(), plugin_name.clone()));
        }
        for hook in &manifest.hooks {
            self.hooks.push((hook.clone(), plugin_name.clone()));
        }
        self.plugins.push(manifest);
    }

    /// Tools as the model would see them — schema-only, no executor refs.
    pub fn get_tools(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect()
    }

    /// Concatenated prompt injections matching `task_type` (or `"always"`).
    pub fn get_prompt_injections(&self, task_type: Option<&str>) -> String {
        self.prompts
            .iter()
            .filter(|(p, _)| {
                p.inject == "always"
                    || task_type.map(|t| t == p.inject).unwrap_or(false)
            })
            .map(|(p, _)| p.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Execute a plugin tool by name.
    ///
    /// * Plugins with a `cmd` argv spawn that subprocess, feed the JSON args
    ///   on stdin, and parse stdout as a JSON result (falling back to a wrap
    ///   in `{ "result": <stdout> }` if it isn't valid JSON).
    /// * Plugins that declared only a JS `handler` return `None` (graceful
    ///   no-op — the schema is still useful for the model to see).
    /// * Unknown tool name also returns `None`.
    pub async fn execute_tool(&self, name: &str, args: Value) -> Option<Value> {
        let tool = self.tools.iter().find(|t| t.name == name)?;
        let plugin_name = self.tool_owner.get(name).cloned().unwrap_or_default();
        let plugin_dir = self
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .and_then(|p| p.dir.clone());

        if let Some(cmd) = &tool.cmd {
            if cmd.is_empty() {
                return Some(json!({
                    "error": format!("plugin `{plugin_name}` tool `{name}` has empty cmd"),
                }));
            }
            return Some(run_subprocess(cmd, &args, plugin_dir.as_deref()).await);
        }
        if tool.handler.is_some() {
            return Some(json!({
                "error": format!(
                    "plugin `{plugin_name}` tool `{name}` uses a JS handler — not supported in the Rust runtime"
                ),
            }));
        }
        None
    }

    /// Execute a plugin slash-command.
    pub async fn execute_command(&self, name: &str, args: Value) -> Option<Value> {
        let (cmd_def, plugin_name) = self.commands.get(name)?.clone();
        let plugin_dir = self
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .and_then(|p| p.dir.clone());

        if let Some(cmd) = &cmd_def.cmd {
            if cmd.is_empty() {
                return Some(json!({
                    "error": format!("plugin `{plugin_name}` command `{name}` has empty cmd"),
                }));
            }
            return Some(run_subprocess(cmd, &args, plugin_dir.as_deref()).await);
        }
        if cmd_def.handler.is_some() {
            return Some(json!({
                "error": format!(
                    "plugin `{plugin_name}` command `{name}` uses a JS handler — not supported in the Rust runtime"
                ),
            }));
        }
        None
    }

    /// Summary list of all loaded plugins, for `/plugins`-style commands.
    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                name: p.name.clone(),
                version: p.version.clone(),
                description: p.description.clone(),
                tools: p.tools.iter().map(|t| t.name.clone()).collect(),
                commands: p.commands.iter().map(|c| c.name.clone()).collect(),
            })
            .collect()
    }
}

fn env_enabled() -> bool {
    !matches!(
        std::env::var("ITSY_PLUGINS").ok().as_deref(),
        Some("false") | Some("0") | Some("off") | Some("no")
    )
}

fn env_set(key: &str) -> Option<HashSet<String>> {
    std::env::var(key).ok().map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn env_set_or_empty(key: &str) -> HashSet<String> {
    env_set(key).unwrap_or_default()
}

// Adapter for the more permissive `disabled` lookup site.
fn _coerce_required(set: Option<HashSet<String>>) -> HashSet<String> {
    set.unwrap_or_default()
}

// Validate the plugin's config — names non-empty, each tool/command has
// either a cmd or a handler (or we still accept schema-only for tools).
fn validate_manifest(m: &PluginManifest) -> bool {
    if m.name.trim().is_empty() {
        return false;
    }
    for t in &m.tools {
        if t.name.trim().is_empty() {
            return false;
        }
    }
    for c in &m.commands {
        if c.name.trim().is_empty() {
            return false;
        }
    }
    for h in &m.hooks {
        if h.event.trim().is_empty() {
            return false;
        }
    }
    true
}

async fn run_subprocess(cmd: &[String], args: &Value, cwd: Option<&Path>) -> Value {
    let timeout_secs = std::env::var("ITSY_PLUGIN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15u64);

    let (program, rest) = match cmd.split_first() {
        Some(s) => s,
        None => return json!({"error": "empty plugin cmd"}),
    };
    let mut child_cmd = Command::new(program);
    child_cmd.args(rest);
    if let Some(dir) = cwd {
        child_cmd.current_dir(dir);
    }
    child_cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match child_cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("failed to spawn plugin: {e}")}),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(args).unwrap_or_else(|_| b"{}".to_vec());
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.shutdown().await;
    }

    let fut = child.wait_with_output();
    let output = match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return json!({"error": format!("plugin io error: {e}")}),
        Err(_) => return json!({"error": format!("plugin timed out after {timeout_secs}s")}),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return json!({
            "error": format!(
                "plugin exited with status {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return json!({"result": ""});
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => v,
        Err(_) => json!({ "result": stdout }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Silence the helper-not-yet-used warning in test-free builds.
    #[allow(dead_code)]
    fn _hush() {
        let _ = env_set_or_empty("ITSY_PLUGINS_DISABLE");
        let _ = _coerce_required(None);
    }

    #[test]
    fn prompt_filtering_by_task_type() {
        let mut l = PluginLoader::new();
        l.prompts.push((
            PromptInjection { inject: "always".into(), content: "A".into() },
            "p1".into(),
        ));
        l.prompts.push((
            PromptInjection { inject: "backend".into(), content: "B".into() },
            "p2".into(),
        ));
        l.prompts.push((
            PromptInjection { inject: "coding".into(), content: "C".into() },
            "p3".into(),
        ));
        let s = l.get_prompt_injections(Some("backend"));
        assert!(s.contains('A') && s.contains('B') && !s.contains('C'));
    }

    #[test]
    fn schema_only_tool_executes_to_none() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut l = PluginLoader::new();
        l.tools.push(ToolDef {
            name: "noop".into(),
            description: "".into(),
            parameters: default_params(),
            cmd: None,
            handler: None,
        });
        let r = rt.block_on(l.execute_tool("noop", json!({})));
        assert!(r.is_none());
    }
}
