//! Per-model capability profile.

use once_cell::sync::Lazy;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Profile {
    pub context_length: u32,
    pub max_output: u32,
    pub supports_tool_calling: bool,
    pub tool_format: &'static str,
    pub strengths: &'static [&'static str],
    pub weaknesses: &'static [&'static str],
}

#[derive(Debug, Clone)]
pub struct EffectiveProfile {
    pub context_length: u32,
    pub max_output: u32,
    pub supports_tool_calling: bool,
    pub tool_format: &'static str,
    pub strengths: Vec<&'static str>,
    pub weaknesses: Vec<&'static str>,
    pub matched_key: Option<&'static str>,
}

pub static KNOWN_PROFILES: Lazy<HashMap<&'static str, Profile>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("gemma-4", Profile { context_length: 32768, max_output: 8192, supports_tool_calling: true, tool_format: "native", strengths: &["code_completion","instruction_following","tool_use"], weaknesses: &["very_long_planning"] });
    m.insert("gemma-4-e4b", Profile { context_length: 32768, max_output: 8192, supports_tool_calling: true, tool_format: "native", strengths: &["speed","code_completion","tool_use"], weaknesses: &["complex_reasoning","multi_file"] });
    m.insert("qwen3", Profile { context_length: 32768, max_output: 8192, supports_tool_calling: true, tool_format: "hermes", strengths: &["reasoning","code_generation","planning"], weaknesses: &["verbosity"] });
    m.insert("qwen2.5-coder", Profile { context_length: 32768, max_output: 8192, supports_tool_calling: true, tool_format: "hermes", strengths: &["code_completion","refactoring"], weaknesses: &["long_planning","multi_file"] });
    m.insert("deepseek-coder", Profile { context_length: 16384, max_output: 4096, supports_tool_calling: true, tool_format: "json", strengths: &["code_completion","debugging"], weaknesses: &["instruction_following","tool_use_reliability"] });
    m.insert("codellama", Profile { context_length: 16384, max_output: 4096, supports_tool_calling: false, tool_format: "text", strengths: &["code_completion"], weaknesses: &["tool_use","instruction_following","planning"] });
    m.insert("llama-3", Profile { context_length: 8192, max_output: 4096, supports_tool_calling: true, tool_format: "native", strengths: &["general_reasoning"], weaknesses: &["code_specific"] });
    m.insert("mistral-nemo", Profile { context_length: 128000, max_output: 4096, supports_tool_calling: true, tool_format: "native", strengths: &["long_context","instruction_following"], weaknesses: &["code_specific"] });
    m.insert("starcoder", Profile { context_length: 8192, max_output: 4096, supports_tool_calling: false, tool_format: "text", strengths: &["code_completion","infilling"], weaknesses: &["instruction_following","tool_use","planning"] });
    m
});

fn match_profile(model_name: &str) -> Option<(&'static str, &'static Profile)> {
    let name = model_name.to_lowercase();
    let mut keys: Vec<&&str> = KNOWN_PROFILES.keys().collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()));
    for k in keys {
        if name.contains(*k) {
            return Some((*k, &KNOWN_PROFILES[*k]));
        }
    }
    None
}

pub fn get_profile(model_name: &str, detected_context_window: u32) -> EffectiveProfile {
    let matched = match_profile(model_name);
    EffectiveProfile {
        context_length: if detected_context_window > 0 {
            detected_context_window
        } else {
            matched.map(|(_, p)| p.context_length).unwrap_or(32768)
        },
        max_output: matched.map(|(_, p)| p.max_output).unwrap_or(4096),
        supports_tool_calling: matched.map(|(_, p)| p.supports_tool_calling).unwrap_or(true),
        tool_format: matched.map(|(_, p)| p.tool_format).unwrap_or("native"),
        strengths: matched.map(|(_, p)| p.strengths.to_vec()).unwrap_or_default(),
        weaknesses: matched.map(|(_, p)| p.weaknesses.to_vec()).unwrap_or_default(),
        matched_key: matched.map(|(k, _)| k),
    }
}
