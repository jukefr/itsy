//! Detects ambiguous user requests and
//! proposes a clarifying question.

pub fn needs_clarification(message: &str) -> Option<String> {
    let lc = message.to_lowercase();
    if lc.contains("the file") && !lc.contains("@") {
        return Some("Which file? Paste a path or use @path/to/file.".into());
    }
    if lc.contains("fix it") && message.len() < 30 {
        return Some("Fix what? Describe the error or paste it.".into());
    }
    None
}
