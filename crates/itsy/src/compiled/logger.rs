//! Stderr logging gated by ITSY_DEBUG.

use std::env;

pub fn debug(area: &str, msg: &str) {
    if env::var("ITSY_DEBUG").is_ok() {
        eprintln!("[{area}] {msg}");
    }
}

pub fn warn(area: &str, msg: &str) {
    eprintln!("[{area}] warn: {msg}");
}
