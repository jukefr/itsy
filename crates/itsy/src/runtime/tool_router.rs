//! Pure, deterministic tool category
//! classifier — no LLM, no network, no randomness.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

struct Signal {
    re: Regex,
    w: f64,
}

struct Category {
    weight: f64,
    min_confidence: f64,
    signals: Vec<Signal>,
}

fn r(s: &str) -> Regex {
    Regex::new(&format!("(?i){s}")).expect("valid regex literal")
}

static CATEGORIES: Lazy<Vec<(&'static str, Category)>> = Lazy::new(|| {
    vec![
        ("read", Category {
            weight: 1.0,
            min_confidence: 0.4,
            signals: vec![
                Signal { re: r(r"\b(read|show|cat|display|print|view|open|look\s+at|check|see|inspect)\b"), w: 3.0 },
                Signal { re: r(r"\b(what'?s\s+in|what\s+is\s+in|contents?\s+of|what\s+files|list\s+files|files\s+in)\b"), w: 3.0 },
                Signal { re: r(r"\b(review|analyze|analyse|examine|audit|look\s+over|go\s+over)\b"), w: 3.0 },
                Signal { re: r(r"\b(file|\.\w{1,4})\b"), w: 1.5 },
                Signal { re: r(r"\b(fix|change|update|modify|add|remove|delete|create|write)\b"), w: -2.0 },
                Signal { re: r(r"\b(run|execute|test|build|install)\b"), w: -1.5 },
            ],
        }),
        ("write", Category {
            weight: 1.0,
            min_confidence: 0.3,
            signals: vec![
                Signal { re: r(r"\b(fix|change|update|modify|edit|refactor|rename|replace|patch)\b"), w: 3.0 },
                Signal { re: r(r"\b(add|insert|append|prepend|implement|create|write|make)\b"), w: 2.5 },
                Signal { re: r(r"\b(remove|delete|strip|clean\s*up|drop)\b"), w: 2.0 },
                Signal { re: r(r"\b(bug|error|typo|issue|broken|wrong|incorrect|failing|fail|crash)\b"), w: 2.0 },
                Signal { re: r(r"\b(file|function|class|method|variable|import|export)\b"), w: 1.0 },
                Signal { re: r(r"\b(explain|what|why|how\s+does|tell\s+me)\b"), w: -1.5 },
                Signal { re: r(r"\b(search|find|grep|look\s+for)\b"), w: -1.5 },
            ],
        }),
        ("search", Category {
            weight: 1.0,
            min_confidence: 0.4,
            signals: vec![
                Signal { re: r(r"\b(find|search|grep|look\s+for|locate|where\s+is|where\s+are)\b"), w: 3.0 },
                Signal { re: r(r"\b(all\s+uses?\s+of|all\s+references?|who\s+calls?|who\s+uses?|imports?\s+of)\b"), w: 3.0 },
                Signal { re: r(r"\b(pattern|regex|match|occurrences?)\b"), w: 2.0 },
                Signal { re: r(r"\b(across|everywhere|all\s+files|codebase|project)\b"), w: 1.5 },
                Signal { re: r(r"\b(fix|change|update|create|write)\b"), w: -2.0 },
            ],
        }),
        ("run", Category {
            weight: 1.0,
            min_confidence: 0.15,
            signals: vec![
                Signal { re: r(r"\b(run|execute|start|launch|invoke)\b"), w: 3.0 },
                Signal { re: r(r"\b(tests?|specs?|jest|pytest|mocha|vitest)\b"), w: 3.0 },
                Signal { re: r(r"\b(build|compile|make|bundle|webpack|tsc|cargo)\b"), w: 2.5 },
                Signal { re: r(r"\b(install|npm|pip|yarn|pnpm|apt|brew)\b"), w: 2.5 },
                Signal { re: r(r"\b(lint|format|prettier|eslint|black|ruff)\b"), w: 2.0 },
                Signal { re: r(r"\b(git|commit|push|pull|merge|branch|checkout|diff|status)\b"), w: 2.0 },
                Signal { re: r(r"\b(deploy|docker|k8s|kubernetes|terraform)\b"), w: 2.0 },
                Signal { re: r(r"\b(explain|what|why|how\s+does)\b"), w: -1.5 },
            ],
        }),
        ("plan", Category {
            weight: 1.0,
            min_confidence: 0.5,
            signals: vec![
                Signal { re: r(r"\b(implement|build|create|design|architect)\b"), w: 2.0 },
                Signal { re: r(r"\b(full|complete|entire|whole|end.to.end|e2e)\b"), w: 2.5 },
                Signal { re: r(r"\b(system|module|feature|service|api|app|application)\b"), w: 1.5 },
                Signal { re: r(r"\b(step\s+by\s+step|plan|break\s+down|decompose|how\s+should\s+i)\b"), w: 3.0 },
                Signal { re: r(r"\b(multiple\s+files|several\s+files|across\s+files|refactor\s+all)\b"), w: 2.0 },
                Signal { re: r(r"\b(show|read|display|cat)\b"), w: -2.0 },
            ],
        }),
        ("code_intel", Category {
            weight: 1.0,
            min_confidence: 0.4,
            signals: vec![
                Signal { re: r(r"\b(how\s+does\s+\w+\s+work|how\s+is\s+\w+\s+implemented)\b"), w: 3.5 },
                Signal { re: r(r"\b(what\s+calls?|who\s+calls?|callers?\s+of)\b"), w: 3.5 },
                Signal { re: r(r"\b(inheritance|extends|subclass|parent\s+class|class\s+hierarchy)\b"), w: 3.0 },
                Signal { re: r(r"\b(what\s+does\s+\w+\s+call|dependencies\s+of|call\s+graph|call\s+chain)\b"), w: 3.0 },
                Signal { re: r(r"\b(explain\s+(symbol|function|class|method)|where\s+is\s+\w+\s+defined)\b"), w: 2.5 },
                Signal { re: r(r"\b(trace|flow|data\s+flow|control\s+flow)\b"), w: 2.0 },
                Signal { re: r(r"\b(fix|change|update|create|write|delete)\b"), w: -2.0 },
                Signal { re: r(r"\b(run|execute|test|build)\b"), w: -1.5 },
            ],
        }),
        ("web", Category {
            weight: 1.0,
            min_confidence: 0.5,
            signals: vec![
                Signal { re: r(r"\b(search\s+the\s+web|google|look\s+up\s+online|internet)\b"), w: 3.0 },
                Signal { re: r(r"\b(latest\s+version|current\s+version|newest|recent)\b"), w: 2.0 },
                Signal { re: r(r"\b(documentation|docs|api\s+reference|npm\s+page|pypi)\b"), w: 2.0 },
                Signal { re: r(r"\b(url|website|link|https?://)\b"), w: 2.5 },
                Signal { re: r(r"\b(download|fetch\s+from|get\s+from)\b"), w: 1.5 },
            ],
        }),
        ("respond", Category {
            weight: 0.8,
            min_confidence: 0.3,
            signals: vec![
                Signal { re: r(r"\b(explain|what\s+is|what\s+are|what\s+does|how\s+does|how\s+do|tell\s+me|describe)\b"), w: 3.0 },
                Signal { re: r(r"\b(why\s+is|why\s+does|why\s+do|why\s+did|why\s+doesn't|why\s+won't)\b"), w: 1.5 },
                Signal { re: r(r"\b(difference\s+between|compare|vs|versus)\b"), w: 2.5 },
                Signal { re: r(r"\b(help|guide|tutorial|example|show\s+me\s+how)\b"), w: 2.0 },
                Signal { re: r(r"\b(opinion|think|recommend|suggest|best\s+practice)\b"), w: 2.0 },
                Signal { re: r(r"\b(thanks|thank\s+you|ok|sure|yes|no|got\s+it)\b"), w: 3.0 },
                // Conversational openers — "say hello", "greet", "respond with"
                // shouldn't trigger file tools just because the message is long.
                Signal { re: r(r"\b(say|greet|respond\s+with|reply\s+with|just\s+say)\b"), w: 4.0 },
                // Strong "no side effects" override.
                Signal { re: r(r"\b(do\s+nothing|nothing\s+else|don'?t\s+do|don'?t\s+touch|do\s+not\s+touch|just\s+chat)\b"), w: 5.0 },
                // Hi / hello / greet — common chat openers.
                Signal { re: r(r"^\s*(hi|hello|hey|yo|sup|gm|good\s+(morning|afternoon|evening|night))\b"), w: 4.0 },
                Signal { re: r(r"\b(failing|failed|broken|crash|error|bug|wrong)\b"), w: -2.0 },
                Signal { re: r(r"\b(review|check|look\s+at|analyze|analyse|read|show|examine|audit)\b"), w: -3.0 },
                Signal { re: r(r"\b(file|code|function|class|module|script|demo|mode)\b"), w: -1.5 },
            ],
        }),
    ]
});

