//! Picks which model name to send a turn to
//! based on task type and configured `models.fast`/`default`/`strong` triad.

use crate::Config;

pub fn route_model(config: &Config, task_type: &str) -> String {
    let Some(models) = &config.models else {
        return config.model.name.clone();
    };
    match task_type {
        "explanation" | "search" => models.fast.clone(),
        "backend" | "multi_step" | "debugging" => models.strong.clone(),
        _ => models.default.clone(),
    }
}
