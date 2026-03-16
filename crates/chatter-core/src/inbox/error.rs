//! Inbox error types.
//!
//! Error types for inbox operations including polling, authentication,
//! and message acknowledgment.

/// Errors that can occur during inbox operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InboxError {
    /// Poll request failed.
    #[error("poll failed: {0}")]
    PollFailed(String),

    /// Challenge-response authentication failed.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// Message acknowledgment failed.
    #[error("acknowledge failed: {0}")]
    AcknowledgeFailed(String),

    /// Ed25519 signing failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),

    /// Provider is not running.
    #[error("provider not running")]
    NotRunning,

    /// Provider is already running.
    #[error("provider already running")]
    AlreadyRunning,

    /// Shutdown timed out.
    #[error("shutdown timed out")]
    ShutdownTimeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_error_display() {
        let err = InboxError::PollFailed("timeout".to_string());
        assert_eq!(err.to_string(), "poll failed: timeout");

        let err = InboxError::AuthFailed("invalid signature".to_string());
        assert_eq!(err.to_string(), "authentication failed: invalid signature");

        let err = InboxError::NotRunning;
        assert_eq!(err.to_string(), "provider not running");
    }
}
