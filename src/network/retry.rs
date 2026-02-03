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

/// Retry executor.
pub struct RetryExecutor;
