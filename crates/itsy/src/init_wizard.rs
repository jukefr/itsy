//! Interactive setup wizard. Runs on first launch (no config file present)
//! and is also exposed via `itsy --init`. Writes a fully-formed
//! `~/.config/itsy/config.toml` so subsequent launches can boot without
//! prompting.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use crate::config::{
    ConfigFile, ContextConfig, GitConfig, ModelConfig, ToolsConfig, TuiConfig,
    CURRENT_CONFIG_VERSION,
};
use crate::paths;

#[derive(Debug, Clone, Copy)]
struct Provider {
    /// Display name shown in the wizard banner. Constructed but not
    /// read in the current code path — kept for completeness so future
    /// banner tweaks don't have to redefine the table.
    #[allow(dead_code)]
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

// ---------------------------------------------------------------------------
// Model name introspection
// ---------------------------------------------------------------------------

/// Inspected facts about a model name. All best-effort heuristics.
#[derive(Debug, Clone, Default)]
pub struct ModelHints {
    pub family: Option<String>,
    pub quant: Option<String>,
    pub quant_tier: &'static str,
    pub dense_total_b: Option<f64>,
    pub moe_experts: Option<u32>,
    pub moe_per_expert_b: Option<f64>,
    pub active_params_b: Option<f64>,
    pub is_reasoning: bool,
}

// Quantization marker, e.g. Q4_K_M, IQ2_XXS, Q8_0, F16, BF16, FP16.
// Anchored to a word boundary so we don't catch random Q-letter substrings.
static QUANT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(IQ[1-4](?:_[A-Z0-9]+)*|Q[2-8](?:_[KM0-9]+)*|F16|FP16|BF16|F32)\b")
        .expect("quant regex")
});

// Active params marker for MoE, e.g. A3B, A22B
static ACTIVE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)A(\d+(?:\.\d+)?)B\b").expect("active regex"));

// Mixture-of-experts shorthand, e.g. 8x7B, 8x22B
static MOE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d+)x(\d+(?:\.\d+)?)B\b").expect("moe regex"));

// Dense total param marker, e.g. 7B, 35B, 1.5B
static DENSE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)B\b").expect("dense regex"));

// Reasoning-model name fragments.
//
// The Claude family names we treat as reasoning-capable are 3.7 and any 4.x,
// regardless of the line (opus/sonnet/haiku). So we match either the literal
// `claude-3-7`/`claude-3.7` token *or* any `claude-…-4(-…)?` shape.
static REASONING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        (?: (?:^|[^a-z]) (?: o1 | o3 | o4 | qwen3 | qwq | deepseek-r | deepseek-v3-reason ) (?:[^a-z]|$) )
        |
        (?: claude (?: -[a-z]+ )* - (?: 3[._-]7 | 4 ) (?: [^a-z] | $ ) )
        ",
    )
    .expect("reasoning regex")
});

const FAMILIES: &[&str] = &[
    "gemma", "qwen", "llama", "mistral", "phi", "deepseek", "codellama", "claude", "gpt",
];

fn quant_tier_for(marker: &str) -> &'static str {
    let m = marker.to_uppercase();
    if m.starts_with("IQ1") || m.starts_with("IQ2") || m.starts_with("Q2") {
        "tiny"
    } else if m.starts_with("IQ3") || m.starts_with("Q3") {
        "low"
    } else if m.starts_with("IQ4") || m.starts_with("Q4") {
        "balanced"
    } else if m.starts_with("Q5") || m.starts_with("Q6") {
        "good"
    } else if m.starts_with("Q8") || m == "F16" || m == "FP16" || m == "BF16" || m == "F32" {
        "high"
    } else {
        "unknown"
    }
}

