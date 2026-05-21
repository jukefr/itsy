//! Decides how many "thinking tokens"
//! to give a model based on task complexity hints.

pub fn thinking_budget(task_type: &str, message_len: usize) -> u32 {
    let base = match task_type {
        "coding" | "backend" => 512,
        "editing" => 256,
        "debugging" => 768,
        "multi_step" => 1024,
        "explanation" => 384,
        _ => 256,
    };
    let bonus = if message_len > 400 { 256 } else { 0 };
    base + bonus
}
