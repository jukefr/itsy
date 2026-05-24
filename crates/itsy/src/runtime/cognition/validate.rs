//! Cognition Layer Phase 5 — output validation modes (port of
//! upstream JS `cognition/validate.js`).
//!
//! The upstream JS exposes three validation modes (`schema_only`,
//! `ast_compiles`, `custom:<ext>`). The Rust port keeps the
//! `schema_only` shape-check — that's the mode actually exercised by the
//! itsy runtime today (see `prompts.rs::dispatch`). The
//! TypeScript-compiler-driven `ast_compiles` mode has no Rust analogue
//! and is intentionally omitted (it would require shelling out to
//! `tsc`); a `validate_ast_compiles` stub is provided so callers can
//! still wire the path through without surprises.
//!
//! Public API summary:
//!   * [`ValidationReport`]    — `{ ok, issues }` mirror of the JS shape.
//!   * [`ValidationError`]     — legacy path/reason error used by the old stub.
//!   * [`validate_schema_only`]— main entry point used by the dispatcher.
//!   * [`validate_type`]       — convenience wrapper returning `ValidationError`.
//!   * [`require_fields`]      — assert presence of named object keys.
//!   * [`validate_custom`]     — invoke a user-supplied closure validator.
//!   * [`validate_ast_compiles`] — stub; always returns `ok = true` with a note.

use serde_json::Value;

/// Rich, multi-issue validation report. Mirrors the JS
/// `ValidationReport = { ok: boolean; issues: string[] }` shape that the
/// `on_invalid` repair logic consumes.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub ok: bool,
    pub issues: Vec<String>,
}

impl ValidationReport {
    pub fn ok() -> Self {
        Self { ok: true, issues: Vec::new() }
    }

    pub fn failed<S: Into<String>>(issue: S) -> Self {
        Self { ok: false, issues: vec![issue.into()] }
    }
}

/// Legacy single-issue error returned by [`validate_type`] /
/// [`require_fields`]. Pre-dates [`ValidationReport`]; kept so existing
/// callers (currently `prompts.rs::dispatch`) don't have to change.
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub path: String,
    pub reason: String,
}

// ─── Public schema validator ────────────────────────────────────────────────

/// Validate a parsed model output against the prompt's declared return
/// type. `expected` is the IR-serialised type expression:
///   * primitives: `"string"`, `"int"`, `"uint"`, `"float"`, `"bool"`, `"json"`
///   * the catch-all `"unknown"` — accepts anything
///   * `"file"` / `"File"`         — `{ path, content, kind? }` record
///   * `"files"` / `list<File>`    — non-empty array of File records
///   * `enum<["a","b",...]>`       — value must be one of the literals
///   * `list<T>`                   — array; each item type-checked as `T`
///   * anything else               — accepted (no-op for unknown IR types)
pub fn validate_schema_only(value: &Value, expected: &str) -> ValidationReport {
    let mut issues: Vec<String> = Vec::new();

    // "unknown" — JS `default: return null;` catch-all that the dispatch
    // file lists explicitly. Accept anything.
    if expected == "unknown" {
        return ValidationReport::ok();
    }

    // File: { path, content, kind? }
    if expected == "File" || expected == "file" {
        file_shape_issues(value, &mut issues, "");
        return finalize(issues);
    }

    // list<File>  /  "files"
    if expected == "files" || is_list_of_file(expected) {
        let Some(arr) = value.as_array() else {
            issues.push(format!("expected list<File>, got {}", type_name(value)));
            return finalize(issues);
        };
        if arr.is_empty() {
            issues.push("expected at least one File, got empty array".into());
            return finalize(issues);
        }
        let mut seen: Vec<String> = Vec::new();
        for (i, item) in arr.iter().enumerate() {
            let prefix = format!("file[{i}]: ");
            file_shape_issues(item, &mut issues, &prefix);
            if let Some(p) = item.get("path").and_then(|p| p.as_str()) {
                if seen.iter().any(|s| s == p) {
                    issues.push(format!("file[{i}]: duplicate path \"{p}\""));
                }
                seen.push(p.to_string());
            }
        }
        return finalize(issues);
    }

    // Enum literal set.
    if let Some(allowed) = parse_enum_decl(expected) {
        match value.as_str() {
            None => issues.push(format!(
                "expected one of [{}], got {}",
                allowed.join(", "),
                type_name(value)
            )),
            Some(s) if !allowed.iter().any(|a| a == s) => {
                let preview: String = s.chars().take(64).collect();
                issues.push(format!(
                    "value \"{preview}\" is not in [{}]",
                    allowed.join(", ")
                ));
            }
            _ => {}
        }
        return finalize(issues);
    }

    // list<T>
    if let Some(inner) = parse_list_inner(expected) {
        match value.as_array() {
            None => issues.push(format!("expected list<{inner}>, got {}", type_name(value))),
            Some(arr) => {
                for (i, item) in arr.iter().enumerate() {
                    if let Some(reason) = declared_primitive_check(&inner, item) {
                        issues.push(format!("item[{i}]: {reason}"));
                    }
                }
            }
        }
        return finalize(issues);
    }

    // Primitive / json catch-all.
    if let Some(reason) = declared_primitive_check(expected, value) {
        issues.push(reason);
    }
    finalize(issues)
}