pub const PRIORITY: &[&str] = &["write", "run", "code_intel", "search", "plan", "read", "web", "respond"];

const SHORT_MSG_THRESHOLD: usize = 10;
const LONG_MSG_THRESHOLD: usize = 200;
const FALLBACK: &str = "read";
const SHORT_MSG_DEFAULT: &str = "respond";

#[derive(Debug, Clone)]
pub struct Classification {
    pub category: String,
    pub confidence: f64,
    pub scores: HashMap<String, f64>,
}

fn score_category(message: &str, category: &Category) -> f64 {
    let mut score = 0.0;
    for sig in &category.signals {
        if sig.re.is_match(message) {
            score += sig.w;
        }
    }
    score * category.weight
}

pub fn classify_tool_category(message: &str) -> Classification {
    if message.is_empty() {
        return Classification { category: FALLBACK.into(), confidence: 0.0, scores: HashMap::new() };
    }
    let trimmed = message.trim();
    let action_re = Regex::new(r"(?i)\b(run|fix|read|show|find|build|test|git|npm|pip|go|cd|ls|rm|mv|cp)\b").expect("valid regex literal");
    if trimmed.len() <= SHORT_MSG_THRESHOLD && !action_re.is_match(trimmed) {
        let mut scores = HashMap::new();
        scores.insert("respond".to_string(), 1.0);
        return Classification { category: SHORT_MSG_DEFAULT.into(), confidence: 1.0, scores };
    }

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut max_score = f64::NEG_INFINITY;
    let mut max_category = FALLBACK.to_string();

    for (name, cat) in CATEGORIES.iter() {
        let mut s = score_category(trimmed, cat);
        if *name == "plan" && trimmed.len() > LONG_MSG_THRESHOLD {
            s += 2.0;
        }
        scores.insert((*name).to_string(), s);
        if s > max_score {
            max_score = s;
            max_category = (*name).to_string();
        } else if (s - max_score).abs() < f64::EPSILON {
            let cur_idx = PRIORITY.iter().position(|p| p == name).unwrap_or(usize::MAX);
            let max_idx = PRIORITY.iter().position(|p| *p == max_category.as_str()).unwrap_or(usize::MAX);
            if cur_idx < max_idx {
                max_category = (*name).to_string();
            }
        }
    }

    let mut sorted: Vec<(&String, &f64)> = scores.iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top = *sorted[0].1;
    let second = sorted.get(1).map(|(_, v)| **v).unwrap_or(0.0);
    let margin = top - second.max(0.0);
    let confidence = if top > 0.0 {
        (margin / top.max(3.0)).min(1.0)
    } else {
        0.0
    };

    if top <= 0.0 {
        return Classification { category: FALLBACK.into(), confidence: 0.0, scores };
    }

    if confidence < 0.1 && sorted.len() > 1 {
        let tied: Vec<String> = sorted.iter().filter(|(_, s)| **s >= top - 0.5).map(|(c, _)| (*c).clone()).collect();
        for p in PRIORITY {
            if tied.iter().any(|c| c == p) {
                return Classification { category: (*p).into(), confidence, scores };
            }
        }
    }

    Classification { category: max_category, confidence, scores }
}

