//! Call error types.

/// Errors that can occur during call operations.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("Already in a call")]
    AlreadyInCall,

    #[error("Not in a call")]
    NotInCall,

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Server error: {0}")]
    ServerError(String),

    #[error("Crypto error: {0}")]
    CryptoError(String),

    #[error("Invalid call ID")]
    InvalidCallId,

    #[error("Invalid channel ID")]
    InvalidChannelId,

    #[error("Invalid user ID")]
    InvalidUserId,

    #[error("Permission denied")]
    PermissionDenied,

    #[error("Call not found")]
    NotFound,

    #[error("Call ended")]
    CallEnded,

    #[error("Track publish failed: {0}")]
    TrackPublishFailed(String),

    #[error("Device error: {0}")]
    DeviceError(String),

    #[error("Call worker shut down")]
    WorkerShutdown,
}
