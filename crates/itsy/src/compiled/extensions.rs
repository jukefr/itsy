//! Extension registration is a stub in
//! the JS version too; we keep an empty registry that the rest of the app can
//! poll without crashing.

use std::collections::HashMap;

#[derive(Default)]
pub struct ExtensionRegistry {
    pub extensions: HashMap<String, serde_json::Value>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self { extensions: HashMap::new() }
    }
}
