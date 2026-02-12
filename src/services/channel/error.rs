//! Channel service errors.

use crate::domain::ids::ChannelId;
use crate::error::{NetworkError, StorageError};

/// Channel service errors.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(ChannelId),

    /// User is not authorized for operation.
    #[error("not authorized")]
    NotAuthorized,

    /// Input validation failed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_error_display() {
        let err = ChannelError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = ChannelError::InvalidInput("empty name".to_string());
        assert!(err.to_string().contains("empty name"));

        let channel_id = ChannelId::new();
        let err = ChannelError::ChannelNotFound(channel_id);
        assert!(err.to_string().contains("channel not found"));

        let err = ChannelError::NotAuthorized;
        assert!(err.to_string().contains("not authorized"));
    }
}
