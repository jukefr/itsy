//! Per-model capability profile.
//!
//! Profiles drive routing, context budgeting, tool format selection, and
//! escalation decisions. Built-in entries cover the local-LLM families we
//! ship support for (Gemma, Qwen, DeepSeek, Llama, Mistral, StarCoder,
//! Phi, GLM, Yi). Users can override or extend via `profiles/*.toml` on
//! disk, and pick an explicit profile by name via the `ITSY_PROFILE` env
//! var.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// Static, compiled-in capability profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub context_length: u32,
    pub max_output: u32,
    pub supports_tool_calling: bool,
    pub tool_format: &'static str,
    pub strengths: &'static [&'static str],
    pub weaknesses: &'static [&'static str],
    // Quantitative capability flags. 0.0–1.0 scale.
    pub tool_use_quality: f32,
    pub instruction_following_score: f32,
    pub code_quality: f32,
}

/// Effective profile (built-in + on-disk overrides + detection).
#[derive(Debug, Clone)]
pub struct EffectiveProfile {
    pub context_length: u32,
    pub max_output: u32,
    pub supports_tool_calling: bool,
    pub tool_format: &'static str,
    pub strengths: Vec<&'static str>,
    pub weaknesses: Vec<&'static str>,
    pub tool_use_quality: f32,
    pub instruction_following_score: f32,
    pub code_quality: f32,
    pub matched_key: Option<&'static str>,
}

/// On-disk profile (loaded from `profiles/<name>.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskProfile {
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub max_output: Option<u32>,
    #[serde(default)]
    pub supports_tool_calling: Option<bool>,
    #[serde(default)]
    pub tool_format: Option<String>,
    #[serde(default)]
    pub strengths: Vec<String>,
    #[serde(default)]
    pub weaknesses: Vec<String>,
    #[serde(default)]
    pub tool_use_quality: Option<f32>,
    #[serde(default)]
    pub instruction_following_score: Option<f32>,
    #[serde(default)]
    pub code_quality: Option<f32>,
}

macro_rules! prof {
    (
        ctx: $ctx:expr, out: $out:expr, tools: $tools:expr, fmt: $fmt:expr,
        s: $s:expr, w: $w:expr,
        tq: $tq:expr, if_: $if_:expr, cq: $cq:expr $(,)?
    ) => {
        Profile {
            context_length: $ctx,
            max_output: $out,
            supports_tool_calling: $tools,
            tool_format: $fmt,
            strengths: $s,
            weaknesses: $w,
            tool_use_quality: $tq,
            instruction_following_score: $if_,
            code_quality: $cq,
        }
    };
}

