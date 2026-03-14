//! Mailbox-specific errors.
//!
//! Errors related to blind signature tokens and ephemeral mailbox operations.

use thiserror::Error;

/// Content padding errors.
#[derive(Debug, Clone, Error)]
pub enum ContentError {
    /// Content exceeds maximum size.
    #[error("content too large: {0} bytes (max {1})")]
    ContentTooLarge(usize, usize),

    /// Invalid padded content (corrupted or tampered).
    #[error("invalid padded content: {0}")]
    InvalidPadding(String),
}

/// Mailbox error for token and mailbox operations.
#[derive(Debug, Clone, Error)]
pub enum MailboxError {
    /// Blind signature operation failed.
    #[error("blind signature error: {0}")]
    BlindSignature(String),

    /// Server public key parsing or validation failed.
    #[error("invalid server key: {0}")]
    InvalidServerKey(String),

    /// Token has expired.
    #[error("token expired")]
    TokenExpired,

    /// Daily token quota exhausted.
    #[error("token quota exhausted")]
    QuotaExhausted,

    /// No tokens available for use.
    #[error("no tokens available")]
    NoTokensAvailable,

    /// Token not found in storage.
    #[error("token not found: {0}")]
    TokenNotFound(String),

    /// Mailbox not found on server.
    #[error("mailbox not found on server")]
    MailboxNotFound,

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(String),

    /// Conversation already exists (bootstrap not needed).
    #[error("conversation already exists")]
    ConversationExists,

    /// Conversation not found in storage.
    #[error("conversation not found")]
    ConversationNotFound,

    /// No mailbox configured for conversation.
    #[error("no mailbox configured")]
    NoMailboxConfigured,

    /// Peer's mailbox address unknown.
    #[error("peer mailbox unknown")]
    PeerMailboxUnknown,

    /// Conversation not fully established (no shared secret).
    #[error("conversation not established")]
    ConversationNotEstablished,

    /// Key derivation failed.
    #[error("key derivation failed: {0}")]
    KeyDerivationFailed(String),

    /// Group not found in storage.
    #[error("group not found")]
    GroupNotFound,

    /// Group not ready for messaging (no member mailboxes).
    #[error("group not ready: no member mailboxes configured")]
    GroupNotReady,

    /// Fanout had partial failures.
    #[error("fanout partial failure: {successful} succeeded, {failed} failed")]
    FanoutPartialFailure { successful: usize, failed: usize },

    /// Content encoding/padding error.
    #[error("content error: {0}")]
    Content(#[from] ContentError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = MailboxError::BlindSignature("failed to blind key".to_string());
        assert_eq!(err.to_string(), "blind signature error: failed to blind key");

        let err = MailboxError::TokenExpired;
        assert_eq!(err.to_string(), "token expired");

        let err = MailboxError::QuotaExhausted;
        assert_eq!(err.to_string(), "token quota exhausted");

        let err = MailboxError::InvalidServerKey("invalid DER format".to_string());
        assert_eq!(err.to_string(), "invalid server key: invalid DER format");
    }

    #[test]
    fn test_error_variants() {
        let errors: Vec<MailboxError> = vec![
            MailboxError::BlindSignature("test".to_string()),
            MailboxError::InvalidServerKey("test".to_string()),
            MailboxError::TokenExpired,
            MailboxError::QuotaExhausted,
            MailboxError::NoTokensAvailable,
            MailboxError::TokenNotFound("id".to_string()),
            MailboxError::Storage("db error".to_string()),
            MailboxError::Network("connection failed".to_string()),
            MailboxError::ConversationExists,
            MailboxError::ConversationNotFound,
            MailboxError::NoMailboxConfigured,
            MailboxError::PeerMailboxUnknown,
            MailboxError::ConversationNotEstablished,
            MailboxError::KeyDerivationFailed("test".to_string()),
            MailboxError::GroupNotFound,
            MailboxError::GroupNotReady,
            MailboxError::FanoutPartialFailure {
                successful: 3,
                failed: 2,
            },
            MailboxError::Content(ContentError::ContentTooLarge(100_000, 65_536)),
        ];

        // All variants should implement Error (can call to_string)
        for err in errors {
            let _ = err.to_string();
        }
    }

    #[test]
    fn test_conversation_exists_display() {
        let err = MailboxError::ConversationExists;
        assert_eq!(err.to_string(), "conversation already exists");
    }

    #[test]
    fn test_dm_error_variants_display() {
        let err = MailboxError::ConversationNotFound;
        assert_eq!(err.to_string(), "conversation not found");

        let err = MailboxError::NoMailboxConfigured;
        assert_eq!(err.to_string(), "no mailbox configured");

        let err = MailboxError::PeerMailboxUnknown;
        assert_eq!(err.to_string(), "peer mailbox unknown");

        let err = MailboxError::ConversationNotEstablished;
        assert_eq!(err.to_string(), "conversation not established");

        let err = MailboxError::KeyDerivationFailed("X25519 ECDH failed".to_string());
        assert_eq!(err.to_string(), "key derivation failed: X25519 ECDH failed");
    }

    #[test]
    fn test_group_error_variants_display() {
        let err = MailboxError::GroupNotFound;
        assert_eq!(err.to_string(), "group not found");

        let err = MailboxError::GroupNotReady;
        assert_eq!(
            err.to_string(),
            "group not ready: no member mailboxes configured"
        );

        let err = MailboxError::FanoutPartialFailure {
            successful: 5,
            failed: 2,
        };
        assert_eq!(
            err.to_string(),
            "fanout partial failure: 5 succeeded, 2 failed"
        );
    }

    #[test]
    fn test_content_error_display() {
        let err = ContentError::ContentTooLarge(100_000, 65_536);
        assert_eq!(err.to_string(), "content too large: 100000 bytes (max 65536)");

        let err = ContentError::InvalidPadding("corrupt data".to_string());
        assert_eq!(err.to_string(), "invalid padded content: corrupt data");
    }

    #[test]
    fn test_content_error_conversion() {
        let content_err = ContentError::ContentTooLarge(100, 50);
        let mailbox_err: MailboxError = content_err.into();
        assert!(mailbox_err.to_string().contains("content error"));
    }
}
