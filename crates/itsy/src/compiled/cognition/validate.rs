//! The JS version contains a
//! large library of generated schema checkers — most of which are unused by
//! the runtime. This Rust port exposes a single generic validator that uses
//! `serde_json::Value` and reports the first mismatch path.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub path: String,
    pub reason: String,
}

pub fn validate_type(value: &Value, expected: &str, path: &str) -> Result<(), ValidationError> {
    let ok = match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    };
    if !ok {
        return Err(ValidationError {
            path: path.to_string(),
            reason: format!("expected {expected}, got {value}"),
        });
    }
    Ok(())
}

pub fn require_fields(value: &Value, fields: &[&str], path: &str) -> Result<(), ValidationError> {
    let Some(obj) = value.as_object() else {
        return Err(ValidationError {
            path: path.to_string(),
            reason: "expected object".into(),
        });
    };
    for f in fields {
        if !obj.contains_key(*f) {
            return Err(ValidationError {
                path: format!("{path}.{f}"),
                reason: "missing required field".into(),
            });
        }
    }
    Ok(())
}
