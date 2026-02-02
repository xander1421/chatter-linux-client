//! Notification domain entity.
//!
//! Represents notification types and payloads.

/// Represents a notification.
pub struct Notification;

/// Notification type variants.
pub enum NotificationType {
    /// New message notification.
    Message,
    /// Mention notification.
    Mention,
    /// Call notification.
    Call,
    /// Friend request notification.
    FriendRequest,
    /// Channel invite notification.
    ChannelInvite,
}
