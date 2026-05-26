//! Dual-model contract-assertion review. When a distinct second-opinion
//! model/endpoint is configured we use it to:
//!
//! - independently propose assertions for a brief
//!   ([`negotiate_assertions`]),
//! - cross-check each individual assertion against its evidence
//!   ([`verify_assertion_passed`]),
//! - do a final hand-off review before marking a contract complete
//!   ([`verify_contract_complete`]).
//!
//! Any network/model failure falls back to the safe default (accept the
//! main model's verdict) rather than blocking the user.

use serde_json::{json, Value};

use super::prompts::strip_fences;

// ─── Per-assertion verification ──────────────────────────────────────────────

/// Ask the second model whether a single assertion is genuinely passed.
/// Returns `None` (verified OK) or `Some(reason)` (disputed).
/// No-op when second opinion is not configured.
pub async fn verify_assertion_passed(
    assertion_text: &str,
    evidence: &str,
    command: Option<&str>,
    exit_code: Option<i64>,
    observation: Option<&str>,
    config: &crate::config::Config,
) -> Option<String> {
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        return None;
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();

    let mut ev_block = format!("Description: {evidence}");
    if let (Some(cmd), Some(ec), Some(obs)) = (command, exit_code, observation) {
        ev_block.push_str(&format!("\nCommand: {cmd}\nExit code: {ec}\nOutput: {obs}"));
    }

    let prompt = format!(
        "You are an independent verifier checking whether a software task assertion was correctly verified.\n\n\
         Assertion: {assertion_text}\n\nEvidence:\n{ev_block}\n\n\
         Is this assertion ACTUALLY passed based on the evidence?\n\
         - Return {{\"verified\":true}} if the evidence clearly and specifically confirms it.\n\
         - Return {{\"verified\":false,\"reason\":\"...\"}} if the evidence is insufficient, vague, or contradicts the assertion.\n\n\
         Return ONLY JSON."
    );

    let raw = crate::runtime::providers::openai_compat::chat_oneshot(&second_url, &second_model, &prompt, None, 120).await.ok()?;
    let parsed: Value = serde_json::from_str(&strip_fences(&raw)).ok()?;
    if parsed.get("verified").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    Some(
        parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("insufficient evidence")
            .to_string(),
    )
}

// ─── Final contract-complete review ──────────────────────────────────────────

