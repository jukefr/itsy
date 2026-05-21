//! JSON-output repair heuristics:
//! when the model returns invalid JSON, try to coerce it into valid JSON.

use serde_json::Value;

/// Attempt to repair common JSON serialisation errors emitted by small models.
/// Returns the original input unchanged if no repair is applicable.
pub fn repair_json(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return input.to_string();
    }
    // Strip ```json fences
    let mut s = trimmed.to_string();
    if let Some(rest) = s.strip_prefix("```json") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("```") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_suffix("```") {
        s = rest.to_string();
    }
    let s = s.trim();
    // Replace single-quoted keys with double-quoted (common small-model bug)
    let s = replace_unquoted_keys(s);
    // Try parse — if ok, re-serialise to a canonical form
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        return serde_json::to_string(&v).unwrap_or(s);
    }
    s
}

fn replace_unquoted_keys(input: &str) -> String {
    let re = regex::Regex::new(r#"([\{,]\s*)([A-Za-z_][A-Za-z0-9_]*)\s*:"#).unwrap();
    re.replace_all(input, "$1\"$2\":").into_owned()
}