pub static KNOWN_PROFILES: Lazy<HashMap<&'static str, Profile>> = Lazy::new(|| {
    let mut m: HashMap<&'static str, Profile> = HashMap::new();

    // ── Gemma ────────────────────────────────────────────────────────
    m.insert("gemma-4", prof! {
        ctx: 32768, out: 8192, tools: true, fmt: "native",
        s: &["code_completion","instruction_following","tool_use"],
        w: &["very_long_planning"],
        tq: 0.78, if_: 0.82, cq: 0.74,
    });
    m.insert("gemma-4-e4b", prof! {
        ctx: 32768, out: 8192, tools: true, fmt: "native",
        s: &["speed","code_completion","tool_use"],
        w: &["complex_reasoning","multi_file"],
        tq: 0.72, if_: 0.74, cq: 0.68,
    });

    // ── Qwen ─────────────────────────────────────────────────────────
    m.insert("qwen3", prof! {
        ctx: 32768, out: 8192, tools: true, fmt: "hermes",
        s: &["reasoning","code_generation","planning"],
        w: &["verbosity"],
        tq: 0.83, if_: 0.84, cq: 0.82,
    });
    m.insert("qwen2.5-coder", prof! {
        ctx: 32768, out: 8192, tools: true, fmt: "hermes",
        s: &["code_completion","refactoring"],
        w: &["long_planning","multi_file"],
        tq: 0.74, if_: 0.76, cq: 0.86,
    });
    m.insert("qwen2.5", prof! {
        ctx: 32768, out: 8192, tools: true, fmt: "hermes",
        s: &["general_reasoning","instruction_following"],
        w: &["code_specific"],
        tq: 0.76, if_: 0.80, cq: 0.72,
    });

    // ── DeepSeek ─────────────────────────────────────────────────────
    m.insert("deepseek-coder", prof! {
        ctx: 16384, out: 4096, tools: true, fmt: "json",
        s: &["code_completion","debugging"],
        w: &["instruction_following","tool_use_reliability"],
        tq: 0.62, if_: 0.66, cq: 0.85,
    });
    m.insert("deepseek-r1", prof! {
        ctx: 65536, out: 8192, tools: false, fmt: "text",
        s: &["reasoning","math","planning"],
        w: &["tool_use","verbosity"],
        tq: 0.30, if_: 0.78, cq: 0.80,
    });

    // ── CodeLlama / Llama ────────────────────────────────────────────
    m.insert("codellama", prof! {
        ctx: 16384, out: 4096, tools: false, fmt: "text",
        s: &["code_completion"],
        w: &["tool_use","instruction_following","planning"],
        tq: 0.30, if_: 0.55, cq: 0.72,
    });
    m.insert("llama-3", prof! {
        ctx: 8192, out: 4096, tools: true, fmt: "native",
        s: &["general_reasoning"],
        w: &["code_specific"],
        tq: 0.68, if_: 0.74, cq: 0.62,
    });
    m.insert("llama-3.1", prof! {
        ctx: 128000, out: 4096, tools: true, fmt: "native",
        s: &["long_context","general_reasoning"],
        w: &["code_specific"],
        tq: 0.72, if_: 0.78, cq: 0.66,
    });

    // ── Mistral / Nemo ───────────────────────────────────────────────
    m.insert("mistral-nemo", prof! {
        ctx: 128000, out: 4096, tools: true, fmt: "native",
        s: &["long_context","instruction_following"],
        w: &["code_specific"],
        tq: 0.74, if_: 0.80, cq: 0.66,
    });
    m.insert("mistral", prof! {
        ctx: 32768, out: 4096, tools: true, fmt: "native",
        s: &["general_reasoning"],
        w: &["code_specific"],
        tq: 0.66, if_: 0.72, cq: 0.62,
    });

    // ── StarCoder ────────────────────────────────────────────────────
    m.insert("starcoder", prof! {
        ctx: 8192, out: 4096, tools: false, fmt: "text",
        s: &["code_completion","infilling"],
        w: &["instruction_following","tool_use","planning"],
        tq: 0.20, if_: 0.45, cq: 0.74,
    });

    // ── Phi ──────────────────────────────────────────────────────────
    m.insert("phi-3", prof! {
        ctx: 128000, out: 4096, tools: true, fmt: "native",
        s: &["speed","instruction_following"],
        w: &["complex_reasoning","long_planning"],
        tq: 0.64, if_: 0.78, cq: 0.60,
    });
    m.insert("phi-4", prof! {
        ctx: 16384, out: 8192, tools: true, fmt: "native",
        s: &["reasoning","instruction_following"],
        w: &["long_context"],
        tq: 0.74, if_: 0.82, cq: 0.70,
    });

    // ── GLM / Yi ─────────────────────────────────────────────────────
    m.insert("glm-4", prof! {
        ctx: 128000, out: 4096, tools: true, fmt: "native",
        s: &["long_context","reasoning"],
        w: &["code_specific"],
        tq: 0.68, if_: 0.74, cq: 0.66,
    });
    m.insert("yi-coder", prof! {
        ctx: 131072, out: 4096, tools: true, fmt: "hermes",
        s: &["long_context","code_generation"],
        w: &["instruction_following"],
        tq: 0.66, if_: 0.68, cq: 0.78,
    });

    m
});

/// Match a model name to a known profile using longest-prefix substring match.
fn match_profile(model_name: &str) -> Option<(&'static str, &'static Profile)> {
    let name = model_name.to_lowercase();
    let mut keys: Vec<&&'static str> = KNOWN_PROFILES.keys().collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()));
    for k in keys {
        if name.contains(*k) {
            return Some((*k, &KNOWN_PROFILES[*k]));
        }
    }
    None
}

