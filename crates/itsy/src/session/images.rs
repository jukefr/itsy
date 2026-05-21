//! Extracts image references from user
//! messages and formats them for OpenAI-vision-style API calls.

use std::fs;
use std::path::Path;

use base64::Engine;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub path: String,
    pub mime: String,
    pub base64: String,
}

pub fn extract_images(message: &str, cwd: &Path) -> Vec<ImageRef> {
    let re = regex::Regex::new(r"@?([\w./-]+\.(?:png|jpg|jpeg|gif|webp))").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(message) {
        let p = &cap[1];
        let full = cwd.join(p);
        if let Ok(bytes) = fs::read(&full) {
            let mime = guess_mime(p);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            out.push(ImageRef {
                path: p.to_string(),
                mime: mime.into(),
                base64: b64,
            });
        }
    }
    out
}

fn guess_mime(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

pub fn model_supports_vision(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("gpt-4o")
        || m.contains("gpt-5")
        || m.contains("claude")
        || m.contains("vision")
        || m.contains("llava")
        || m.contains("qwen2-vl")
}

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