/// Ask the second model whether ALL passed assertions together represent a complete solution.
/// Returns `None` (all good) or `Some(disputed_ids)`.
/// No-op when second opinion is not configured.
pub async fn verify_contract_complete(
    brief: &str,
    assertions: &[(String, String, String)], // (id, text, evidence)
    config: &crate::config::Config,
) -> Option<Vec<String>> {
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        return None;
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();

    let list = assertions
        .iter()
        .map(|(id, text, ev)| format!("[{id}] {text}\n       evidence: {ev}"))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are doing a final review before a software task is marked complete.\n\n\
         Task brief: {brief}\n\nAssertions marked passed:\n{list}\n\n\
         Are you confident ALL assertions are genuinely satisfied and the solution is complete?\n\
         - Return {{\"accept\":true}} if yes.\n\
         - Return {{\"accept\":false,\"disputed\":[\"A1\"],\"reason\":\"...\"}} if you doubt any.\n\n\
         Return ONLY JSON."
    );

    let raw = crate::runtime::providers::openai_compat::chat_oneshot(&second_url, &second_model, &prompt, None, 120).await.ok()?;
    let parsed: Value = serde_json::from_str(&strip_fences(&raw)).ok()?;
    if parsed.get("accept").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    let disputed = parsed
        .get("disputed")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
        .unwrap_or_default();
    if disputed.is_empty() { None } else { Some(disputed) }
}

// ─── Assertion negotiation (dual-model) ─────────────────────────────────────

/// Run dual-model negotiation on contract assertions.
///
/// If no distinct second-opinion model/endpoint is configured this is a no-op.
/// Otherwise:
/// 1. Second model independently proposes its own assertions for the brief.
/// 2. Both sets are merged into a combined candidate list.
/// 3. Each model reviews the candidate — if both accept, done.
/// 4. On any objection, the objecting model's revised list becomes the new
///    candidate and we loop. Max 3 rounds, then we return what we have.
///
/// Any network/model failure falls back to the main assertions unchanged.
pub async fn negotiate_assertions(
    brief: &str,
    title: &str,
    main_assertions: Vec<(String, String)>,
    config: &crate::config::Config,
) -> (Vec<(String, String)>, bool) {
    if config.second_opinion.model.is_none() && config.second_opinion.endpoint.is_none() {
        eprintln!("[negotiate] skipped: no second_opinion configured");
        return (main_assertions, false);
    }
    let second_model = config.second_opinion.resolved_model(config).to_string();
    let second_url = config.second_opinion.resolved_endpoint(config).to_string();
    let main_model = config.model.name.clone();
    let main_url = config.model.base_url.clone();
    eprintln!("[negotiate] start: main={main_model} second={second_model} url={second_url}");

    let second_assertions =
        match ask_for_assertions(brief, title, &second_model, &second_url).await {
            Some(a) if !a.is_empty() => {
                eprintln!("[negotiate] second model returned {} assertions", a.len());
                a
            }
            _ => {
                eprintln!("[negotiate] second model returned empty/None — falling back");
                return (main_assertions, false);
            }
        };

    let mut current = merge_assertions(main_assertions, second_assertions);

    for _ in 0..3 {
        let main_rev = review_assertions(brief, &current, &main_model, &main_url).await;
        let second_rev = review_assertions(brief, &current, &second_model, &second_url).await;

        let main_revised = match main_rev {
            AssertionReview::Revise(r) if !r.is_empty() => Some(r),
            _ => None,
        };
        let second_revised = match second_rev {
            AssertionReview::Revise(r) if !r.is_empty() => Some(r),
            _ => None,
        };

        match (main_revised, second_revised) {
            (None, None) => break,
            (Some(r), None) => current = r,
            (None, Some(r)) => current = r,
            (Some(a), Some(b)) => current = merge_assertions(a, b),
        }
    }

    current.truncate(24);
    (current, true)
}

enum AssertionReview {
    Accept,
    Revise(Vec<(String, String)>),
}

async fn ask_for_assertions(
    brief: &str,
    title: &str,
    model: &str,
    base_url: &str,
) -> Option<Vec<(String, String)>> {
    let prompt = format!(
        "You are reviewing a coding task. Generate a list of testable assertions \
(acceptance criteria) that can be verified by running commands or inspecting files. \
Each assertion must be specific and concrete.\n\n\
For every constraint in the brief, write an assertion that verifies the constraint \
directly — not a proxy. If the brief constrains the *content* of a modification (what \
something becomes, what range/set it must come from, what shape it must have), the \
assertion must check the modification itself, not just the existence or integrity of \
related files. A file-unchanged or compile-succeeded check does not verify a content \
constraint.\n\n\
Title: {title}\n\
Brief: {brief}\n\n\
Return ONLY a JSON array, no markdown or explanation:\n\
[{{\"id\":\"A1\",\"text\":\"<specific verifiable statement>\"}},...]"
    );
    let raw = match crate::runtime::providers::openai_compat::chat_oneshot(base_url, model, &prompt, None, 120).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[negotiate] chat_oneshot({model}) failed: {e}");
            return None;
        }
    };
    let parsed = parse_assertion_array(&raw);
    if parsed.is_none() {
        eprintln!("[negotiate] chat_oneshot returned but parse_assertion_array failed. \
                   Raw response (first 300 chars): {}",
                  raw.chars().take(300).collect::<String>());
    }
    parsed
}

