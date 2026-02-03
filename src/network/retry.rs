//! Retry logic.
//!
//! Retry logic with jitter for failed operations.

use std::time::Duration;

use crate::error::NetworkError;
use crate::network::recovery::FibonacciBackoff;

/// Retry policy configuration.
///
/// Configures how retry attempts are made, including maximum attempts,
/// timeout sequences, jitter, and whether to retry authentication errors.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum retry attempts (0 = no retries, None = unlimited).
    pub max_attempts: Option<usize>,
    /// Timeout sequence to use (STANDARD or EXTENDED).
    pub timeouts: &'static [Duration],
    /// Maximum jitter to add to backoff delays.
    pub jitter: Duration,
    /// Whether to retry on auth errors (usually false).
    pub retry_auth_errors: bool,
}

impl Default for RetryPolicy {
    /// Create a default retry policy.
    ///
    /// - 3 attempts
    /// - STANDARD timeouts
    /// - 5s jitter
    /// - No auth error retry
    fn default() -> Self {
        Self {
            max_attempts: Some(3),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::from_secs(5),
            retry_auth_errors: false,
        }
    }
}

impl RetryPolicy {
    /// Create an aggressive retry policy for critical operations.
    ///
    /// - 5 attempts
    /// - STANDARD timeouts
    /// - 2s jitter
    /// - No auth error retry
    pub fn aggressive() -> Self {
        Self {
            max_attempts: Some(5),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::from_secs(2),
            retry_auth_errors: false,
        }
    }

    /// Create a patient retry policy for prolonged offline scenarios.
    ///
    /// - 10 attempts
    /// - EXTENDED timeouts
    /// - 10s jitter
    /// - No auth error retry
    pub fn patient() -> Self {
        Self {
            max_attempts: Some(10),
            timeouts: FibonacciBackoff::EXTENDED,
            jitter: Duration::from_secs(10),
            retry_auth_errors: false,
        }
    }

    /// Create a no-retry policy (single attempt only).
    ///
    /// - 0 retries (1 total attempt)
    /// - STANDARD timeouts (unused)
    /// - No jitter
    /// - No auth error retry
    pub fn no_retry() -> Self {
        Self {
            max_attempts: Some(0),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        }
    }

    /// Determine if an operation should be retried.
    ///
    /// Returns `true` if:
    /// - The error is retryable (or is an auth error with `retry_auth_errors` enabled)
    /// - The attempt count hasn't exceeded `max_attempts` (if set)
    ///
    /// # Arguments
    ///
    /// * `error` - The error that occurred
    /// * `attempt` - The current attempt number (1-indexed)
    pub fn should_retry(&self, error: &NetworkError, attempt: usize) -> bool {
        // Check if we've exceeded max attempts
        if let Some(max) = self.max_attempts {
            if attempt > max {
                return false;
            }
        }

        // Check if the error is retryable
        if error.is_retryable() {
            return true;
        }

        // Check if we should retry auth errors
        if self.retry_auth_errors && error.is_auth_error() {
            return true;
        }

        false
    }
}

/// Retry executor that wraps async operations with retry logic.
///
/// Uses the configured `RetryPolicy` to determine when and how to retry
/// failed operations, with backoff delays provided by `FibonacciBackoff`.
pub struct RetryExecutor {
    policy: RetryPolicy,
}

