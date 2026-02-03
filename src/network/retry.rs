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
