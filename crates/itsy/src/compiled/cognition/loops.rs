//! Retry-loop helper. The JS code
//! is async/await driven; we mirror it with an async closure.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub jitter_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_attempts: 3, backoff_ms: 250, jitter_ms: 50 }
    }
}

pub async fn run_with_retry<F, T, E, Fut>(cfg: RetryConfig, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last: Option<E> = None;
    for attempt in 0..cfg.max_attempts {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                if attempt + 1 < cfg.max_attempts {
                    let jitter = (rand::random::<u64>() % cfg.jitter_ms.max(1)) as u64;
                    tokio::time::sleep(Duration::from_millis(cfg.backoff_ms + jitter)).await;
                }
            }
        }
    }
    Err(last.expect("no attempts ran"))
}
