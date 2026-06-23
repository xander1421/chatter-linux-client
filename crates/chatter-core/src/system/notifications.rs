//! Desktop notifications.
//!
//! Desktop notifications using notify-rust.
//! - Linux: freedesktop.org notification spec
//! - Windows: WinRT toast notifications
//! - macOS: macOS notification center

use crate::domain::notification::{Notification, NotificationPriority, NotificationType};
use crate::error::SystemError;
use super::platform::APP_DISPLAY_NAME;
use notify_rust::Notification as DesktopNotif;

#[cfg(target_os = "linux")]
use notify_rust::Urgency;

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
    pub fn show(&self, notification: &Notification) -> Result<(), SystemError> {
        let mut notif = DesktopNotif::new();
        notif
            .appname(APP_DISPLAY_NAME)
            .summary(&notification.title)
            .body(&notification.body);

        #[cfg(target_os = "linux")]
        {
            let priority = self.type_to_priority(&notification.notification_type);
            let icon = self.type_to_icon(&notification.notification_type);
            notif.icon(icon);
            notif.urgency(self.priority_to_urgency(priority));
        }

        notif
            .show()
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?;

        Ok(())
    }

    /// Show a generic notification with custom title, body, and priority.
    pub fn show_message(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let mut notif = DesktopNotif::new();
        notif
            .appname(APP_DISPLAY_NAME)
            .summary(title)
            .body(body);

        #[cfg(target_os = "linux")]
        {
            notif.icon("dialog-information");
            notif.urgency(self.priority_to_urgency(priority));
        }

        // Suppress unused variable warning on non-Linux
        #[cfg(not(target_os = "linux"))]
        let _ = priority;

        notif
            .show()
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?;

        Ok(())
    }

    /// Show a notification with action buttons (Linux only, ignored on other platforms).
    #[cfg(target_os = "linux")]
    pub fn show_with_actions(
        &self,
        title: &str,
        body: &str,
        actions: &[(&str, &str)],
    ) -> Result<notify_rust::NotificationHandle, SystemError> {
        let mut notif = DesktopNotif::new();
        notif
            .appname(APP_DISPLAY_NAME)
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

    /// Map notification priority to notify-rust urgency (Linux only).
    #[cfg(target_os = "linux")]
    fn priority_to_urgency(&self, priority: NotificationPriority) -> Urgency {
        match priority {
            NotificationPriority::Low => Urgency::Low,
            NotificationPriority::Normal => Urgency::Normal,
            NotificationPriority::High | NotificationPriority::Urgent => Urgency::Critical,
        }
    }

    /// Map notification type to an appropriate XDG icon name (Linux only).
    #[cfg(target_os = "linux")]
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

impl super::notification_sink::NotificationSink for DesktopNotifications {
    fn show(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        DesktopNotifications::show_message(self, title, body, priority)
    }

    fn show_domain(&self, notification: &Notification) -> Result<(), SystemError> {
        DesktopNotifications::show(self, notification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_notifications_new() {
        let _notif = DesktopNotifications::new();
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn test_type_to_icon() {
        use crate::domain::ids::{CallId, ChannelId, MessageId, UserId};
        use crate::domain::notification::CallNotificationType;

        let notif = DesktopNotifications::new();

        let msg_type = NotificationType::Message {
            channel_id: ChannelId::new(),
            message_id: MessageId::new(),
        };
        assert_eq!(notif.type_to_icon(&msg_type), "mail-message-new");

        let call_type = NotificationType::Call {
            call_id: CallId::new(),
            caller_id: UserId::new(),
            call_type: CallNotificationType::Incoming,
        };
        assert_eq!(notif.type_to_icon(&call_type), "call-start");

        let friend_type = NotificationType::FriendRequest {
            from_user_id: UserId::new(),
        };
        assert_eq!(notif.type_to_icon(&friend_type), "user-available");

        let alert_type = NotificationType::SystemAlert {
            title: "Test".to_string(),
            body: "Test".to_string(),
        };
        assert_eq!(notif.type_to_icon(&alert_type), "dialog-warning");
    }
}