/// Apply a disk override onto an effective profile in-place.
fn apply_disk_override(eff: &mut EffectiveProfile, disk: &DiskProfile) {
    if let Some(v) = disk.context_length {
        eff.context_length = v;
    }
    if let Some(v) = disk.max_output {
        eff.max_output = v;
    }
    if let Some(v) = disk.supports_tool_calling {
        eff.supports_tool_calling = v;
    }
    if let Some(v) = &disk.tool_format {
        // Disk values must outlive the program — leak short strings to &'static.
        eff.tool_format = Box::leak(v.clone().into_boxed_str());
    }
    if let Some(v) = disk.tool_use_quality {
        eff.tool_use_quality = v;
    }
    if let Some(v) = disk.instruction_following_score {
        eff.instruction_following_score = v;
    }
    if let Some(v) = disk.code_quality {
        eff.code_quality = v;
    }
    if !disk.strengths.is_empty() {
        eff.strengths = disk.strengths.iter().map(|s| &*Box::leak(s.clone().into_boxed_str())).collect();
    }
    if !disk.weaknesses.is_empty() {
        eff.weaknesses = disk.weaknesses.iter().map(|s| &*Box::leak(s.clone().into_boxed_str())).collect();
    }
}

/// Search candidate directories for `profiles/<name>.toml`. Returns the
/// first match's parsed [`DiskProfile`].
pub fn load_profile(name: &str) -> Option<DiskProfile> {
    // Defensive: forbid path-traversal in the user-supplied name.
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return None;
    }
    let file_name = format!("{name}.toml");
    let candidates: Vec<PathBuf> = candidate_profile_dirs()
        .into_iter()
        .map(|d| d.join(&file_name))
        .collect();
    for path in candidates {
        if !path.exists() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = toml::from_str::<DiskProfile>(&content) {
                return Some(parsed);
            }
        }
    }
    None
}

fn candidate_profile_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("profiles"));
        out.push(cwd.join(".itsy").join("profiles"));
    }
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".config").join("itsy").join("profiles"));
    }
    out
}

/// Get the effective profile for a model.
///
/// Resolution order (highest priority first):
///   1. `ITSY_PROFILE` env var — pin to a specific profile name.
///   2. Disk override at `profiles/<matched_key>.toml`.
///   3. Built-in [`KNOWN_PROFILES`] match.
///   4. Defaults (32k ctx, 4k out, native tool format).
///
/// `detected_context_window` (from endpoint auto-detection) supersedes the
/// profile's `context_length` when non-zero.
pub fn get_profile(model_name: &str, detected_context_window: u32) -> EffectiveProfile {
    let matched = match_profile(model_name);

    let mut eff = EffectiveProfile {
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
        tool_use_quality: matched.map(|(_, p)| p.tool_use_quality).unwrap_or(0.6),
        instruction_following_score: matched.map(|(_, p)| p.instruction_following_score).unwrap_or(0.65),
        code_quality: matched.map(|(_, p)| p.code_quality).unwrap_or(0.6),
        matched_key: matched.map(|(k, _)| k),
    };

    // Disk override under the matched key.
    if let Some((key, _)) = matched {
        if let Some(disk) = load_profile(key) {
            apply_disk_override(&mut eff, &disk);
        }
    }

    // Env override last — `ITSY_PROFILE` pins to a specific on-disk file.
    if let Ok(env_name) = std::env::var("ITSY_PROFILE") {
        let env_name = env_name.trim();
        if !env_name.is_empty() {
            if let Some(disk) = load_profile(env_name) {
                apply_disk_override(&mut eff, &disk);
                // Make the env-chosen key visible in matched_key for downstream.
                eff.matched_key = Some(Box::leak(env_name.to_string().into_boxed_str()));
            }
        }
    }

    eff
}

#[allow(dead_code)]
pub fn profile_path_for(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.toml");
    for dir in candidate_profile_dirs() {
        let p: &Path = &dir;
        let candidate = p.join(&file);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
