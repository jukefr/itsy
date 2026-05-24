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
