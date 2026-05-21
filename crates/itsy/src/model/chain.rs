//! Chained-call helper: routes a single user
//! turn through a small chain of models (classifier → executor → reviewer).

use crate::model::profiles::EffectiveProfile;

pub struct ChainStep {
    pub model: String,
    pub purpose: &'static str,
}

pub fn pick_chain(profile: &EffectiveProfile, task_type: &str) -> Vec<ChainStep> {
    let model = profile.matched_key.unwrap_or("(default)").to_string();
    match task_type {
        "multi_step" | "backend" => vec![
            ChainStep { model: model.clone(), purpose: "plan" },
            ChainStep { model: model.clone(), purpose: "execute" },
            ChainStep { model, purpose: "review" },
        ],
        _ => vec![ChainStep { model, purpose: "execute" }],
    }
}