// ─── Legacy single-error helpers (kept for back-compat) ─────────────────────

/// Single-issue type check. Returns `Err(ValidationError)` on the first
/// failure. Thin wrapper around [`validate_schema_only`] so the dispatcher
/// and other legacy callers can keep their `.is_err()` style.
pub fn validate_type(value: &Value, expected: &str, path: &str) -> Result<(), ValidationError> {
    // Cheap aliases for the basic JSON-Schema-ish primitives the old
    // stub recognised; map them onto the rich validator.
    let mapped = match expected {
        "number" => {
            if value.is_number() {
                return Ok(());
            }
            return Err(ValidationError {
                path: path.to_string(),
                reason: format!("expected number, got {}", type_name(value)),
            });
        }
        "boolean" => {
            if value.is_boolean() {
                return Ok(());
            }
            return Err(ValidationError {
                path: path.to_string(),
                reason: format!("expected boolean, got {}", type_name(value)),
            });
        }
        "object" => {
            if value.is_object() {
                return Ok(());
            }
            return Err(ValidationError {
                path: path.to_string(),
                reason: format!("expected object, got {}", type_name(value)),
            });
        }
        "array" => {
            if value.is_array() {
                return Ok(());
            }
            return Err(ValidationError {
                path: path.to_string(),
                reason: format!("expected array, got {}", type_name(value)),
            });
        }
        "null" => {
            if value.is_null() {
                return Ok(());
            }
            return Err(ValidationError {
                path: path.to_string(),
                reason: format!("expected null, got {}", type_name(value)),
            });
        }
        other => other,
    };

    let report = validate_schema_only(value, mapped);
    if report.ok {
        Ok(())
    } else {
        Err(ValidationError {
            path: path.to_string(),
            reason: report.issues.join("; "),
        })
    }
}

/// Assert that an object has each of `fields`. Returns the offending path
/// on the first miss.
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

// ─── Custom + AST modes ─────────────────────────────────────────────────────

/// Delegate to a user-supplied validator closure. The closure may return
/// a fully-formed [`ValidationReport`] (rich) or `Ok(())` / `Err(msg)`
/// for the simple case. Panics inside the closure are NOT caught — that
/// would require `std::panic::catch_unwind` and a `RefUnwindSafe` bound
/// we don't want to force on users. Closures that can fail should return
/// `Err` instead of panicking.
pub fn validate_custom<F>(value: &Value, validator: F) -> ValidationReport
where
    F: FnOnce(&Value) -> Result<ValidationReport, String>,
{
    match validator(value) {
        Ok(report) => report,
        Err(msg) => {
            let trimmed: String = msg.chars().take(200).collect();
            ValidationReport::failed(format!("custom validator threw: {trimmed}"))
        }
    }
}

/// Stub for the JS `validateAstCompiles` mode. The Rust port doesn't ship
/// a TypeScript compiler; this returns `ok = true` with a single
/// `"skipped"` note so callers can still wire the mode through without
/// erroring. If real TS validation is needed, shell out to `tsc` from
/// the caller and feed the result to [`validate_custom`].
pub fn validate_ast_compiles(_value: &Value) -> ValidationReport {
    ValidationReport {
        ok: true,
        issues: vec!["typescript validation not available in Rust port — ast_compiles skipped".into()],
    }
}

// ─── Internals ──────────────────────────────────────────────────────────────