async fn review_assertions(
    brief: &str,
    assertions: &[(String, String)],
    model: &str,
    base_url: &str,
) -> AssertionReview {
    let assertions_json = serde_json::to_string(
        &assertions
            .iter()
            .map(|(id, text)| json!({"id": id, "text": text}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    let prompt = format!(
        "Review these contract assertions for a coding task. \
If they fully and correctly cover what needs to be done, return {{\"accept\":true}}. \
If any are missing, wrong, or too broad to verify, return the full revised list as \
{{\"accept\":false,\"revised\":[{{\"id\":\"A1\",\"text\":\"...\"}},...]}}\n\
Break broad assertions into specific verifiable ones. Remove duplicates.\n\n\
For every constraint in the brief, check that an assertion verifies the constraint \
directly. A proxy check (file unchanged, compile succeeded, output exists) is not \
enough when the brief restricts the *content* of a modification — that requires an \
assertion that examines the modification itself. If a constraint is unverified, add \
or rewrite an assertion to cover it.\n\n\
Brief: {brief}\n\nAssertions:\n{assertions_json}\n\nReturn ONLY JSON."
    );

    let raw = match crate::runtime::providers::openai_compat::chat_oneshot(base_url, model, &prompt, None, 120).await {
        Ok(s) => s,
        Err(_) => return AssertionReview::Accept,
    };

    let cleaned = strip_fences(&raw);
    let parsed: Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(_) => return AssertionReview::Accept,
    };

    if parsed.get("accept").and_then(|v| v.as_bool()).unwrap_or(true) {
        return AssertionReview::Accept;
    }

    let revised = parsed
        .get("revised")
        .and_then(|v| v.as_array())
        .map(|arr| parse_assertion_array_from_value(arr))
        .unwrap_or_default();

    if revised.is_empty() {
        AssertionReview::Accept
    } else {
        AssertionReview::Revise(revised)
    }
}

fn merge_assertions(
    a: Vec<(String, String)>,
    b: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_texts = std::collections::HashSet::new();
    let mut result: Vec<(String, String)> = Vec::new();
    for (id, text) in a.into_iter().chain(b) {
        // Semantic dedup: if another assertion already says the same thing
        // (after lowercasing + stripping punctuation + collapsing whitespace),
        // drop this one. Two-model negotiation routinely produces verbatim or
        // near-verbatim duplicates ("pdflatex compiles successfully" appears
        // in both sets); without this we ended up with 12 assertions where 4
        // were exact duplicates and 4 more were semantic overlaps.
        let key = normalize_for_dedup(&text);
        if !key.is_empty() && !seen_texts.insert(key) {
            continue;
        }
        if seen_ids.contains(&id) {
            let mut n = 2u32;
            let mut new_id = format!("{id}_{n}");
            while seen_ids.contains(&new_id) {
                n += 1;
                new_id = format!("{id}_{n}");
            }
            seen_ids.insert(new_id.clone());
            result.push((new_id, text));
        } else {
            seen_ids.insert(id.clone());
            result.push((id, text));
        }
    }
    result
}

/// Normalize an assertion text for deduplication: lowercase, drop non-
/// alphanumeric characters, collapse runs of whitespace. Catches verbatim
/// duplicates and most punctuation-only / phrasing-trivial variations.
fn normalize_for_dedup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            prev_space = false;
        } else if c.is_whitespace() || c.is_ascii_punctuation() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        }
        // Non-ASCII non-alphanumeric symbols (emoji, etc.) are dropped silently.
    }
    out.trim().to_string()
}

fn parse_assertion_array(raw: &str) -> Option<Vec<(String, String)>> {
    let cleaned = strip_fences(raw);
    let parsed: Value = serde_json::from_str(&cleaned).ok()?;
    let arr = parsed.as_array()?;
    let items = parse_assertion_array_from_value(arr);
    if items.is_empty() { None } else { Some(items) }
}

