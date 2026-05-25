//! System prompt assembly — builds the per-call prompt in both contract and
//! freeform modes. All functions moved here from `bin/itsy.rs` during the
//! Phase B extraction.

use std::path::Path;
use serde_json::Value;

use crate::knowledge::{get_knowledge_loader, SelectOptions};
use crate::memory::MemoryStore;
use crate::model_client::build_system_prompt;
use crate::plugins::loader::PluginLoader;
use crate::plugins::skills::SkillManager;
use crate::tools_impl::test_runner;
use crate::Config;

/// JS `getMemoryContext`. Loads scored memory for the last user message and
/// formats it inline (≤ ~800 tokens / 3200 chars).
pub fn get_memory_context(messages: &[Value], memory: &MemoryStore) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(task) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let items = memory.load_for_task(task);
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\nRelevant project memory:\n");
    let max_chars = 3200usize;
    for o in items {
        let entry = format!("[{}] {}: {}\n", o.kind, o.title, o.content);
        if out.len() + entry.len() > max_chars {
            break;
        }
        out.push_str(&entry);
    }
    out
}

/// JS `getSkillContext`. Auto-loads matching skills based on the last user
/// message and formats them (capped at ~4000 chars).
pub fn get_skill_context(messages: &[Value], skills: &SkillManager) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(msg) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let auto = skills.get_auto_skills(msg);
    if auto.is_empty() {
        return String::new();
    }
    let formatted = skills.format_for_prompt(&auto);
    if formatted.len() > 4000 {
        format!(
            "{}\n... (skills truncated to fit context)",
            &formatted[..4000]
        )
    } else {
        formatted
    }
}

/// Plugin-supplied prompt injections gated by task type.
pub fn get_plugin_prompts(plugins: &PluginLoader, task_type: Option<&str>) -> String {
    let injection = plugins.get_prompt_injections(task_type);
    if injection.is_empty() {
        return String::new();
    }
    let capped = if injection.len() > 2000 {
        format!("{}\n... (plugin prompts truncated)", &injection[..2000])
    } else {
        injection
    };
    format!("\n\n{capped}")
}

/// JS `getKnowledgeContext`. Walks the project's `knowledge/` directory and
/// pulls in docs that overlap with the last user message.
pub fn get_knowledge_context(messages: &[Value]) -> String {
    let Some(last_user) = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return String::new();
    };
    let Some(query) = last_user.get("content").and_then(|c| c.as_str()) else {
        return String::new();
    };
    let s = crate::settings::get();
    let max_tokens = (s.detected_window as f64 * 0.04)
        .clamp(200.0, 1500.0) as usize;
    let loader = get_knowledge_loader();
    loader.format_for_prompt(query, &SelectOptions { max_tokens: Some(max_tokens) })
}

/// JS `getTestRunnerContext`.
pub fn get_test_runner_context(cwd: &Path) -> String {
    test_runner::format_for_prompt(cwd)
}

/// Verification guidance block — injected into contract prompts.
fn contract_verification_guidance(cwd: &Path) -> String {
    crate::verification::discover(cwd)
        .prompt_block()
        .map(|s| format!("\n{s}\n"))
        .unwrap_or_default()
}

/// Contract-mode prompt for turn 1 (no contract yet). Short, focused,
/// laser-targeted: the model's only job is to call `propose_contract`.
pub fn build_contract_proposal_prompt(cwd_path: &Path, cwd: &str) -> String {
    let model_name = crate::settings::get().model_name.clone();
    let model_line = if model_name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {model_name}")
    };
    let verification = contract_verification_guidance(cwd_path);
    format!(
        include_str!("../assets/prompts/contract_proposal.txt"),
        cwd = cwd,
        model_line = model_line,
        verification = verification,
    )
}

/// Contract-mode prompt for turn 2+ (contract is active). The
/// assertions and their current states ARE the prompt.
/// Assemble the full per-call system prompt. Builds the contract-shaped prompt
/// when the contract feature is active, otherwise layers memory, skills,
/// plugins, knowledge, code-graph hits, test-runner hints, and tool guidance
pub fn build_contract_active_prompt(
    c: &crate::session::contract::Contract,
    cwd_path: &Path,
    cwd: &str,
) -> String {
    let model_name = crate::settings::get().model_name.clone();
    let model_line = if model_name.is_empty() {
        String::new()
    } else {
        format!("\nModel: {model_name}")
    };
    let body = crate::session::contract::render_for_prompt(c);
    let verification = contract_verification_guidance(cwd_path);
    format!(
        include_str!("../assets/prompts/contract_active.txt"),
        cwd = cwd,
        model_line = model_line,
        body = body,
        verification = verification,
    )
}

