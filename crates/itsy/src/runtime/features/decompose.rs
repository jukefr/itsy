//! Decompose a stuck task. When the agent has failed `N` attempts on the
//! same file, ask the compiled `decompose_task` prompt to pick a strategy
//! (split file, fix one error at a time, rewrite section, extract function).
//! When a distinct second-opinion model is configured, also ask it and
//! surface both perspectives if they disagree.

use serde_json::{json, Value};

use super::prompts::{call_prompt, strip_fences, truncate};

#[derive(Debug, Clone)]
pub struct DecomposeStrategy {
    pub strategy: String,
    pub reason: String,
    pub instruction: String,
}

pub async fn decompose_task(
    task: &str,
    errors: &str,
    file_context: &str,
    config: &crate::config::Config,
) -> Option<DecomposeStrategy> {
    let r = call_prompt(
        "decompose_task",
        json!({ "task": task, "errors": errors, "file_context": file_context }),
    )
    .await
    .ok()?;
    let cleaned = strip_fences(&r);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    let valid_strategies = ["split_file", "one_error_at_a_time", "rewrite_section", "extract_function"];
    let strategy = parsed
        .get("strategy")
        .and_then(|v| v.as_str())
        .filter(|s| valid_strategies.contains(s))
        .unwrap_or("rewrite_section")
        .to_string();
    let main_reason = truncate(parsed.get("reason").and_then(|v| v.as_str()).unwrap_or(""), 300);
    let main_instruction = truncate(parsed.get("instruction").and_then(|v| v.as_str()).unwrap_or(""), 600);

    // Second opinion: ask the second model for its diagnosis. If it disagrees on
    // strategy, surface both perspectives in the instruction so the model gets
    // a richer view of why it's stuck.
    let instruction = if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        main_instruction
    } else {
        let second_model = config.second_opinion.resolved_model(config).to_string();
        let second_url = config.second_opinion.resolved_endpoint(config).to_string();
        let task_s: String = task.chars().take(300).collect();
        let errors_s: String = errors.chars().take(500).collect();
        let ctx_s: String = file_context.chars().take(1000).collect();
        let prompt = format!(
            "A coding task has failed after multiple attempts. Suggest a decomposition strategy.\n\n\
             Task: {task_s}\nErrors: {errors_s}\nFile context: {ctx_s}\n\n\
             Return JSON: {{\"strategy\":\"split_file|one_error_at_a_time|rewrite_section|extract_function\",\
             \"reason\":\"<why>\",\"instruction\":\"<2-3 sentence instruction for the model>\"}}"
        );
        match crate::runtime::providers::openai_compat::chat_oneshot(&second_url, &second_model, &prompt, None, 120).await {
            Ok(raw) => {
                let v: Value = serde_json::from_str(&strip_fences(&raw)).unwrap_or(Value::Null);
                let second_instr = truncate(v.get("instruction").and_then(|v| v.as_str()).unwrap_or(""), 400);
                let second_strat = v.get("strategy").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !second_instr.is_empty() && second_strat != strategy {
                    format!("{main_instruction}\n[Second opinion — {second_strat}]: {second_instr}")
                } else if !second_instr.is_empty() {
                    format!("{main_instruction} {second_instr}")
                } else {
                    main_instruction
                }
            }
            Err(_) => main_instruction,
        }
    };

    Some(DecomposeStrategy { strategy, reason: main_reason, instruction })
}
