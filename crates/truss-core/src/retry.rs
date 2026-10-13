//! Automatic retry with exponential backoff + jitter.
//!
//! Retries are driven by the error taxonomy in [`crate::error`]: only
//! [`ProviderError::is_retryable`] failures are retried, and a server-supplied
//! `Retry-After` is honored over the computed backoff. Cancellation short-circuits
//! the wait — a Ctrl-C during a backoff sleep returns immediately.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ctx::Ctx;
use crate::error::ProviderError;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(20),
        }
    }
}

impl RetryPolicy {
    pub fn none() -> Self {
        RetryPolicy {
            max_retries: 0,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }
}

/// Cheap deterministic-enough jitter in `[0, 1)` without a rng dependency.
fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

fn backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let exp = policy.base_delay.as_millis() as f64 * 2f64.powi(attempt as i32);
    let capped = exp.min(policy.max_delay.as_millis() as f64);
    // Full jitter: sleep a random amount in [0, capped].
    Duration::from_millis((capped * jitter()) as u64)
}

/// Run `op` with retries. `op` receives the zero-based attempt number.
pub async fn with_retry<F, Fut, T>(
    ctx: &Ctx,
    policy: &RetryPolicy,
    op: F,
) -> Result<T, ProviderError>
where
    F: Fn(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut attempt = 0u32;
    loop {
        if ctx.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !e.is_retryable() || attempt >= policy.max_retries {
                    return Err(e);
                }
                let delay = match e.retry_after_secs() {
                    Some(secs) => Duration::from_secs(secs),
                    None => backoff(policy, attempt),
                };
                tracing::warn!(attempt, ?delay, error = %e, "retrying provider call");
                // Interruptible sleep: cancellation wins over the backoff.
                let token = ctx.cancel_token();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = token.cancelled() => {
                        return Err(ProviderError::Cancelled);
                    }
                }
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let ctx = Ctx::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<u8, ProviderError> = with_retry(&ctx, &fast_policy(), move |_| {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(ProviderError::Transport("blip".into())) // retryable
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry");
    }

    #[tokio::test]
    async fn does_not_retry_fatal() {
        let ctx = Ctx::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<u8, ProviderError> = with_retry(&ctx, &fast_policy(), move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Decode("bad json".into())) // fatal
            }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fatal errors are not retried"
        );
    }

    #[tokio::test]
    async fn exhausts_retries() {
        let ctx = Ctx::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<u8, ProviderError> = with_retry(&ctx, &fast_policy(), move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Timeout) // always retryable
            }
        })
        .await;
        assert!(out.is_err());
        // initial try + max_retries
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
