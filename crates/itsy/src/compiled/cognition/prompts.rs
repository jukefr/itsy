//! The JS file is a large
//! generated registry of named prompt templates. The Rust port keeps the same
//! lookup contract; templates are stored as `&'static str` literals.
//!
//! Only the templates the runtime actually addresses by name are retained.
//! Adding more is mechanical (paste the template text into the match arm).

pub fn render(template: &str, vars: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'{') {
            chars.next();
            let mut key = String::new();
            while let Some(&nc) = chars.peek() {
                if nc == '}' {
                    break;
                }
                key.push(nc);
                chars.next();
            }
            // consume }}
            if chars.peek() == Some(&'}') {
                chars.next();
            }
            if chars.peek() == Some(&'}') {
                chars.next();
            }
            let key_t = key.trim();
            let lookup = vars.pointer(&format!("/{}", key_t)).or_else(|| vars.get(key_t));
            if let Some(v) = lookup {
                match v {
                    serde_json::Value::String(s) => out.push_str(s),
                    other => out.push_str(&other.to_string()),
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn get_template(name: &str) -> Option<&'static str> {
    Some(match name {
        "classify_task" => include_str!("../../assets/prompts/classify_task.txt"),
        "summarize_file" => include_str!("../../assets/prompts/summarize_file.txt"),
        _ => return None,
    })
}
