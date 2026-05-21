//! Lightweight token
//! budget for retrieved snippets — picks the highest-scoring ones until the
//! budget is exhausted.

#[derive(Debug, Clone)]
pub struct Candidate {
    pub key: String,
    pub text: String,
    pub score: f64,
    pub tokens: usize,
}

pub fn pick_within_budget(mut candidates: Vec<Candidate>, max_tokens: usize) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    let mut used = 0usize;
    for c in candidates {
        if used + c.tokens > max_tokens {
            continue;
        }
        used += c.tokens;
        out.push(c);
    }
    out
}
