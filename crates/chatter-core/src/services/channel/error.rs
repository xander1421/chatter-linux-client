//! Channel service errors.

use crate::domain::ids::ChannelId;
use crate::error::{NetworkError, StorageError};
use crate::services::current_user::CurrentUserError;

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

    /// Sender key rotation failed after member removal.
    ///
    /// This is a security-critical error: the removed member still has
    /// the old sender key, so we MUST rotate before sending any new messages.
    #[error("sender key rotation failed: {0}")]
    SenderKeyRotationFailed(String),
}

impl From<CurrentUserError> for ChannelError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => ChannelError::Storage(e),
            CurrentUserError::NotLoggedIn => ChannelError::NotLoggedIn,
        }
    }
}

impl From<super::membership_log::LogError> for ChannelError {
    fn from(err: super::membership_log::LogError) -> Self {
        use super::membership_log::LogError;
        use super::verified_state::VerifyError;
        match err {
            LogError::Network(e) => Self::Network(e),
            LogError::Storage(e) => Self::Storage(e),
            LogError::Intent(VerifyError::NotPermitted(_)) => Self::NotAuthorized,
            LogError::Intent(e) => Self::InvalidInput(e.to_string()),
            other => Self::InvalidInput(other.to_string()),
        }
    }
}

impl ChannelError {
    /// Text for a toast or status line; the `Display` chain
    /// ("network error: invalid request: precondition failed: ...") is for logs.
    ///
    /// The server's reason for a refused request (already a member, banned,
    /// ...) arrives as the last `": "`-separated segment of an `InvalidRequest`.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::NotLoggedIn => "You're signed out.".to_string(),
            Self::NotAuthorized | Self::Network(NetworkError::PermissionDenied) => {
                "You don't have permission to do that.".to_string()
            }
            Self::ChannelNotFound(_) => "Channel not found.".to_string(),
            Self::InvalidInput(m) | Self::Network(NetworkError::InvalidRequest(m)) => {
                let reason = m.rsplit(": ").next().unwrap_or(m);
                let mut c = reason.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect::<String>() + ".")
                    .unwrap_or_else(|| "Something went wrong. Try again.".to_string())
            }
            Self::Network(NetworkError::RateLimited) => "Too many requests. Try again later.".to_string(),
            Self::Network(_) => "Couldn't reach the server. Try again.".to_string(),
            Self::Storage(_) | Self::SenderKeyRotationFailed(_) => {
                "Something went wrong. Try again.".to_string()
            }
        }
    }
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

    #[test]
    fn user_message_carries_the_servers_reason() {
        // AlreadyExists / FailedPrecondition as NetworkError::from_status builds them.
        let err = ChannelError::Network(NetworkError::InvalidRequest(
            "already exists: user is already a member of this channel".to_string(),
        ));
        assert_eq!(err.user_message(), "User is already a member of this channel.");

        let err = ChannelError::Network(NetworkError::InvalidRequest(
            "precondition failed: user is banned from this channel".to_string(),
        ));
        assert_eq!(err.user_message(), "User is banned from this channel.");

        assert_eq!(
            ChannelError::Network(NetworkError::Unavailable).user_message(),
            "Couldn't reach the server. Try again."
        );
    }
}