/// Parse a model name string into best-effort hints.
pub fn detect_model_hints(model_name: &str) -> ModelHints {
    let lower = model_name.to_lowercase();
    let mut hints = ModelHints { quant_tier: "unknown", ..Default::default() };

    // Family
    for fam in FAMILIES {
        if lower.contains(fam) {
            hints.family = Some((*fam).into());
            break;
        }
    }

    // Quantization
    if let Some(cap) = QUANT_RE.captures(model_name) {
        let raw = cap.get(1).unwrap().as_str();
        hints.quant = Some(raw.to_string());
        hints.quant_tier = quant_tier_for(raw);
    }

    // MoE: NxYB
    if let Some(cap) = MOE_RE.captures(model_name) {
        if let (Some(x), Some(y)) = (cap.get(1), cap.get(2)) {
            if let (Ok(experts), Ok(per)) = (x.as_str().parse::<u32>(), y.as_str().parse::<f64>()) {
                hints.moe_experts = Some(experts);
                hints.moe_per_expert_b = Some(per);
            }
        }
    }

    // Active params (e.g. A3B)
    if let Some(cap) = ACTIVE_RE.captures(model_name) {
        if let Ok(v) = cap.get(1).unwrap().as_str().parse::<f64>() {
            hints.active_params_b = Some(v);
        }
    }

    // Dense total — only when neither MoE nor active marker matched, to avoid
    // double-counting the same digits. Pick the largest "NB" token found.
    if hints.moe_experts.is_none() && hints.active_params_b.is_none() {
        let mut best: Option<f64> = None;
        for cap in DENSE_RE.captures_iter(model_name) {
            if let Ok(v) = cap.get(1).unwrap().as_str().parse::<f64>() {
                if best.map(|b| v > b).unwrap_or(true) {
                    best = Some(v);
                }
            }
        }
        hints.dense_total_b = best;
    }

    // Reasoning
    hints.is_reasoning = REASONING_RE.is_match(&lower);

    hints
}

// ---------------------------------------------------------------------------
// /models probe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ModelsListEnvelope {
    #[serde(default)]
    data: Vec<ModelsListEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelsListEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    max_context_length: Option<u32>,
    // Some servers (OpenRouter, LM Studio) nest extras like this.
    #[serde(default)]
    top_provider: Option<TopProvider>,
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u32>,
}

/// Probe an OpenAI-compatible `/models` endpoint to discover the actual
/// context length the server advertises. Returns None on network failure
/// or when the server does not surface a context length.
pub async fn probe_context_window(base_url: &str, model_name: &str) -> Option<u32> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let env: ModelsListEnvelope = resp.json().await.ok()?;
    let needle = model_name.to_lowercase();
    // Prefer exact ID match; fall back to substring match.
    let mut exact: Option<&ModelsListEntry> = None;
    let mut fuzzy: Option<&ModelsListEntry> = None;
    for entry in &env.data {
        if let Some(id) = entry.id.as_deref() {
            let id_l = id.to_lowercase();
            if id_l == needle {
                exact = Some(entry);
                break;
            } else if !needle.is_empty() && (id_l.contains(&needle) || needle.contains(&id_l)) {
                fuzzy.get_or_insert(entry);
            }
        }
    }
    let pick = exact.or(fuzzy)?;
    pick.context_length
        .or(pick.max_context_length)
        .or_else(|| pick.top_provider.as_ref().and_then(|tp| tp.context_length))
}

fn family_default_window(family: Option<&str>, model_name_lower: &str) -> u32 {
    match family {
        Some("gemma") if model_name_lower.contains("4") => 32_768,
        Some("qwen") if model_name_lower.contains("qwen3") => 32_768,
        Some("mistral") if model_name_lower.contains("nemo") => 128_000,
        _ => 8_192,
    }
}

fn budget_pct_for(window: u32) -> u32 {
    if window >= 32_000 {
        70
    } else if window >= 16_000 {
        60
    } else {
        50
    }
}

fn routing_for(window: u32) -> &'static str {
    if window <= 16_000 { "two_stage" } else { "direct" }
}

fn synchronous_probe(base_url: &str, model_name: &str) -> Option<u32> {
    // Wizard is called from a tokio runtime context (#[tokio::main]).
    // Use block_in_place + the current handle so we can stay synchronous
    // here without spawning a fresh runtime.
    if model_name.is_empty() {
        return None;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(probe_context_window(base_url, model_name)))
    } else {
        // Fallback: spin up a single-thread runtime just for the probe.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(probe_context_window(base_url, model_name))
    }
}

