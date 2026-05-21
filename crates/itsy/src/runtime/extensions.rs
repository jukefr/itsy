//! Extension points: implementations of the compiler-declared `tmpl_*`
//! helpers that the cognition layer dispatches to.
//!
//! These mirror the JS `extensions.js` extension function bodies. Higher
//! layers (`runtime::cognition::prompts`) already expose `tmpl_classify_task`
//! et al as free functions; the helpers here expose the same bodies as
//! public functions for callers that want to use the registry shape.

use std::collections::HashMap;

use serde_json::Value;

/// Extension point: `tmpl_classify_task`.
///
/// Returns a prompt that asks the model to classify the user's task into one
/// of itsy's known categories.
pub fn tmpl_classify_task(user_message: &str) -> String {
    format!(
        "Classify this user message into ONE of these categories. Reply with ONLY the category name, nothing else.\n\n\
        Categories:\n\
        - coding: creating new code/files\n\
        - editing: modifying existing files\n\
        - search: finding files or symbols\n\
        - shell: running commands\n\
        - explanation: answering questions, explaining concepts\n\
        - multi_step: tasks with multiple sequential parts\n\
        - debugging: fixing errors or bugs\n\
        - backend: building backend services / APIs\n\n\
        User message: \"{user_message}\"\n\n\
        Category:"
    )
}

/// Extension point: `tmpl_compress_history`.
///
/// Returns a prompt that asks the model to compress conversation history
/// into a concise summary, capped at `max_tokens` tokens.
pub fn tmpl_compress_history(history: &str, max_tokens: u64) -> String {
    format!(
        "Summarize this conversation history into a concise summary of facts, decisions, and current state. Maximum {max_tokens} tokens. Keep file paths, error messages, and code identifiers exact.\n\n\
        History:\n{history}\n\n\
        Concise summary:"
    )
}

/// Extension point: `tmpl_code_assist`.
///
/// Returns a prompt for the main code-assistance loop. `complexity` matches
/// the router's chosen tier (`fast` / `default` / `strong`), kept on the
/// signature for parity even when the body doesn't currently inline it.
pub fn tmpl_code_assist(task: &str, context: &str, _complexity: &str) -> String {
    let ctx_block = if context.is_empty() {
        String::new()
    } else {
        format!("Context:\n{context}\n\n")
    };
    format!(
        "You are itsy, a coding agent. Use tools to read, write, edit files and run commands. Be concise.\n\n\
        {ctx_block}Task: {task}"
    )
}

// ---------------------------------------------------------------------------
// Extension registry — kept as a runtime-pluggable hook surface.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ExtensionRegistry {
    pub extensions: HashMap<String, Value>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self {
            extensions: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, value: Value) {
        self.extensions.insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.extensions.get(name)
    }
}
