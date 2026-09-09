//! Generic async retry helper with exponential backoff and jitter.

use std::future::Future;
use std::hash::{Hash, Hasher};
use std::time::Duration;

/// Configuration for [`retry_with_backoff`].
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// How many times to retry after the first failed attempt (total invocations ≤ `max_retries + 1`).
    pub max_retries: u32,
    /// Initial delay before the first retry, in milliseconds.
    pub initial_delay_ms: u64,
    /// Upper bound on delay between attempts, in milliseconds.
    pub max_delay_ms: u64,
    /// Multiplier applied to the delay after each failed attempt.
    pub backoff_factor: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 30_000,
            backoff_factor: 2.0,
        }
    }
}

/// Returns a factor in `[0.75, 1.25]` using a cheap mix of time and thread id (jitter ≈ ±25%).
fn jitter_factor() -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::thread::current().id().hash(&mut h);
    let n = h.finish();
    let r = (n % 10_000) as f64 / 10_000.0;
    0.75 + r * 0.5
}

/// Retries `operation` using [`RetryConfig::default`] (same behavior as [`retry_with_backoff`] with default settings).
pub async fn retry_async<F, Fut, T, E>(operation: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    retry_with_backoff(&RetryConfig::default(), operation).await
}

/// Runs `operation` until it succeeds or `config.max_retries` is exhausted, sleeping with exponential backoff between attempts.
///
/// The sleep duration is multiplied by [`jitter_factor`] (roughly ±25%) to avoid synchronized retries.
pub async fn retry_with_backoff<F, Fut, T, E>(config: &RetryConfig, operation: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt: u32 = 0;
    let mut delay_ms = config.initial_delay_ms as f64;

    loop {
        match operation().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= config.max_retries {
                    return Err(e);
                }
                let jittered = ((delay_ms * jitter_factor()) as u64).max(1);
                let capped = jittered.min(config.max_delay_ms);
                tracing::debug!(
                    attempt = attempt + 1,
                    max_retries = config.max_retries,
                    delay_ms = capped,
                    error = %e,
                    "retrying after a failed request"
                );
                tokio::time::sleep(Duration::from_millis(capped)).await;
                delay_ms = (delay_ms * config.backoff_factor).min(config.max_delay_ms as f64);
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    #[derive(Debug, Clone)]
    struct CountingError(&'static str);

    impl std::fmt::Display for CountingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let config = RetryConfig::default();
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let out = retry_with_backoff(&config, || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, CountingError>(42u32)
            }
        })
        .await
        .expect("should succeed");
        assert_eq!(out, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn succeeds_on_third_attempt() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay_ms: 1,
            max_delay_ms: 10,
            backoff_factor: 2.0,
        };
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let out = retry_with_backoff(&config, || {
            let c = Arc::clone(&c);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(CountingError("transient"))
                } else {
                    Ok(n)
                }
            }
        })
        .await
        .expect("should succeed on third try");
        assert_eq!(out, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        let config = RetryConfig {
            max_retries: 2,
            initial_delay_ms: 1,
            max_delay_ms: 5,
            backoff_factor: 2.0,
        };
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let err = retry_with_backoff(&config, || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(CountingError("always"))
            }
        })
        .await
        .expect_err("should fail");
        assert_eq!(err.to_string(), "always");
        // initial + 2 retries = 3 invocations
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