pub fn get_tools_for_category(category: &str) -> Vec<&'static str> {
    match category {
        "code_intel" => vec!["graph_search", "explain_symbol", "read_file", "read_original", "find_files", "search"],
        "read" => vec!["read_file", "read_original", "list_projects", "graph_search", "find_files", "find_and_read"],
        "write" => vec![
            "read_file", "read_original", "write_file", "patch", "bash", "read_and_patch", "create_and_run",
            "propose_contract", "mark_assertion", "mark_feature", "contract_status", "close_contract",
        ],
        "search" => vec!["search", "find_files", "graph_search", "read_file", "read_original", "explain_symbol", "search_and_read"],
        "run" => vec![
            "bash", "run", "read_file", "read_original",
            "propose_contract", "mark_assertion", "mark_feature", "contract_status", "close_contract",
        ],
        "plan" => vec![
            "read_file", "read_original", "write_file", "patch", "bash", "search", "find_files",
            "graph_search", "memory_load", "memory_remember",
            "read_and_patch", "create_and_run", "find_and_read", "search_and_read",
            "propose_contract", "mark_assertion", "mark_feature", "contract_status", "close_contract",
        ],
        "web" => vec!["web_search", "web_fetch", "read_file", "read_original"],
        "respond" => vec![],
        _ => vec![
            "read_file", "read_original", "write_file", "patch", "bash", "search",
            "propose_contract", "mark_assertion", "mark_feature", "contract_status", "close_contract",
        ],
    }
}

