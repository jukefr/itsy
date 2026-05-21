//! Picks a sampling temperature based
//! on task type and recent success/failure pattern.

pub fn adaptive_temperature(task_type: &str, recent_failures: u32) -> f64 {
    let base = match task_type {
        "coding" | "editing" | "backend" => 0.1,
        "search" | "explanation" => 0.2,
        _ => 0.15,
    };
    (base + (recent_failures as f64) * 0.05).min(0.7)
}
