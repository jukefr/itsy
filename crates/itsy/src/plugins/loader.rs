//! Loads plugin manifests from
//! `.itsy/plugins/*.json` and exposes their declared tool schemas.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub prompts: Option<String>,
}

#[derive(Debug, Default)]
pub struct PluginLoader {
    pub plugins: Vec<PluginManifest>,
}

impl PluginLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from(&mut self, root: &Path) {
        let dir = root.join(".itsy").join("plugins");
        if !dir.exists() {
            return;
        }
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(m) = serde_json::from_str::<PluginManifest>(&content) {
                    self.plugins.push(m);
                }
            }
        }
    }

    pub fn get_tools(&self) -> Vec<Value> {
        self.plugins.iter().flat_map(|p| p.tools.clone()).collect()
    }

    pub fn get_prompt_injections(&self, _task_type: Option<&str>) -> String {
        self.plugins
            .iter()
            .filter_map(|p| p.prompts.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn execute_tool(&self, _name: &str, _args: Value) -> Option<Value> {
        // Plugin execution surface is project-specific in the JS port; in the
        // Rust binary we expose the schemas but defer execution to callers
        // that register their own runners.
        None
    }
}