fn print_detected(hints: &ModelHints, window: u32, source: &str) {
    println!("\n  Detected:");
    if let Some(fam) = &hints.family {
        println!("    family       : {fam}");
    } else {
        println!("    family       : (unknown)");
    }
    match (&hints.quant, hints.quant_tier) {
        (Some(q), tier) => println!("    quant        : {q}  (tier: {tier})"),
        (None, _) => println!("    quant        : (none / full precision)"),
    }
    if let (Some(experts), Some(per)) = (hints.moe_experts, hints.moe_per_expert_b) {
        println!("    params       : MoE {experts}x{per}B");
    } else if let Some(active) = hints.active_params_b {
        println!("    params       : active {active}B (MoE-style A*B marker)");
    } else if let Some(total) = hints.dense_total_b {
        println!("    params       : dense {total}B");
    } else {
        println!("    params       : (unknown)");
    }
    println!("    reasoning    : {}", if hints.is_reasoning { "yes" } else { "no" });
    println!("    context window: {window} ({source})");
    if hints.is_reasoning {
        println!("    note: reasoning model — thinking-budget fields will be sent.");
    }
}

/// Run the wizard interactively, write the resulting config to disk, and
/// return the populated [`ConfigFile`].
pub fn run() -> Result<ConfigFile> {
    // While this guard is held, a Ctrl+C cleanly exits the process
    // (the synchronous read_line() calls below block in the kernel and
    // can't observe a cooperative interrupt counter).
    let _interrupt_guard = crate::interrupt::WizardGuard::enter();

    println!("\n  ⚡ itsy Setup");
    println!("  ─────────────────────────────────\n");
    println!("  No config found at {}.", paths::config_file().display());
    println!("  Let's create one.  (Ctrl+C cancels.)\n");

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

    // Detect hints from the model name and probe the server for a real
    // context length. The probe is best-effort: a network failure just
    // falls back to the family default.
    let hints = detect_model_hints(&model_name);
    let normalized_base = crate::config::normalize_base_url(&base_url);
    let lower_name = model_name.to_lowercase();
    let family_default = family_default_window(hints.family.as_deref(), &lower_name);

    let (detected_window, window_source) = match synchronous_probe(&normalized_base, &model_name) {
        Some(w) => (w, "from /models"),
        None => (family_default, "family default"),
    };

    print_detected(&hints, detected_window, window_source);

    // Defaults derived from the detected window + quant tier.
    let default_budget = budget_pct_for(detected_window);
    let default_routing = routing_for(detected_window);
    let is_tiny = hints.quant_tier == "tiny";
    let default_bash_timeout: u32 = if is_tiny && hints.is_reasoning { 60 } else { 30 };

    if is_tiny {
        println!(
            "  ⚠ {} quant detected — output quality may be limited.",
            hints.quant.clone().unwrap_or_else(|| "tiny".into())
        );
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

    // Override prompts for the smart-detected values so users can tweak.
    let budget_str = ask(
        "  Context budget percent",
        &default_budget.to_string(),
    );
    let max_budget_pct: u32 = budget_str.parse().unwrap_or(default_budget);

    let window_str = ask(
        "  Context window (tokens)",
        &detected_window.to_string(),
    );
    let detected_window_final: u32 = window_str.parse().unwrap_or(detected_window);

    let timeout_str = ask("  Bash tool timeout (seconds)", &default_bash_timeout.to_string());
    let bash_timeout: u32 = timeout_str.parse().unwrap_or(default_bash_timeout);

    let routing_choice = ask(
        "  Tool routing (direct / two_stage)",
        default_routing,
    );
    let tool_routing = match routing_choice.trim().to_lowercase().as_str() {
        "two_stage" | "two-stage" | "twostage" => "two_stage".into(),
        _ => "direct".into(),
    };

    // Small-model safeguards — bulk-on for tiny quants, otherwise ask
    // whether to enable. All are individually overridable via `ITSY_*`
    // env vars or by editing the TOML later.
    let safeguards_default = matches!(hints.quant_tier, "tiny" | "low" | "balanced" | "unknown");
    let safeguards_all = ask_bool(
        "Enable small-model safeguards (snapshot/write-guard/trust-decay/bootstrap)?",
        safeguards_default,
    );

    // Thinking-budget cap for reasoning models. 0 = use per-task heuristic.
    let thinking_budget: u32 = if hints.is_reasoning {
        let default = "8000".to_string();
        ask("  Thinking-token budget per turn (0 = heuristic)", &default)
            .parse()
            .unwrap_or(8000)
    } else {
        0
    };

    // LLM-cost features — each adds a round-trip per turn or per failure.
    // Auto-suggest on for tiny quants (where recovery is most valuable)
    // since they fail more often; off by default otherwise to keep
    // latency tight on bigger models.
    let llm_recover_default = safeguards_default;
    let semantic_merge = ask_bool(
        "Recover from failed patches with an LLM merge call?",
        llm_recover_default,
    );
    let error_diagnosis = ask_bool(
        "Diagnose bash failures with an LLM hint?",
        llm_recover_default,
    );
    let clarifier = ask_bool(
        "Ask the model to clarify when the user message is vague?",
        true,
    );
    let context_retrieval = ask_bool(
        "Inject code-graph hits into the system prompt for long messages?",
        true,
    );
    let validate_edits = ask_bool(
        "LLM self-critique after every successful write/patch? (extra LLM call per edit)",
        false,
    );
    let features = crate::config::FeaturesConfig {
        snapshot: safeguards_all,
        snapshot_auto_rollback: safeguards_all,
        write_guard: safeguards_all,
        bootstrap: safeguards_all,
        bootstrap_max_chars: 4000,
        trust_decay: safeguards_all,
        temp_adapt: safeguards_all,
        thinking_budget,
        clarifier,
        semantic_merge,
        error_diagnosis,
        validate_edits,
        context_retrieval,
        contract: true,
    };

    let web_browse = ask_bool("Enable web_search / web_fetch tools?", true);
    let shell_persist = ask_bool("Use a persistent shell (cd src; ls works as expected)?", true);
    let allow_outside_paths = ask_bool(
        "Allow read/write tools to touch absolute paths outside the project root (e.g. /data, /tmp)?",
        true,
    );

    let file = ConfigFile {
        version: CURRENT_CONFIG_VERSION.into(),
        model: Some(ModelConfig {
            provider: "openai".into(),
            name: model_name,
            base_url: normalized_base,
            timeout: 600,
            api_key,
        }),
        context: Some(ContextConfig {
            max_budget_pct,
            detected_window: detected_window_final,
            working_memory_tokens: 500,
            summary_threshold: 200,
        }),
        tools: Some(ToolsConfig {
            bash_timeout,
            tool_routing,
            web_browse,
            shell_persist,
            shell_contain: false,
            rtk: true,
        }),
        tui: Some(TuiConfig {
            show_token_usage: true,
            auto_approve,
            theme,
            classic: false,
        }),
        git: Some(GitConfig { auto_commit }),
        features: Some(features),
        models: None,
        limits: Some(crate::config::LimitsConfig::default()),
        security: Some(crate::config::SecurityConfig {
            allow_outside_paths,
            ..Default::default()
        }),
        diff: Some(crate::config::DiffConfig::default()),
        filetree: Some(crate::config::FileTreeConfig::default()),
        snapshots: Some(crate::config::SnapshotPathsConfig::default()),
        code_graph: Some(crate::config::CodeGraphConfig::default()),
        tests: Some(crate::config::TestsConfig::default()),
        traces: Some(crate::config::TracesConfig::default()),
        dedup: Some(crate::config::DedupConfig::default()),
        evidence: Some(crate::config::EvidenceConfig::default()),
        plugins: Some(crate::config::PluginsConfig::default()),
        diag: Some(crate::config::DiagConfig::default()),
        second_opinion: None,
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
    // (write_default no longer reads ITSY_MODEL/ITSY_BASE_URL — pass
    //  values via --init/--model/--endpoint or fill in the TOML.)
    let file = ConfigFile {
        version: CURRENT_CONFIG_VERSION.into(),
        model: Some(ModelConfig {
            provider: "openai".into(),
            name: String::new(),
            base_url: "http://localhost:1234/v1".into(),
            timeout: 300,
            api_key: None,
        }),
        context: Some(ContextConfig {
            max_budget_pct: 70,
            detected_window: 128_000,
            working_memory_tokens: 500,
            summary_threshold: 200,
        }),
        tools: Some(ToolsConfig {
            bash_timeout: 30,
            tool_routing: "direct".into(),
            web_browse: false,
            shell_persist: true,
            shell_contain: false,
            rtk: true,
        }),
        tui: Some(TuiConfig {
            show_token_usage: true,
            auto_approve: false,
            theme: "dark".into(),
            classic: false,
        }),
        git: Some(GitConfig { auto_commit: false }),
        features: Some(crate::config::FeaturesConfig::default()),
        models: None,
        limits: Some(crate::config::LimitsConfig::default()),
        security: Some(crate::config::SecurityConfig::default()),
        diff: Some(crate::config::DiffConfig::default()),
        filetree: Some(crate::config::FileTreeConfig::default()),
        snapshots: Some(crate::config::SnapshotPathsConfig::default()),
        code_graph: Some(crate::config::CodeGraphConfig::default()),
        tests: Some(crate::config::TestsConfig::default()),
        traces: Some(crate::config::TracesConfig::default()),
        dedup: Some(crate::config::DedupConfig::default()),
        evidence: Some(crate::config::EvidenceConfig::default()),
        plugins: Some(crate::config::PluginsConfig::default()),
        diag: Some(crate::config::DiagConfig::default()),
        second_opinion: None,
    };
    paths::ensure_config_dirs()?;
    file.save_to_path(&paths::config_file())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qwen3_iq2_gguf() {
        let h = detect_model_hints("unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS");
        assert_eq!(h.family.as_deref(), Some("qwen"));
        assert_eq!(h.quant.as_deref(), Some("IQ2_XXS"));
        assert_eq!(h.quant_tier, "tiny");
        assert_eq!(h.active_params_b, Some(3.0));
        // Active-params marker present, so dense_total_b should be untouched
        // to avoid double-counting the "35B" portion.
        assert!(h.dense_total_b.is_none());
        assert!(h.is_reasoning, "qwen3 should be flagged as reasoning");
    }

    #[test]
    fn detects_gpt_4o_mini() {
        let h = detect_model_hints("gpt-4o-mini");
        assert_eq!(h.family.as_deref(), Some("gpt"));
        assert!(h.quant.is_none());
        assert_eq!(h.quant_tier, "unknown");
        assert!(!h.is_reasoning);
        // "4" is not "4B" so no params should be picked up.
        assert!(h.dense_total_b.is_none());
        assert!(h.active_params_b.is_none());
    }

    #[test]
    fn detects_claude_sonnet_45() {
        let h = detect_model_hints("claude-sonnet-4-5");
        assert_eq!(h.family.as_deref(), Some("claude"));
        // claude-4 family bucket
        assert!(h.is_reasoning);
        assert!(h.quant.is_none());
    }

    #[test]
    fn detects_o3_mini_reasoning() {
        let h = detect_model_hints("o3-mini");
        assert!(h.is_reasoning, "o3-mini must be flagged reasoning");
        assert_eq!(h.quant_tier, "unknown");
    }

    #[test]
    fn detects_mixtral_moe() {
        let h = detect_model_hints("Mixtral-8x7B-Instruct-v0.1-Q5_K_M");
        assert_eq!(h.moe_experts, Some(8));
        assert_eq!(h.moe_per_expert_b, Some(7.0));
        assert_eq!(h.quant.as_deref(), Some("Q5_K_M"));
        assert_eq!(h.quant_tier, "good");
    }
}
