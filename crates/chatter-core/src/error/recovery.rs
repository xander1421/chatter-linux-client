//! Error recovery strategies.
//!
//! Strategies for recovering from errors.

/// Error recovery handler.
pub struct ErrorRecovery;

/// Recovery action to take.
pub enum RecoveryAction {
    /// Retry the operation.
    Retry,
    /// Queue for later.
    QueueForRetry,
    /// Retry with backoff.
    RetryWithBackoff,
    /// Re-authenticate.
    Reauthenticate,
    /// Show error to user.
    ShowError,
}
