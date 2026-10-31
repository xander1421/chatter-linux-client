//! Error types for anonymous group service.

use crate::domain::UserId;
use crate::domain::anon_group::AnonGroupId;
use crate::error::StorageError;

/// Anon group service errors.
#[derive(Debug, thiserror::Error)]
pub enum AnonGroupError {
    #[error("Delivery unconfirmed; the original ciphertext is retained for retry")]
    DeliveryUnconfirmed,
    /// Not logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Group not found.
    #[error("group not found: {0}")]
    GroupNotFound(AnonGroupId),

    /// Member not found.
    #[error("member not found: {0}")]
    MemberNotFound(UserId),

    /// Insufficient permissions.
    #[error("insufficient permissions: {0}")]
    InsufficientPermissions(String),

    /// Cannot kick yourself.
    #[error("cannot kick yourself")]
    CannotKickSelf,

    /// Cannot kick the owner.
    #[error("cannot kick the owner")]
    CannotKickOwner,

    /// The owner tried to leave while other members remain: nobody could
    /// issue member lists, kicks or role changes after them.
    #[error("make another member the owner before leaving")]
    OwnerMustTransfer,

    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Current user error.
    #[error("current user error: {0}")]
    CurrentUser(#[from] crate::services::current_user::CurrentUserError),

    /// Message send error.
    #[error("message error: {0}")]
    MessageError(String),

    /// Crypto error.
    #[error("crypto error: {0}")]
    CryptoError(String),

    /// Invalid data.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Rate limit exceeded.
    #[error("rate limit exceeded: {0} messages per minute")]
    RateLimitExceeded(u32),

    /// Group is blocked.
    #[error("group is blocked: {0}")]
    GroupBlocked(AnonGroupId),

    /// User is blocked in this group.
    #[error("user is blocked in group: {0}")]
    UserBlocked(UserId),
}

impl From<crate::services::pairwise::PairwiseError> for AnonGroupError {
    fn from(e: crate::services::pairwise::PairwiseError) -> Self {
        use crate::services::pairwise::PairwiseError;
        match e {
            PairwiseError::Network(e) => Self::MessageError(e.to_string()),
            PairwiseError::Storage(e) => Self::Storage(e),
            e => Self::CryptoError(e.to_string()),
        }
    }
}

impl AnonGroupError {
    /// Static metadata survives redaction without exposing error text or IDs.
    pub(crate) fn trace_invite_failure(&self) {
        match self {
            Self::Storage(_) => {
                tracing::warn!(target: "group_invite::storage", "invite acceptance failed")
            }
            Self::MessageError(_) | Self::DeliveryUnconfirmed => {
                tracing::warn!(target: "group_invite::delivery", "invite acceptance failed")
            }
            Self::CryptoError(_) => {
                tracing::warn!(target: "group_invite::crypto_or_token", "invite acceptance failed")
            }
            Self::InvalidData(_) | Self::MemberNotFound(_) => {
                tracing::warn!(target: "group_invite::invalid_state", "invite acceptance failed")
            }
            _ => tracing::warn!(target: "group_invite::other", "invite acceptance failed"),
        }
    }
}
