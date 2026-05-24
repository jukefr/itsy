//! Image attachment support.
//!
//! Detects image references in user messages (typically `@path/to.png`) and
//! encodes them for multimodal models. Usage:
//!
//! ```text
//! look at @screenshot.png and fix the layout
//! ```
//!
//! The image is loaded, base64-encoded, and emitted as an OpenAI-style
//! `image_url` content part.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::security::{safe_resolve_path, PathOptions};

pub const IMAGE_EXTENSIONS: &[&str] =
    &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg"];

/// 8 MB cap per image to prevent base64 context blow-up.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// One decoded image reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: String,
    pub mime: String,
    pub base64: String,
    pub size: usize,
}

/// Same regex as the JS version — match `@path` references that don't follow
/// a word character or backtick. We then filter by extension below.
static FILE_REGEX: Lazy<Regex> = Lazy::new(|| {
    // (?<![\w`]) is not supported by `regex`. Emulate by capturing the
    // preceding character if any and skipping matches where it's a word
    // character or backtick.
    Regex::new(r"(?:^|[^\w`])@(\.?[^\s`,]*(?:\.[^\s`,]+)+)").expect("valid regex literal")
});

/// Extract images from a message. Out-of-tree and sensitive paths are
/// silently dropped (see [`safe_resolve_path`]).
pub fn extract_images(message: &str, cwd: &Path) -> Vec<ImageRef> {
    let mut out = Vec::new();
    for cap in FILE_REGEX.captures_iter(message) {
        let raw = match cap.get(1) {
            Some(m) => m.as_str().trim_end_matches(['.', ',', ';']),
            None => continue,
        };
        if raw.is_empty() {
            continue;
        }
        let ext = match extension_of(raw) {
            Some(e) => e,
            None => continue,
        };
        if !IMAGE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
            continue;
        }
        let resolved = match safe_resolve_path(raw, cwd, PathOptions::default()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let meta = match fs::metadata(&resolved.full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
            continue;
        }
        let bytes = match fs::read(&resolved.full_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        out.push(ImageRef {
            path: raw.to_string(),
            mime: guess_mime(&ext).to_string(),
            base64: b64,
            size: bytes.len(),
        });
    }
    out
}

/// Format a slice of images as OpenAI-vision-style content parts.
pub fn format_images_for_api(images: &[ImageRef]) -> Vec<Value> {
    images
        .iter()
        .map(|i| {
            json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", i.mime, i.base64)
                }
            })
        })
        .collect()
}

/// Heuristic: does the model likely support vision input?
pub fn model_supports_vision(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("vision")
        || m.contains("gemma-4")
        || m.contains("gpt-4")
        || m.contains("gpt-5")
        || m.contains("claude")
        || m.contains("qwen")
        || m.contains("llava")
        || m.contains("pixtral")
}

/// Detect if an input is just a (possibly quoted) file path dragged onto
/// the terminal. Returns the absolute resolved path on success.
pub fn detect_dropped_file(input: &str) -> Option<PathBuf> {
    let trimmed = input.trim();
    // Strip a single pair of surrounding quotes.
    let stripped = if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if stripped.is_empty() {
        return None;
    }
    let ext = extension_of(stripped)?;
    if !IMAGE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
        return None;
    }
    let looks_like_path = stripped.contains('/')
        || stripped.contains('\\')
        || stripped.starts_with('.')
        || stripped
            .chars()
            .nth(1)
            .map(|c| c == ':')
            .unwrap_or(false);
    if !looks_like_path {
        return None;
    }
    let resolved = match fs::canonicalize(stripped) {
        Ok(p) => p,
        Err(_) => return None,
    };
    if resolved.exists() { Some(resolved) } else { None }
}

fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
}

fn guess_mime(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".bmp" => "image/bmp",
        ".svg" => "image/svg+xml",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn vision_heuristic() {
        assert!(model_supports_vision("gpt-4o"));
        assert!(model_supports_vision("claude-3-opus"));
        assert!(model_supports_vision("qwen2-vl"));
        assert!(!model_supports_vision("tinyllama"));
    }

    #[test]
    fn extracts_at_reference() {
        let dir = tempdir().unwrap();
        let img = dir.path().join("pic.png");
        let mut f = fs::File::create(&img).unwrap();
        f.write_all(&[0x89, 0x50, 0x4e, 0x47]).unwrap();
        let msg = "look at @pic.png please";
        let imgs = extract_images(msg, dir.path());
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
    }

    #[test]
    fn skips_non_image_extension() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("notes.txt");
        fs::write(&f, b"hi").unwrap();
        let imgs = extract_images("see @notes.txt", dir.path());
        assert!(imgs.is_empty());
    }
}
