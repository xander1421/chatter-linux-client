//! Message domain entity.
//!
//! Represents chat messages, threads, and message content.

/// Represents a chat message.
pub struct Message;

/// Message thread conversation.
pub struct Thread;

/// Message content types.
pub enum MessageContent {
    /// Plain text message.
    Text,
    /// Encrypted message content.
    Encrypted,
    /// System message (join, leave, etc).
    System,
    /// Call-related message.
    Call,
    /// AI-generated message.
    AI,
}

/// Encrypted message content.
pub struct EncryptedContent;

/// Encrypted message type variants.
pub enum EncryptedMessageType {
    /// Standard Signal protocol message.
    Signal,
    /// Sealed sender anonymous message.
    SealedSender,
    /// Sender key group message.
    SenderKey,
}
