//! Generate a conventional-commit subject line from a task description and
//! the set of changed files. Falls back to a `chore:`-prefixed truncation of
//! the task when the model fails or returns a non-conventional shape.

use serde_json::json;

use super::prompts::call_prompt;

pub async fn generate_commit_message(task: &str, changed_files: &[String]) -> String {
    let fallback = format!(
        "itsy: {}",
        task.chars()
            .take(50)
            .collect::<String>()
            .replace(['\n', '\r', '"', '\'', '`', '$', '\\'], " ")
            .trim()
    );
    let files_joined = changed_files.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
    let r = match call_prompt(
        "commit_message",
        json!({ "task": task, "changed_files": files_joined }),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return fallback,
    };
    let cv_re = regex::Regex::new(r"^(feat|fix|docs|refactor|test|chore|style|ci|perf|build|revert)(\(.+\))?:").expect("valid regex literal");
    let trimmed = r
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim_end_matches('.')
        .chars()
        .take(72)
        .collect::<String>();
    if cv_re.is_match(&trimmed) {
        trimmed
    } else {
        let short: String = trimmed.chars().take(65).collect();
        format!("chore: {short}")
    }
}
