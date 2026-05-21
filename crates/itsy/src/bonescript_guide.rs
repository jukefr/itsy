//! BoneScript syntax reference text injected into the system prompt when
//! the task type is `backend` — keeps the model honest about which BoneScript
//! primitives exist without burning a doc-lookup tool call.

use once_cell::sync::Lazy;
use regex::Regex;

pub const BONESCRIPT_GUIDE: &str = "BoneScript Quick Reference:
- system: top-level container for the entire backend
- entity: data model with fields, constraints, state machine, auth
  owns: [field: type, ...]    (string, int, float, bool, uuid, timestamp, json)
  constraints: [field.unique, field.length in min..max, field.required]
  states: state1 -> state2 -> state3 (state machine transitions)
  auth: jwt | api_key | oauth2
- capability: operation with preconditions, effects, events
  requires: [preconditions]
  effects: [state changes, side effects]
  emits: EventName
  sync: transactional | eventual | fire_and_forget
- event: durable message between services
  payload: { field: type, ... }
  delivery: exactly_once | at_least_once | best_effort
- channel: WebSocket real-time channel
  ordering: fifo | causal | none
  persistence: durable | ephemeral
- policy: rate limiting, audit, encryption
  rate_limit: N per Xs/Xm/Xh
  audit: true | false
  encryption: aes256 | none
- store: database engine selection
  engine: postgres | sqlite | mysql
- flow: multi-step saga with compensation
  steps: [step1, step2, ...]
  on_failure: compensate | abort | retry
- extension_point: custom logic hooks (user code survives recompilation)

Compile: bone_compile <file.bone>
Check:   bone_check <file.bone>

Targets: express (default), nakama, prisma, sqlite";

pub fn get_bonescript_guide() -> &'static str {
    BONESCRIPT_GUIDE
}

static EXPLICIT_BONE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b\.bone\b|\bbonescript\b").unwrap());
static NON_NODE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(python|django|fastapi|flask|go|golang|rust|actix|axum|ruby|rails|php|laravel|java|spring|c#|dotnet|asp\.net|elixir|phoenix)\b").unwrap()
});
static NODE_BACKEND_A: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(api|backend|server|rest|crud|auth|database|endpoint|express|fastify|node|typescript|ts)\b.*\b(create|build|make|implement|set up)\b").unwrap()
});
static NODE_BACKEND_B: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(create|build|make)\b.*\b(api|backend|server|rest|crud|endpoint)\b").unwrap()
});
static NODE_BACKEND_C: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(node|typescript|ts|express|fastify)\b.*\b(api|backend|server|rest|crud)\b").unwrap()
});

/// Check if a task message suggests backend work that should use BoneScript.
/// Only triggers for Node.js/TypeScript backends.
pub fn should_use_bonescript(message: &str) -> bool {
    if EXPLICIT_BONE.is_match(message) {
        return true;
    }
    if NON_NODE.is_match(message) {
        return false;
    }
    NODE_BACKEND_A.is_match(message) || NODE_BACKEND_B.is_match(message) || NODE_BACKEND_C.is_match(message)
}