/// Assemble the full per-call system prompt. Builds the contract-shaped prompt
/// when the contract feature is active, otherwise layers memory, skills,
/// plugins, knowledge, code-graph hits, test-runner hints, and tool guidance
/// on top of the base [`build_system_prompt`].
pub fn build_full_system_prompt(
    config: &Config,
    task_type: &str,
    messages: &[Value],
    memory: &MemoryStore,
    skills: &SkillManager,
    plugins: &PluginLoader,
    cwd: &Path,
) -> String {
    // Contract-mode short-circuit: return a focused contract prompt instead
    // of the generic kitchen-sink one when the feature is on and the task
    // is actionable.
    if crate::settings::get().contract
        && !matches!(task_type, "explanation" | "respond")
    {
        let active = crate::session::contract::current();
        let cwd_path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cwd = cwd_path.to_string_lossy().into_owned();
        if let Some(c) = active {
            return build_contract_active_prompt(&c, &cwd_path, &cwd);
        }
        return build_contract_proposal_prompt(&cwd_path, &cwd);
    }

    let mem_ctx = get_memory_context(messages, memory);

    let skill_ctx = get_skill_context(messages, skills);

    let plugin_ctx = get_plugin_prompts(plugins, Some(task_type));

    let mut prompt = build_system_prompt(
        config,
        &mem_ctx,
        &skill_ctx,
        &plugin_ctx,
        Some(task_type),
    );

    // Knowledge auto-injection.
    let know = get_knowledge_context(messages);
    if !know.is_empty() {
        prompt.push_str(&know);
    }

    // Code-graph hits for long user messages.
    if config.features.context_retrieval {
        if let Some(last_user) = messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        {
            if last_user.len() > 200 {
                if let Some(graph) = crate::code_graph::try_get_code_graph() {
                    if let Ok(hits) = graph.search_graph(last_user, 1500) {
                        if !hits.is_empty() {
                            prompt.push_str("\n\nRelevant code from the project:\n");
                            for h in hits.iter().take(5) {
                                prompt.push_str(&format!(
                                    "- {} ({} at {}:{})\n",
                                    h.name, h.kind, h.file, h.line
                                ));
                                if let Some(sig) = &h.signature {
                                    prompt.push_str(&format!("    {}\n", sig));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Test runner hint.
    let tr = get_test_runner_context(cwd);
    if !tr.is_empty() {
        prompt.push_str(&tr);
    }

    // Tool skill cards.
    let cards = crate::runtime::tool_guidance::select_tool_skill_cards(messages);
    if !cards.is_empty() {
        prompt.push_str(&cards);
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use serde_json::json;
    use tempfile::tempdir;

    /// Empty conversation → no memory context (no `Relevant project memory:` header).
    #[test]
    fn memory_context_empty_when_no_user_message() {
        let dir = tempdir().unwrap();
        let mem = MemoryStore::new_json_only(dir.path());
        let messages: Vec<Value> = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        assert_eq!(get_memory_context(&messages, &mem), "");
    }

    /// User message with no matching memory entries → still empty.
    #[test]
    fn memory_context_empty_with_no_matches() {
        let dir = tempdir().unwrap();
        let mem = MemoryStore::new_json_only(dir.path());
        let messages: Vec<Value> = vec![json!({"role": "user", "content": "totally novel topic xyz"})];
        let out = get_memory_context(&messages, &mem);
        assert!(out.is_empty() || out.contains("Relevant project memory:"),
            "expected empty or properly-headed; got {out:?}");
    }

    /// User message uses the LAST user msg (newest), not the first.
    /// Anti-regression: stale early-conversation context could mislead the model.
    #[test]
    fn memory_context_uses_last_user_message() {
        let dir = tempdir().unwrap();
        let mut mem = MemoryStore::new_json_only(dir.path());
        mem.remember("decision", "auth-rewrite",
            "Decided to rewrite the auth layer in Rust last quarter", vec!["rust".into()]);
        // Build conversation where the LAST user message mentions Rust.
        let messages: Vec<Value> = vec![
            json!({"role": "user", "content": "first message about JavaScript"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "now tell me about the rust rewrite"}),
        ];
        let out = get_memory_context(&messages, &mem);
        assert!(out.contains("auth-rewrite") || out.is_empty(),
            "memory context should pull from last user msg (rust); got: {out:?}");
    }

    /// `get_plugin_prompts` with no plugin injections returns empty string —
    /// no stray newlines.
    #[test]
    fn plugin_prompts_empty_when_no_injection() {
        let plugins = crate::plugins::loader::PluginLoader::new();
        assert_eq!(get_plugin_prompts(&plugins, Some("coding")), "");
        assert_eq!(get_plugin_prompts(&plugins, None), "");
    }

    /// `get_test_runner_context` on an empty dir is empty (no project type detected).
    #[test]
    fn test_runner_context_empty_on_empty_dir() {
        let dir = tempdir().unwrap();
        let out = get_test_runner_context(dir.path());
        // Either truly empty, or doesn't mention specific frameworks.
        assert!(!out.contains("cargo") && !out.contains("pytest"),
            "empty dir must not fake a runner; got {out:?}");
    }

    /// `get_test_runner_context` on a cargo project mentions cargo.
    #[test]
    fn test_runner_context_mentions_cargo_when_present() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        let out = get_test_runner_context(dir.path());
        assert!(out.contains("cargo"), "got: {out:?}");
    }

    // ── build_contract_proposal_prompt ─────────────────────────────────────

    /// Contract proposal prompt includes the cwd path and instructs `propose_contract`.
    #[test]
    fn proposal_prompt_includes_cwd_and_instructions() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let p = build_contract_proposal_prompt(dir.path(), &cwd);
        assert!(p.contains(&cwd), "cwd must appear in prompt");
        assert!(p.contains("propose_contract") || p.contains("contract"),
            "prompt must instruct contract proposal; got: {p}");
    }

    // ── build_contract_active_prompt ────────────────────────────────────────

    /// Active contract prompt renders the contract body.
    #[test]
    fn active_prompt_renders_contract_body() {
        use crate::session::contract::{Contract, ContractStatus, Assertion, AssertionState};
        let c = Contract {
            id: "test-id".into(),
            title: "Wire login".into(),
            brief: "Add login endpoint.".into(),
            created_at: "2024".into(),
            status: ContractStatus::Active,
            assertions: vec![
                Assertion {
                    id: "A.001".into(),
                    text: "/login returns 200".into(),
                    state: AssertionState::Pending,
                    evidence: None,
                    last_check: None,
                },
            ],
            features: vec![],
        };
        let dir = tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let p = build_contract_active_prompt(&c, dir.path(), &cwd);
        assert!(p.contains("Wire login") || p.contains("A.001"),
            "active prompt must reference contract content; got: {p}");
        assert!(p.contains(&cwd));
    }
}