/// Check a single value against a primitive type name. Returns
/// `Some(reason)` on mismatch, `None` on pass.
fn declared_primitive_check(declared: &str, value: &Value) -> Option<String> {
    match declared {
        "string" => match value.as_str() {
            None => Some(format!("expected string, got {}", type_name(value))),
            Some("") => Some("empty string".into()),
            _ => None,
        },
        "int" => {
            if !value.is_number() {
                return Some(format!("expected number, got {}", type_name(value)));
            }
            if let Some(f) = value.as_f64() {
                if !f.is_finite() {
                    return Some(format!("expected number, got {}", type_name(value)));
                }
                if f.fract() != 0.0 {
                    return Some("expected int, got non-integer".into());
                }
            }
            None
        }
        "uint" => {
            if !value.is_number() {
                return Some(format!("expected number, got {}", type_name(value)));
            }
            if let Some(f) = value.as_f64() {
                if !f.is_finite() {
                    return Some(format!("expected number, got {}", type_name(value)));
                }
                if f < 0.0 {
                    return Some("expected uint, got negative".into());
                }
            }
            None
        }
        "float" => {
            if !value.is_number() {
                return Some(format!("expected number, got {}", type_name(value)));
            }
            if let Some(f) = value.as_f64() {
                if !f.is_finite() {
                    return Some(format!("expected number, got {}", type_name(value)));
                }
            }
            None
        }
        "bool" => {
            if value.is_boolean() {
                None
            } else {
                Some(format!("expected bool, got {}", type_name(value)))
            }
        }
        // "json" accepts any JSON value (matches the JS behaviour).
        "json" => None,
        // Unknown declared type — JS falls through to a no-op accept so
        // users can declare richer return shapes via custom validators.
        _ => None,
    }
}

/// Push issues for a value that should be a File record. Empty prefix is
/// fine for the singular File path; pass `"file[N]: "` for list items.
fn file_shape_issues(value: &Value, issues: &mut Vec<String>, prefix: &str) {
    let Some(obj) = value.as_object() else {
        issues.push(format!("{prefix}expected File record, got {}", type_name(value)));
        return;
    };

    match obj.get("path").and_then(|p| p.as_str()) {
        None => issues.push(format!("{prefix}File.path must be a non-empty string")),
        Some("") => issues.push(format!("{prefix}File.path must be a non-empty string")),
        Some(p) => {
            // Reject path-traversal and absolute paths so the artifact
            // can be safely written to a sandbox dir. Mirrors the JS
            // regex `..` / leading `/` / Windows drive prefix.
            let win_drive = p.len() >= 2
                && p.as_bytes()[1] == b':'
                && p.as_bytes()[0].is_ascii_alphabetic();
            if p.contains("..") || p.starts_with('/') || win_drive {
                issues.push(format!(
                    "{prefix}File.path must be a relative path without \"..\" segments"
                ));
            }
        }
    }

    match obj.get("content") {
        Some(Value::String(s)) if s.is_empty() => {
            issues.push(format!("{prefix}File.content is empty"));
        }
        Some(Value::String(_)) => {}
        Some(_) => issues.push(format!("{prefix}File.content must be a string")),
        None => issues.push(format!("{prefix}File.content must be a string")),
    }

    if let Some(kind) = obj.get("kind") {
        if !kind.is_string() && !kind.is_null() {
            issues.push(format!("{prefix}File.kind must be a string when present"));
        }
    }
}

/// Strip `enum<["a","b"]>` and return the literal set. Returns `None`
/// when the input isn't an enum declaration. Items are JSON-quoted
/// strings; we parse them with `serde_json` and fall back to a manual
/// quote-strip if that fails.
fn parse_enum_decl(declared: &str) -> Option<Vec<String>> {
    let inner = declared.strip_prefix("enum<[")?.strip_suffix("]>")?;

    let mut items: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => depth -= 1,
            ',' if depth == 0 => {
                items.push(buf.trim().to_string());
                buf.clear();
                continue;
            }
            _ => {}
        }
        buf.push(ch);
    }
    if !buf.trim().is_empty() {
        items.push(buf.trim().to_string());
    }

    Some(
        items
            .into_iter()
            .map(|raw| match serde_json::from_str::<String>(&raw) {
                Ok(s) => s,
                Err(_) => raw
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            })
            .collect(),
    )
}

/// Match `list<T>` and return `T`. Returns `None` if it isn't a list
/// type. Whitespace around `T` is trimmed.
fn parse_list_inner(declared: &str) -> Option<String> {
    let inner = declared.strip_prefix("list<")?.strip_suffix('>')?;
    Some(inner.trim().to_string())
}

/// Specifically detect `list<File>` (with optional whitespace) — checked
/// before the generic `list<T>` so the dedicated File-list validator
/// runs.
fn is_list_of_file(declared: &str) -> bool {
    parse_list_inner(declared).map(|t| t == "File").unwrap_or(false)
}