fn parse_assertion_array_from_value(arr: &[Value]) -> Vec<(String, String)> {
    arr.iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim().to_string();
            let text = item.get("text")?.as_str()?.trim().to_string();
            if id.is_empty() || text.len() < 5 {
                return None;
            }
            Some((id, text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── merge_assertions ────────────────────────────────────────────────────

    #[test]
    fn merge_assertions_concatenates_unique_ids() {
        let a = vec![("A.001".into(), "first".into())];
        let b = vec![("A.002".into(), "second".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, "A.001");
        assert_eq!(merged[1].0, "A.002");
    }

    /// Conflicting IDs are renumbered (`A.001` → `A.001_2`) — anti-regression
    /// for silent data loss when two assertion sets share IDs.
    #[test]
    fn merge_assertions_renames_collisions() {
        let a = vec![("A.001".into(), "from a".into())];
        let b = vec![("A.001".into(), "from b".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2, "no assertion may be silently dropped");
        assert_eq!(merged[0].0, "A.001");
        assert_eq!(merged[1].0, "A.001_2",
            "duplicate ID must be renumbered, not collapsed");
        assert_eq!(merged[0].1, "from a");
        assert_eq!(merged[1].1, "from b");
    }

    /// Multiple collisions of the same id keep increasing the suffix.
    #[test]
    fn merge_assertions_handles_triple_collision() {
        let a = vec![("X".into(), "a".into()), ("X".into(), "b".into())];
        let b = vec![("X".into(), "c".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 3);
        let ids: Vec<&str> = merged.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["X", "X_2", "X_3"]);
    }

    /// Empty inputs handled gracefully.
    #[test]
    fn merge_assertions_empty_inputs() {
        assert!(merge_assertions(vec![], vec![]).is_empty());
        let a = vec![("A.001".into(), "x".into())];
        assert_eq!(merge_assertions(a.clone(), vec![]), a);
        assert_eq!(merge_assertions(vec![], a.clone()), a);
    }

    /// Semantic dedup: identical text under different ids collapses to one.
    /// Anti-regression for the case where two-model negotiation produced 12
    /// assertions because every Qwen assertion appeared again verbatim from
    /// Gemma under a renumbered id (`A1` + `A1_2`).
    #[test]
    fn merge_assertions_dedupes_identical_text() {
        let a = vec![
            ("A1".into(), "pdflatex compiles main.tex successfully".into()),
            ("A2".into(), "no overfull hbox warnings".into()),
        ];
        let b = vec![
            ("B1".into(), "pdflatex compiles main.tex successfully".into()),
            ("B2".into(), "synonyms.txt is unchanged".into()),
        ];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 3, "duplicate text must collapse");
        let texts: Vec<&str> = merged.iter().map(|(_, t)| t.as_str()).collect();
        assert!(texts.contains(&"pdflatex compiles main.tex successfully"));
        assert!(texts.contains(&"no overfull hbox warnings"));
        assert!(texts.contains(&"synonyms.txt is unchanged"));
    }

    /// Punctuation / case differences should also be treated as duplicates.
    #[test]
    fn merge_assertions_dedupes_phrasing_variants() {
        let a = vec![("A1".into(), "pdflatex compiles main.tex successfully (exit code 0)".into())];
        let b = vec![("B1".into(), "pdflatex   compiles main.tex successfully, exit code 0.".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 1,
            "casing/punctuation/whitespace differences must not bypass dedup");
    }

    /// Different texts with the same id still both ship (with renumbering).
    #[test]
    fn merge_assertions_preserves_distinct_texts_under_same_id() {
        let a = vec![("A.001".into(), "first thing".into())];
        let b = vec![("A.001".into(), "totally different thing".into())];
        let merged = merge_assertions(a, b);
        assert_eq!(merged.len(), 2, "different text must not be deduped");
    }

    // ── normalize_for_dedup ────────────────────────────────────────────────

    #[test]
    fn normalize_strips_case_and_punctuation() {
        assert_eq!(normalize_for_dedup("Hello, World!"), "hello world");
        assert_eq!(normalize_for_dedup("  HELLO   world  "), "hello world");
        assert_eq!(normalize_for_dedup("foo.bar:baz"), "foo bar baz");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_for_dedup("a\n\tb  c"), "a b c");
    }

    #[test]
    fn normalize_empty_is_empty() {
        assert_eq!(normalize_for_dedup(""), "");
        assert_eq!(normalize_for_dedup("...  ,, !!"), "");
    }

    // ── parse_assertion_array ──────────────────────────────────────────────

    #[test]
    fn parse_assertions_valid_array() {
        let raw = r#"[{"id":"A.001","text":"hello world"},{"id":"A.002","text":"goodbye"}]"#;
        let r = parse_assertion_array(raw).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("A.001".into(), "hello world".into()));
        assert_eq!(r[1], ("A.002".into(), "goodbye".into()));
    }

    #[test]
    fn parse_assertions_strips_fences_first() {
        let raw = "```json\n[{\"id\":\"A.001\",\"text\":\"hello world\"}]\n```";
        let r = parse_assertion_array(raw).unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn parse_assertions_drops_invalid_items() {
        let raw = r#"[
            {"id":"A.001","text":"valid one"},
            {"id":"","text":"empty id"},
            {"text":"missing id"},
            {"id":"A.002","text":"x"}
        ]"#;
        let r = parse_assertion_array(raw).unwrap();
        assert!(r.iter().any(|(id, _)| id == "A.001"));
        assert!(!r.iter().any(|(id, _)| id.is_empty()));
        assert!(!r.iter().any(|(id, _)| id == "A.002"),
            "text shorter than 5 chars must be dropped; got {r:?}");
    }

    #[test]
    fn parse_assertions_rejects_non_array() {
        assert!(parse_assertion_array("not json").is_none());
        assert!(parse_assertion_array("{\"obj\":true}").is_none());
        assert!(parse_assertion_array("[]").is_none(), "empty array → None");
    }
}
