//! Generic retry middleware for gRPC RPCs.
//!
//! Provides transport-aware retry with exponential backoff. On transport
//! errors the caller's pool is disconnected so the next attempt gets a
//! fresh HTTP/2 channel instead of reusing the stale one.

use std::future::Future;
use std::time::Duration;

use crate::error::NetworkError;

/// Retry configuration for RPC calls.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of attempts (including the first).
    pub max_attempts: u32,
    /// Base delay between retries (multiplied by attempt number).
    pub base_delay: Duration,
    /// Upper bound on retry delay.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(2),
        }
    }
}

/// Retry an async operation that returns `Result<T, NetworkError>`.
///
/// Only retries when [`NetworkError::is_retryable`] returns true.
/// If `on_transport_error` is provided it is called before each retry
/// when the error is a transport-level failure — use this to disconnect
/// a stale pool so the next attempt creates a fresh channel.
///
/// # Arguments
///
/// * `config` — retry parameters
/// * `operation` — human-readable name for logging
/// * `on_transport_error` — optional callback invoked on transport errors before retry
/// * `f` — closure that produces the RPC future (called once per attempt)
pub async fn retry_unary<F, Fut, T, H>(
    config: &RetryConfig,
    operation: &str,
    mut on_transport_error: Option<H>,
    mut f: F,
) -> Result<T, NetworkError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, NetworkError>>,
    H: FnMut(),
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f().await {
            Ok(val) => {
                if attempt > 1 {
                    tracing::info!(operation, attempt, "RPC succeeded after retry");
                }
                return Ok(val);
            }
            Err(e) => {
                if e.is_retryable() && attempt < config.max_attempts {
                    // On transport errors, signal the caller to reconnect.
                    if e.is_transport_error() {
                        if let Some(ref mut hook) = on_transport_error {
                            hook();
                        }
                    }

                    let delay = config
                        .base_delay
                        .saturating_mul(attempt)
                        .min(config.max_delay);

                    tracing::warn!(
                        operation,
                        error = %e,
                        attempt,
                        max_attempts = config.max_attempts,
                        next_delay_ms = delay.as_millis() as u64,
                        "RPC failed with retryable error — will retry"
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    if attempt > 1 {
                        tracing::error!(
                            operation,
                            error = %e,
                            attempt,
                            "RPC failed after all retry attempts"
                        );
                    }
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_succeeds_first_try() {
        let calls = AtomicU32::new(0);
        let result = retry_unary(&RetryConfig::default(), "test_op", None::<fn()>, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, NetworkError>(42) }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retries_on_unavailable() {
        let calls = AtomicU32::new(0);
        let config = RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(1), // Fast for tests
            max_delay: Duration::from_millis(10),
        };

        let result = retry_unary(&config, "test_op", None::<fn()>, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(NetworkError::Unavailable)
                } else {
                    Ok(99)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_no_retry_on_non_retryable() {
        let calls = AtomicU32::new(0);
        let result = retry_unary(&RetryConfig::default(), "test_op", None::<fn()>, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<i32, _>(NetworkError::NotFound("x".into())) }
        })
        .await;

        assert!(matches!(result, Err(NetworkError::NotFound(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_exhausts_retries() {
        let calls = AtomicU32::new(0);
        let config = RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        };

        let result = retry_unary(&config, "test_op", None::<fn()>, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<i32, _>(NetworkError::Unavailable) }
        })
        .await;

        assert!(matches!(result, Err(NetworkError::Unavailable)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_transport_error_hook_called() {
        let hook_calls = AtomicU32::new(0);
        let config = RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        };

        let _ = retry_unary(
            &config,
            "test_op",
            Some(|| {
                hook_calls.fetch_add(1, Ordering::SeqCst);
            }),
            || async { Err::<i32, _>(NetworkError::Unavailable) },
        )
        .await;

        // Hook called once (only on first failure, second failure exhausts retries)
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    }
}