/// Wraps a final `(ok, issues)` snapshot. Kept as a tiny helper so the
/// per-branch returns above read like the JS.
fn finalize(issues: Vec<String>) -> ValidationReport {
    ValidationReport {
        ok: issues.is_empty(),
        issues,
    }
}

/// Mirror of `typeof` for the JSON value kinds the JS validator
/// surfaces in its error messages.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_accepts_anything() {
        assert!(validate_schema_only(&json!(null), "unknown").ok);
        assert!(validate_schema_only(&json!({"x": 1}), "unknown").ok);
    }

    #[test]
    fn string_rejects_empty() {
        assert!(validate_schema_only(&json!("hi"), "string").ok);
        let r = validate_schema_only(&json!(""), "string");
        assert!(!r.ok && r.issues[0].contains("empty"));
    }

    #[test]
    fn int_uint_float() {
        assert!(validate_schema_only(&json!(3), "int").ok);
        assert!(!validate_schema_only(&json!(3.5), "int").ok);
        assert!(!validate_schema_only(&json!(-1), "uint").ok);
        assert!(validate_schema_only(&json!(1.5), "float").ok);
        assert!(!validate_schema_only(&json!("nope"), "int").ok);
    }

    #[test]
    fn json_catch_all() {
        assert!(validate_schema_only(&json!({"a": [1, 2]}), "json").ok);
        assert!(validate_schema_only(&json!(null), "json").ok);
    }

    #[test]
    fn file_shape() {
        let good = json!({"path": "src/a.ts", "content": "hi"});
        assert!(validate_schema_only(&good, "File").ok);

        let bad_abs = json!({"path": "/etc/passwd", "content": "x"});
        assert!(!validate_schema_only(&bad_abs, "File").ok);

        let bad_traverse = json!({"path": "../x", "content": "x"});
        assert!(!validate_schema_only(&bad_traverse, "File").ok);

        let bad_empty_content = json!({"path": "a.ts", "content": ""});
        assert!(!validate_schema_only(&bad_empty_content, "File").ok);
    }

    #[test]
    fn file_list() {
        let good = json!([{"path": "a.ts", "content": "x"}, {"path": "b.ts", "content": "y"}]);
        assert!(validate_schema_only(&good, "list<File>").ok);

        let dup = json!([{"path": "a.ts", "content": "x"}, {"path": "a.ts", "content": "y"}]);
        let r = validate_schema_only(&dup, "list<File>");
        assert!(!r.ok && r.issues.iter().any(|i| i.contains("duplicate")));

        assert!(!validate_schema_only(&json!([]), "files").ok);
    }

    #[test]
    fn enum_literal() {
        let r = validate_schema_only(&json!("a"), r#"enum<["a","b"]>"#);
        assert!(r.ok);
        let r = validate_schema_only(&json!("c"), r#"enum<["a","b"]>"#);
        assert!(!r.ok);
    }

    #[test]
    fn list_of_string() {
        assert!(validate_schema_only(&json!(["a", "b"]), "list<string>").ok);
        assert!(!validate_schema_only(&json!(["a", ""]), "list<string>").ok);
        assert!(!validate_schema_only(&json!("nope"), "list<string>").ok);
    }

    #[test]
    fn legacy_validate_type() {
        assert!(validate_type(&json!("hi"), "string", "p").is_ok());
        assert!(validate_type(&json!(""), "string", "p").is_err());
        assert!(validate_type(&json!(3), "number", "p").is_ok());
        assert!(validate_type(&json!(true), "boolean", "p").is_ok());
        assert!(validate_type(&json!({}), "object", "p").is_ok());
        assert!(validate_type(&json!([]), "array", "p").is_ok());
    }

    #[test]
    fn legacy_require_fields() {
        let v = json!({"a": 1, "b": 2});
        assert!(require_fields(&v, &["a", "b"], "p").is_ok());
        assert!(require_fields(&v, &["c"], "p").is_err());
        assert!(require_fields(&json!("x"), &["a"], "p").is_err());
    }

    #[test]
    fn custom_validator() {
        let ok = validate_custom(&json!(1), |_| Ok(ValidationReport::ok()));
        assert!(ok.ok);
        let bad = validate_custom(&json!(1), |_| Err("boom".into()));
        assert!(!bad.ok && bad.issues[0].contains("boom"));
    }

    #[test]
    fn ast_compiles_is_stub() {
        let r = validate_ast_compiles(&json!("anything"));
        assert!(r.ok && r.issues[0].contains("skipped"));
    }
}