pub fn category_needs_tools(category: &str) -> bool {
    category != "respond"
}

#[allow(dead_code)]
pub fn min_confidence_for(category: &str) -> Option<f64> {
    CATEGORIES.iter().find(|(n, _)| *n == category).map(|(_, c)| c.min_confidence)
}

// ── Input classification helpers ──

/// Detect a short affirmation like "yes" / "ok" / "go ahead".
pub fn is_affirmation(s: &str) -> bool {
    let trimmed = s.trim().trim_end_matches('.').to_lowercase();
    matches!(
        trimmed.as_str(),
        "yes" | "y" | "yep" | "yeah" | "sure"
            | "ok" | "okay" | "go" | "proceed"
            | "do it" | "continue" | "please" | "please do" | "alright"
    )
}

/// Detect quoted absolute paths or paths with a slash/extension.
pub fn looks_like_path(s: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r#"[\\/]|\.\w{1,5}\s*$|^["'].*["']$"#).expect("valid regex literal")
    });
    RE.is_match(s.trim())
}

/// Detect option-references like "option 2", "do 3", "first", "second".
pub fn looks_like_option_ref(s: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)^(option\s+\d|work\s+on\s+\d|do\s+\d|start\s+with\s+\d|\d+\.?\s*$|first|second|third|fourth)\b").expect("valid regex literal")
    });
    RE.is_match(s.trim())
}

/// Single entry point for tool routing. Consolidates affirmation guard,
/// respond override, and normal classification into one call.
/// Returns the category name and whether the task needs tools.
pub fn classify_and_filter(
    message: &str,
    prior_category: Option<&str>,
) -> RoutingDecision {
    // 1. Affirmation guard — keep the prior turn's tool set.
    if is_affirmation(message) {
        if let Some(cat) = prior_category {
            if cat != "respond" {
                return RoutingDecision { category: cat.into(), needs_tools: true };
            }
        }
        return RoutingDecision { category: "plan".into(), needs_tools: true };
    }
    // 2. Respond override — deterministic classifier with positive confidence.
    let cls = classify_tool_category(message);
    if cls.category == "respond" && cls.confidence > 0.0 {
        return RoutingDecision { category: "respond".into(), needs_tools: false };
    }
    // 3. Normal classification.
    RoutingDecision { category: cls.category.clone(), needs_tools: category_needs_tools(&cls.category) }
}

pub struct RoutingDecision {
    pub category: String,
    pub needs_tools: bool,
}

#[cfg(test)]
mod classify_user_examples {
    use super::*;
    #[test]
    fn say_hello_lands_in_respond() {
        for msg in [
            "say hello",
            "say hello and do nothing else",
            "hi",
            "hello",
            "hey there",
            "thanks",
        ] {
            let r = classify_tool_category(msg);
            assert_eq!(r.category, "respond", "msg = {:?}, scores = {:?}", msg, r.scores);
        }
    }
    #[test]
    fn write_commands_dont_land_in_respond() {
        for msg in ["create a file foo.rs", "write a hello world program", "fix the build"] {
            let r = classify_tool_category(msg);
            assert_ne!(r.category, "respond", "msg = {:?} should NOT be respond, got {} scores = {:?}", msg, r.category, r.scores);
        }
    }
}
