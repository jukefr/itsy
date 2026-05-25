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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `tmpl_classify_task` interpolates the user message and lists the
    /// known category set. Anti-regression for category drift.
    #[test]
    fn classify_template_interpolates_message_and_lists_categories() {
        let p = tmpl_classify_task("fix the failing tests");
        assert!(p.contains("fix the failing tests"));
        // All 8 known categories must appear:
        for cat in ["coding", "editing", "search", "shell", "explanation",
                    "multi_step", "debugging", "backend"] {
            assert!(p.contains(cat), "category {cat} missing from prompt");
        }
        assert!(p.contains("ONLY the category name"),
            "must instruct the model to reply with only the category");
    }

    /// `tmpl_compress_history` includes the max_tokens cap and the history text.
    #[test]
    fn compress_template_includes_cap_and_history() {
        let p = tmpl_compress_history("user: hi\nassistant: hello", 2048);
        assert!(p.contains("2048"));
        assert!(p.contains("user: hi"));
        assert!(p.contains("assistant: hello"));
    }

    /// `tmpl_code_assist` omits the Context block when context is empty
    /// — anti-bloat regression so the prompt doesn't ship a useless "Context:" header.
    #[test]
    fn code_assist_template_omits_empty_context() {
        let p = tmpl_code_assist("write fizzbuzz", "", "default");
        assert!(p.contains("write fizzbuzz"));
        assert!(!p.contains("Context:\n\n"),
            "empty context must not produce an empty Context: block");
    }

    /// With non-empty context, the Context: block is included.
    #[test]
    fn code_assist_template_includes_context() {
        let p = tmpl_code_assist("task", "ctx data", "default");
        assert!(p.contains("Context:"));
        assert!(p.contains("ctx data"));
    }

    // ── ExtensionRegistry ─────────────────────────────────────────────────

    #[test]
    fn registry_register_and_get_round_trip() {
        let mut r = ExtensionRegistry::new();
        r.register("foo", json!({"key": "value"}));
        assert_eq!(r.get("foo"), Some(&json!({"key": "value"})));
        assert!(r.get("missing").is_none());
    }

    /// Re-registering replaces the previous value.
    #[test]
    fn registry_register_overwrites() {
        let mut r = ExtensionRegistry::new();
        r.register("foo", json!(1));
        r.register("foo", json!(2));
        assert_eq!(r.get("foo"), Some(&json!(2)));
    }
}