impl RetryExecutor {
    /// Create a new RetryExecutor with the given policy.
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }

    /// Create a RetryExecutor with the default policy.
    pub fn with_default_policy() -> Self {
        Self::new(RetryPolicy::default())
    }

    /// Execute an async operation with retry.
    ///
    /// The operation is called repeatedly until:
    /// - It succeeds (returns Ok)
    /// - It fails with a non-retryable error
    /// - Maximum attempts are exhausted
    ///
    /// Returns the operation result or the last error.
    ///
    /// # Arguments
    ///
    /// * `operation` - A closure that returns a Future producing a Result
    ///
    /// # Example
    ///
    /// ```ignore
    /// let executor = RetryExecutor::new(RetryPolicy::default());
    /// let result = executor.execute(|| async {
    ///     // Your async operation here
    ///     Ok::<_, NetworkError>(())
    /// }).await;
    /// ```
    pub async fn execute<F, Fut, T, E>(&self, operation: F) -> Result<T, E>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: Into<NetworkError> + From<NetworkError> + Clone,
    {
        self.execute_with(operation, |err, attempt| {
            let network_err: NetworkError = err.clone().into();
            self.policy.should_retry(&network_err, attempt)
        })
        .await
    }

    /// Execute an async operation with a custom retry predicate.
    ///
    /// Similar to `execute()`, but allows custom logic to determine
    /// whether to retry.
    ///
    /// # Arguments
    ///
    /// * `operation` - A closure that returns a Future producing a Result
    /// * `should_retry` - A predicate that takes the error and attempt number,
    ///   returns true if the operation should be retried
    ///
    /// # Example
    ///
    /// ```ignore
    /// let executor = RetryExecutor::new(RetryPolicy::default());
    /// let result = executor.execute_with(
    ///     || async { /* operation */ Ok::<_, MyError>(()) },
    ///     |err, attempt| attempt <= 3 && err.is_temporary(),
    /// ).await;
    /// ```
    pub async fn execute_with<F, Fut, T, E, P>(&self, operation: F, should_retry: P) -> Result<T, E>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        P: Fn(&E, usize) -> bool,
    {
        let mut backoff = FibonacciBackoff::with_jitter(self.policy.timeouts, self.policy.jitter);
        let mut attempt = 1usize;

        loop {
            match operation().await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    // Check if we should retry
                    if !should_retry(&err, attempt) {
                        return Err(err);
                    }

                    // Get next backoff delay and sleep
                    let delay = backoff.next();
                    tokio::time::sleep(delay).await;

                    attempt += 1;
                }
            }
        }
    }

    /// Get a reference to the retry policy.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // ==================== RetryPolicy Tests ====================

    #[test]
    fn test_default_policy() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.max_attempts, Some(3));
        assert_eq!(policy.timeouts.len(), FibonacciBackoff::STANDARD.len());
        assert_eq!(policy.jitter, Duration::from_secs(5));
        assert!(!policy.retry_auth_errors);
    }

    #[test]
    fn test_aggressive_policy() {
        let policy = RetryPolicy::aggressive();

        assert_eq!(policy.max_attempts, Some(5));
        assert_eq!(policy.jitter, Duration::from_secs(2));
        assert!(!policy.retry_auth_errors);
    }

    #[test]
    fn test_patient_policy() {
        let policy = RetryPolicy::patient();

        assert_eq!(policy.max_attempts, Some(10));
        assert_eq!(policy.timeouts.len(), FibonacciBackoff::EXTENDED.len());
        assert_eq!(policy.jitter, Duration::from_secs(10));
        assert!(!policy.retry_auth_errors);
    }

    #[test]
    fn test_no_retry_policy() {
        let policy = RetryPolicy::no_retry();

        assert_eq!(policy.max_attempts, Some(0));
        assert_eq!(policy.jitter, Duration::ZERO);

        // should_retry always returns false for no_retry
        let err = NetworkError::Unavailable;
        assert!(!policy.should_retry(&err, 1));
    }

    #[test]
    fn test_should_retry_retryable_error() {
        let policy = RetryPolicy::default();

        // Retryable errors should return true
        assert!(policy.should_retry(&NetworkError::Unavailable, 1));
        assert!(policy.should_retry(&NetworkError::RateLimited, 1));
        assert!(policy.should_retry(&NetworkError::ServerError("test".into()), 1));
        assert!(policy.should_retry(&NetworkError::Timeout, 1));
    }

    #[test]
    fn test_should_retry_non_retryable_error() {
        let policy = RetryPolicy::default();

        // Non-retryable errors should return false
        assert!(!policy.should_retry(&NetworkError::NotFound("test".into()), 1));
        assert!(!policy.should_retry(&NetworkError::InvalidRequest("test".into()), 1));
        assert!(!policy.should_retry(&NetworkError::Unauthenticated, 1));
        assert!(!policy.should_retry(&NetworkError::PermissionDenied, 1));
    }

    #[test]
    fn test_should_retry_respects_max_attempts() {
        let policy = RetryPolicy::default(); // max_attempts = 3

        let err = NetworkError::Unavailable; // retryable

        // Attempts 1-3 should be allowed
        assert!(policy.should_retry(&err, 1));
        assert!(policy.should_retry(&err, 2));
        assert!(policy.should_retry(&err, 3));

        // Attempt 4 should be denied (exceeds max)
        assert!(!policy.should_retry(&err, 4));
    }

    #[test]
    fn test_should_retry_unlimited_attempts() {
        let policy = RetryPolicy {
            max_attempts: None, // unlimited
            ..RetryPolicy::default()
        };

        let err = NetworkError::Unavailable;

        // Should allow very high attempt counts
        assert!(policy.should_retry(&err, 100));
        assert!(policy.should_retry(&err, 1000));
    }

    #[test]
    fn test_should_retry_auth_errors_when_enabled() {
        let policy = RetryPolicy {
            retry_auth_errors: true,
            ..RetryPolicy::default()
        };

        // Auth errors should be retryable when enabled
        assert!(policy.should_retry(&NetworkError::Unauthenticated, 1));
        assert!(policy.should_retry(&NetworkError::PermissionDenied, 1));
    }

    // ==================== RetryExecutor Tests ====================

    #[tokio::test]
    async fn test_execute_succeeds_first_try() {
        let executor = RetryExecutor::new(RetryPolicy {
            max_attempts: Some(3),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        });

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result: Result<&str, NetworkError> = executor
            .execute(|| {
                let count = call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok("success")
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_execute_retries_on_transient_error() {
        let executor = RetryExecutor::new(RetryPolicy {
            max_attempts: Some(3),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        });

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result: Result<&str, NetworkError> = executor
            .execute(|| {
                let count = call_count_clone.clone();
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // First call fails with retryable error
                        Err(NetworkError::Unavailable)
                    } else {
                        // Second call succeeds
                        Ok("success")
                    }
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_execute_fails_non_retryable() {
        let executor = RetryExecutor::new(RetryPolicy {
            max_attempts: Some(3),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        });

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result: Result<&str, NetworkError> = executor
            .execute(|| {
                let count = call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Non-retryable error - should not retry
                    Err(NetworkError::NotFound("test resource".into()))
                }
            })
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), NetworkError::NotFound(_)));
        // Should only be called once (no retry for non-retryable errors)
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_execute_exhausts_retries() {
        let executor = RetryExecutor::new(RetryPolicy {
            max_attempts: Some(3),
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        });

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result: Result<&str, NetworkError> = executor
            .execute(|| {
                let count = call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Always fail with retryable error
                    Err(NetworkError::Unavailable)
                }
            })
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), NetworkError::Unavailable));
        // 1 initial + 3 retries = 4 total attempts
        assert_eq!(call_count.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn test_execute_with_custom_predicate() {
        let executor = RetryExecutor::new(RetryPolicy {
            max_attempts: Some(10), // High limit to test custom predicate
            timeouts: FibonacciBackoff::STANDARD,
            jitter: Duration::ZERO,
            retry_auth_errors: false,
        });

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Custom predicate: only retry exactly 2 times
        let result: Result<&str, NetworkError> = executor
            .execute_with(
                || {
                    let count = call_count_clone.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Err(NetworkError::Unavailable)
                    }
                },
                |_err, attempt| attempt <= 2, // Allow 2 retries (attempts 1 and 2)
            )
            .await;

        assert!(result.is_err());
        // 1 initial + 2 retries = 3 total attempts
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_execute_with_no_retry_policy() {
        let executor = RetryExecutor::new(RetryPolicy::no_retry());

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result: Result<&str, NetworkError> = executor
            .execute(|| {
                let count = call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(NetworkError::Unavailable)
                }
            })
            .await;

        assert!(result.is_err());
        // No retries - only 1 attempt
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_executor_policy_accessor() {
        let policy = RetryPolicy::aggressive();
        let executor = RetryExecutor::new(policy.clone());

        assert_eq!(executor.policy().max_attempts, Some(5));
    }

    #[test]
    fn test_executor_with_default_policy() {
        let executor = RetryExecutor::with_default_policy();

        assert_eq!(executor.policy().max_attempts, Some(3));
    }
}
