//! Notification domain entity.
//!
//! Represents notification types and payloads.

use crate::domain::ids::{CallId, ChannelId, MessageId, NotificationId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Notification
// ============================================================================

/// Represents a notification.
///
/// Notifications inform users about events like new messages, mentions,
/// friend requests, and calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub notification_type: NotificationType,
    pub title: String,
    pub body: String,
    pub is_read: bool,
    pub created_at: DateTime<Utc>,
}

impl Notification {
    /// Create a new unread notification with the given type, title, and body.
    #[must_use]
    pub fn new(notification_type: NotificationType, title: String, body: String) -> Self {
        Self {
            id: NotificationId::new(),
            notification_type,
            title,
            body,
            is_read: false,
            created_at: Utc::now(),
        }
    }

    /// Mark this notification as read.
    pub fn mark_read(&mut self) {
        self.is_read = true;
    }

    /// Check if this notification requires user action.
    ///
    /// Returns true for friend requests, channel invites, and incoming calls.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        matches!(
            self.notification_type,
            NotificationType::FriendRequest { .. }
                | NotificationType::ChannelInvite { .. }
                | NotificationType::Call {
                    call_type: CallNotificationType::Incoming,
                    ..
                }
        )
    }

    /// Extract the channel ID if this notification is related to a channel.
    #[must_use]
    pub fn channel_id(&self) -> Option<ChannelId> {
        match &self.notification_type {
            NotificationType::Message { channel_id, .. }
            | NotificationType::Mention { channel_id, .. }
            | NotificationType::ChannelInvite { channel_id, .. } => Some(*channel_id),
            _ => None,
        }
    }
}

// ============================================================================
// Notification Type
// ============================================================================

/// Notification type variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NotificationType {
    /// New message notification.
    Message {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    /// Mention notification.
    Mention {
        channel_id: ChannelId,
        message_id: MessageId,
        mentioned_by: UserId,
    },
    /// Reaction notification.
    Reaction {
        message_id: MessageId,
        emoji: String,
        reacted_by: UserId,
    },
    /// Call notification.
    Call {
        call_id: CallId,
        caller_id: UserId,
        call_type: CallNotificationType,
    },
    /// Friend request notification.
    FriendRequest { from_user_id: UserId },
    /// Friend accepted notification.
    FriendAccepted { user_id: UserId },
    /// Channel invite notification.
    ChannelInvite {
        channel_id: ChannelId,
        invited_by: UserId,
    },
    /// System alert notification.
    SystemAlert { title: String, body: String },
}

// ============================================================================
// Call Notification Type
// ============================================================================

/// Call notification type variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallNotificationType {
    /// Incoming call.
    Incoming,
    /// Missed call.
    Missed,
    /// Declined call.
    Declined,
}

// ============================================================================
// Notification Priority
// ============================================================================

/// Notification priority for sound and display handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotificationPriority {
    /// Low priority (silent, no vibration).
    Low,
    /// Normal priority (standard notification).
    #[default]
    Normal,
    /// High priority (prominent sound, vibration).
    High,
    /// Urgent priority (persistent alert until dismissed).
    Urgent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_new() {
        let channel_id = ChannelId::new();
        let message_id = MessageId::new();
        let notification = Notification::new(
            NotificationType::Message {
                channel_id,
                message_id,
            },
            "New Message".to_string(),
            "You have a new message".to_string(),
        );

        assert!(!notification.is_read);
        assert_eq!(notification.title, "New Message");
        assert_eq!(notification.body, "You have a new message");
    }

    #[test]
    fn test_notification_mark_read() {
        let mut notification = Notification::new(
            NotificationType::SystemAlert {
                title: "Test".to_string(),
                body: "Test body".to_string(),
            },
            "System Alert".to_string(),
            "Test alert".to_string(),
        );

        assert!(!notification.is_read);
        notification.mark_read();
        assert!(notification.is_read);
    }

    #[test]
    fn test_notification_is_actionable() {
        // Friend request is actionable
        let friend_request = Notification::new(
            NotificationType::FriendRequest {
                from_user_id: UserId::new(),
            },
            "Friend Request".to_string(),
            "Someone wants to be your friend".to_string(),
        );
        assert!(friend_request.is_actionable());

        // Channel invite is actionable
        let channel_invite = Notification::new(
            NotificationType::ChannelInvite {
                channel_id: ChannelId::new(),
                invited_by: UserId::new(),
            },
            "Channel Invite".to_string(),
            "You've been invited".to_string(),
        );
        assert!(channel_invite.is_actionable());

        // Incoming call is actionable
        let incoming_call = Notification::new(
            NotificationType::Call {
                call_id: CallId::new(),
                caller_id: UserId::new(),
                call_type: CallNotificationType::Incoming,
            },
            "Incoming Call".to_string(),
            "Someone is calling".to_string(),
        );
        assert!(incoming_call.is_actionable());

        // Missed call is NOT actionable
        let missed_call = Notification::new(
            NotificationType::Call {
                call_id: CallId::new(),
                caller_id: UserId::new(),
                call_type: CallNotificationType::Missed,
            },
            "Missed Call".to_string(),
            "You missed a call".to_string(),
        );
        assert!(!missed_call.is_actionable());

        // Regular message is NOT actionable
        let message = Notification::new(
            NotificationType::Message {
                channel_id: ChannelId::new(),
                message_id: MessageId::new(),
            },
            "New Message".to_string(),
            "You have a new message".to_string(),
        );
        assert!(!message.is_actionable());
    }

    #[test]
    fn test_notification_channel_id() {
        let channel_id = ChannelId::new();

        // Message notification has channel_id
        let message = Notification::new(
            NotificationType::Message {
                channel_id,
                message_id: MessageId::new(),
            },
            "Test".to_string(),
            "Test".to_string(),
        );
        assert_eq!(message.channel_id(), Some(channel_id));

        // Mention notification has channel_id
        let mention = Notification::new(
            NotificationType::Mention {
                channel_id,
                message_id: MessageId::new(),
                mentioned_by: UserId::new(),
            },
            "Test".to_string(),
            "Test".to_string(),
        );
        assert_eq!(mention.channel_id(), Some(channel_id));

        // Channel invite has channel_id
        let invite = Notification::new(
            NotificationType::ChannelInvite {
                channel_id,
                invited_by: UserId::new(),
            },
            "Test".to_string(),
            "Test".to_string(),
        );
        assert_eq!(invite.channel_id(), Some(channel_id));

        // Friend request has no channel_id
        let friend = Notification::new(
            NotificationType::FriendRequest {
                from_user_id: UserId::new(),
            },
            "Test".to_string(),
            "Test".to_string(),
        );
        assert_eq!(friend.channel_id(), None);

        // Call has no channel_id
        let call = Notification::new(
            NotificationType::Call {
                call_id: CallId::new(),
                caller_id: UserId::new(),
                call_type: CallNotificationType::Incoming,
            },
            "Test".to_string(),
            "Test".to_string(),
        );
        assert_eq!(call.channel_id(), None);
    }

    #[test]
    fn test_notification_priority_default() {
        let priority = NotificationPriority::default();
        assert_eq!(priority, NotificationPriority::Normal);
    }

    #[test]
    fn test_call_notification_type_variants() {
        let types = [
            CallNotificationType::Incoming,
            CallNotificationType::Missed,
            CallNotificationType::Declined,
        ];

        assert_eq!(types.len(), 3);
        assert_ne!(CallNotificationType::Incoming, CallNotificationType::Missed);
    }
}
