//! Desktop notifications.
//!
//! Desktop notifications using notify-rust.

use crate::domain::notification::{Notification, NotificationPriority, NotificationType};
use crate::error::SystemError;
use notify_rust::{Notification as DesktopNotif, NotificationHandle, Urgency};

/// Desktop notification service.
///
/// Wraps notify-rust to show system notifications.
/// Stateless - each notification is independent.
pub struct DesktopNotifications;

impl DesktopNotifications {
    /// Create a new desktop notifications service.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Show a notification from the domain model.
    ///
    /// Maps the domain Notification to a desktop notification with
    /// appropriate urgency and icon.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::NotificationSend` if the notification fails to show.
    pub fn show(&self, notification: &Notification) -> Result<(), SystemError> {
        let urgency = self.priority_to_urgency(self.type_to_priority(&notification.notification_type));
        let icon = self.type_to_icon(&notification.notification_type);

        DesktopNotif::new()
            .appname("chatter-linux")
            .summary(&notification.title)
            .body(&notification.body)
            .icon(icon)
            .urgency(urgency)
            .show()
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?;

        Ok(())
    }

    /// Show a generic notification with custom title, body, and priority.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::NotificationSend` if the notification fails to show.
    pub fn show_message(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let urgency = self.priority_to_urgency(priority);

        DesktopNotif::new()
            .appname("chatter-linux")
            .summary(title)
            .body(body)
            .icon("dialog-information")
            .urgency(urgency)
            .show()
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?;

        Ok(())
    }

    /// Show a notification with action buttons.
    ///
    /// Each action is a tuple of (action_id, action_label).
    /// Returns a handle that can be used to wait for action callbacks.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::NotificationSend` if the notification fails to show.
    pub fn show_with_actions(
        &self,
        title: &str,
        body: &str,
        actions: &[(&str, &str)],
    ) -> Result<NotificationHandle, SystemError> {
        let mut notif = DesktopNotif::new();
        notif
            .appname("chatter-linux")
            .summary(title)
            .body(body)
            .icon("dialog-information");

        for (action_id, action_label) in actions {
            notif.action(action_id, action_label);
        }

        notif
            .show()
            .map_err(|e| SystemError::NotificationSend(e.to_string()))
    }

    /// Map notification priority to notify-rust urgency.
    fn priority_to_urgency(&self, priority: NotificationPriority) -> Urgency {
        match priority {
            NotificationPriority::Low => Urgency::Low,
            NotificationPriority::Normal => Urgency::Normal,
            NotificationPriority::High | NotificationPriority::Urgent => Urgency::Critical,
        }
    }

    /// Map notification type to an appropriate XDG icon name.
    fn type_to_icon(&self, notification_type: &NotificationType) -> &'static str {
        match notification_type {
            NotificationType::Message { .. } | NotificationType::Mention { .. } => {
                "mail-message-new"
            }
            NotificationType::Reaction { .. } => "face-smile",
            NotificationType::Call { .. } => "call-start",
            NotificationType::FriendRequest { .. } | NotificationType::FriendAccepted { .. } => {
                "user-available"
            }
            NotificationType::ChannelInvite { .. } => "list-add",
            NotificationType::SystemAlert { .. } => "dialog-warning",
        }
    }

    /// Determine priority based on notification type.
    fn type_to_priority(&self, notification_type: &NotificationType) -> NotificationPriority {
        match notification_type {
            NotificationType::Call { .. } => NotificationPriority::Urgent,
            NotificationType::Mention { .. } | NotificationType::FriendRequest { .. } => {
                NotificationPriority::High
            }
            NotificationType::Message { .. }
            | NotificationType::Reaction { .. }
            | NotificationType::FriendAccepted { .. }
            | NotificationType::ChannelInvite { .. } => NotificationPriority::Normal,
            NotificationType::SystemAlert { .. } => NotificationPriority::High,
        }
    }
}

impl Default for DesktopNotifications {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_notifications_new() {
        let _notif = DesktopNotifications::new();
        // Construction should succeed
    }

    #[test]
    fn test_priority_to_urgency() {
        let notif = DesktopNotifications::new();

        assert!(matches!(
            notif.priority_to_urgency(NotificationPriority::Low),
            Urgency::Low
        ));
        assert!(matches!(
            notif.priority_to_urgency(NotificationPriority::Normal),
            Urgency::Normal
        ));
        assert!(matches!(
            notif.priority_to_urgency(NotificationPriority::High),
            Urgency::Critical
        ));
        assert!(matches!(
            notif.priority_to_urgency(NotificationPriority::Urgent),
            Urgency::Critical
        ));
    }

    #[test]
    fn test_type_to_icon() {
        use crate::domain::ids::{CallId, ChannelId, MessageId, UserId};
        use crate::domain::notification::CallNotificationType;

        let notif = DesktopNotifications::new();

        // Message -> mail icon
        let msg_type = NotificationType::Message {
            channel_id: ChannelId::new(),
            message_id: MessageId::new(),
        };
        assert_eq!(notif.type_to_icon(&msg_type), "mail-message-new");

        // Call -> call icon
        let call_type = NotificationType::Call {
            call_id: CallId::new(),
            caller_id: UserId::new(),
            call_type: CallNotificationType::Incoming,
        };
        assert_eq!(notif.type_to_icon(&call_type), "call-start");

        // Friend request -> user icon
        let friend_type = NotificationType::FriendRequest {
            from_user_id: UserId::new(),
        };
        assert_eq!(notif.type_to_icon(&friend_type), "user-available");

        // System alert -> warning icon
        let alert_type = NotificationType::SystemAlert {
            title: "Test".to_string(),
            body: "Test".to_string(),
        };
        assert_eq!(notif.type_to_icon(&alert_type), "dialog-warning");
    }
}
