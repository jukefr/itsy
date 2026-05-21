//! Interactive setup wizard. Runs on first launch (no config file present)
//! and is also exposed via `itsy --init`. Writes a fully-formed
//! `~/.config/itsy/config.toml` so subsequent launches can boot without
//! prompting.

use std::io::{self, BufRead, Write};

use anyhow::Result;

use crate::config::{
    ConfigFile, ContextConfig, EscalationConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
    CURRENT_CONFIG_VERSION,
};
use crate::paths;

#[derive(Debug, Clone, Copy)]
struct Provider {
    name: &'static str,
    default_url: &'static str,
    needs_key: bool,
}

fn provider_for(choice: &str) -> (&'static str, Provider) {
    match choice {
        "1" => ("lmstudio", Provider { name: "LM Studio", default_url: "http://localhost:1234/v1", needs_key: false }),
        "2" => ("ollama", Provider { name: "Ollama", default_url: "http://localhost:11434/v1", needs_key: false }),
        "3" => ("openrouter", Provider { name: "OpenRouter", default_url: "https://openrouter.ai/api/v1", needs_key: true }),
        "4" => ("openai", Provider { name: "OpenAI", default_url: "https://api.openai.com/v1", needs_key: true }),
        _ => ("custom", Provider { name: "Custom", default_url: "", needs_key: false }),
    }
}

fn ask(prompt: &str, default: &str) -> String {
    let label = if default.is_empty() { format!("{prompt}: ") } else { format!("{prompt} [{default}]: ") };
    print!("{label}");
    io::stdout().flush().ok();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    let trimmed = line.trim();
    if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() }
}

fn ask_bool(prompt: &str, default: bool) -> bool {
    let d = if default { "Y/n" } else { "y/N" };
    let ans = ask(&format!("  {prompt} ({d})"), if default { "y" } else { "n" });
    matches!(ans.trim().to_lowercase().as_str(), "y" | "yes" | "true" | "1")
}

/// True if no config file is present at the canonical location.
pub fn is_first_launch() -> bool {
    !paths::config_file().exists()
}

/// Run the wizard interactively, write the resulting config to disk, and
/// return the populated [`ConfigFile`].
pub fn run() -> Result<ConfigFile> {
    println!("\n  ⚡ itsy Setup");
    println!("  ─────────────────────────────────\n");
    println!("  No config found at {}.", paths::config_file().display());
    println!("  Let's create one.\n");

    println!("  Providers:");
    println!("    1) LM Studio       (local, default port 1234)");
    println!("    2) Ollama          (local, default port 11434)");
    println!("    3) OpenRouter      (cloud — needs API key)");
    println!("    4) OpenAI          (cloud — needs API key)");
    println!("    5) Custom endpoint");
    println!();

    let choice = ask("  Choose provider (1-5)", "1");
    let (_, prov) = provider_for(&choice);

    let base_url = if prov.default_url.is_empty() {
        ask("  Base URL (e.g. http://host:port/v1)", "http://localhost:1234/v1")
    } else {
        ask("  Base URL", prov.default_url)
    };

    let model_name = ask("  Model name (as shown by the server)", "");
    if model_name.is_empty() {
        println!("  ⚠ No model specified. /model later, or set ITSY_MODEL.");
    }

    let api_key = if prov.needs_key {
        let k = ask("  API key", "");
        if k.is_empty() {
            println!("  ⚠ No API key. Set OPENAI_API_KEY in env later.");
        }
        if k.is_empty() { None } else { Some(k) }
    } else if ask_bool("Does your server require an API key?", false) {
        let k = ask("  API key", "");
        if k.is_empty() { None } else { Some(k) }
    } else {
        None
    };

    let auto_approve = ask_bool("Auto-approve tool calls? (no = ask each time)", false);
    let auto_commit = ask_bool("Auto-commit changes after each turn?", false);
    let theme = ask("  Theme (dark / light)", "dark");

    let file = ConfigFile {
        version: CURRENT_CONFIG_VERSION.into(),
        model: Some(ModelConfig {
            provider: "openai".into(),
            name: model_name,
            base_url: crate::config::normalize_base_url(&base_url),
            timeout: 300,
            api_key,
        }),
        context: Some(ContextConfig {
            max_budget_pct: 70,
            detected_window: 128_000,
            working_memory_tokens: 500,
            summary_threshold: 200,
        }),
        tools: Some(ToolsConfig { bash_timeout: 30 }),
        tui: Some(TuiConfig {
            show_token_usage: true,
            auto_approve,
            theme,
            classic: false,
        }),
        escalation: Some(EscalationConfig {
            enabled: true,
            max_per_session: 5,
            confirm: true,
            provider: None,
            api_key: None,
            model: None,
        }),
        git: Some(GitConfig { auto_commit }),
        models: None,
    };

    paths::ensure_config_dirs()?;
    let path = paths::config_file();
    file.save_to_path(&path)?;
    println!("\n  ✓ Wrote {}", path.display());
    println!("    State for each project lives under {}/projects/<id>/.\n", paths::config_dir().display());
    Ok(file)
}

/// One-shot non-interactive write of a default config. Used by smoke tests
/// and by `--init --non-interactive`.
pub fn write_default() -> Result<ConfigFile> {
    let file = ConfigFile {
        version: CURRENT_CONFIG_VERSION.into(),
        model: Some(ModelConfig {
            provider: "openai".into(),
            name: std::env::var("ITSY_MODEL").unwrap_or_default(),
            base_url: std::env::var("ITSY_BASE_URL").unwrap_or_else(|_| "http://localhost:1234/v1".into()),
            timeout: 300,
            api_key: None,
        }),
        context: Some(ContextConfig {
            max_budget_pct: 70,
            detected_window: 128_000,
            working_memory_tokens: 500,
            summary_threshold: 200,
        }),
        tools: Some(ToolsConfig { bash_timeout: 30 }),
        tui: Some(TuiConfig {
            show_token_usage: true,
            auto_approve: false,
            theme: "dark".into(),
            classic: false,
        }),
        escalation: Some(EscalationConfig {
            enabled: true,
            max_per_session: 5,
            confirm: true,
            provider: None,
            api_key: None,
            model: None,
        }),
        git: Some(GitConfig { auto_commit: false }),
        models: None,
    };
    paths::ensure_config_dirs()?;
    file.save_to_path(&paths::config_file())?;
    Ok(file)
}
