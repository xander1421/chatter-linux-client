//! Notification service.
//!
//! Privacy-aware notification delivery across platforms:
//! - **Desktop (Linux/Windows/macOS)**: notify-rust → OS notification daemon
//! - **Android**: JNI → NotificationManager.notify()
//!
//! # Privacy Levels (Signal Desktop approach)
//!
//! | Level              | Title               | Body                | Metadata leaked |
//! |--------------------|---------------------|---------------------|-----------------|
//! | `none` (default)   | "Chatter"           | "New message"       | Nothing         |
//! | `name_only`        | "Alice"             | "New message"       | Sender identity |
//! | `name_and_message` | "Alice"             | "hey are you free?" | Sender + content|
//!
//! For sealed sender / anonymous messages, sender is NEVER shown regardless of
//! detail level — that would defeat the protocol's privacy guarantees.

use crate::domain::notification::{Notification, NotificationPriority};
use crate::error::SystemError;
use crate::network::streaming::push_listener::PushEvent;
#[cfg(not(target_os = "android"))]
use crate::system::notifications::DesktopNotifications;
#[cfg(target_os = "android")]
use crate::system::android_notifications::AndroidNotifications;

/// How much detail to show in OS notifications.
///
/// Default: `None` — maximum privacy, same as Signal Desktop default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationDetailLevel {
    /// "Chatter: New message" — no sender, no content.
    /// Nothing leaks to OS notification system.
    None,
    /// "Alice: New message" — sender name shown, no content.
    /// Sender identity leaks to OS.
    NameOnly,
    /// "Alice: hey are you free?" — sender name + message preview.
    /// Sender identity + message content leak to OS.
    NameAndMessage,
}

impl NotificationDetailLevel {
    /// Parse from settings string value.
    pub fn from_str(s: &str) -> Self {
        match s {
            "name_only" => Self::NameOnly,
            "name_and_message" => Self::NameAndMessage,
            _ => Self::None,
        }
    }

    /// Convert to settings string value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NameOnly => "name_only",
            Self::NameAndMessage => "name_and_message",
        }
    }
}

/// Notification service — privacy-aware cross-platform notification delivery.
pub struct NotificationService {
    #[cfg(not(target_os = "android"))]
    desktop: DesktopNotifications,
    #[cfg(target_os = "android")]
    android: AndroidNotifications,
    detail_level: std::sync::RwLock<NotificationDetailLevel>,
}

impl NotificationService {
    /// Create a new notification service with default privacy (no detail).
    pub fn new() -> Self {
        Self {
            #[cfg(not(target_os = "android"))]
            desktop: DesktopNotifications::new(),
            #[cfg(target_os = "android")]
            android: AndroidNotifications::new(),
            detail_level: std::sync::RwLock::new(NotificationDetailLevel::None),
        }
    }

    /// Set the notification detail level.
    pub fn set_detail_level(&self, level: NotificationDetailLevel) {
        *self.detail_level.write().unwrap() = level;
    }

    /// Get the current detail level.
    pub fn detail_level(&self) -> NotificationDetailLevel {
        *self.detail_level.read().unwrap()
    }

    /// Show a notification for an incoming message.
    ///
    /// Respects privacy settings and sealed sender status.
    pub fn show_message_notification(
        &self,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        is_sealed_sender: bool,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let level = self.detail_level();

        let (title, body) = match (level, is_sealed_sender) {
            // Sealed sender: NEVER show sender, regardless of setting
            (_, true) => ("Chatter".to_string(), "New message".to_string()),
            // None: maximum privacy
            (NotificationDetailLevel::None, false) => ("Chatter".to_string(), "New message".to_string()),
            // Name only: show sender, hide content
            (NotificationDetailLevel::NameOnly, false) => (
                sender_name.unwrap_or("Someone").to_string(),
                "New message".to_string(),
            ),
            // Name and message: show both
            (NotificationDetailLevel::NameAndMessage, false) => (
                sender_name.unwrap_or("Someone").to_string(),
                message_preview.unwrap_or("New message").to_string(),
            ),
        };

        self.show_platform_notification(&title, &body, priority)
    }

    /// Show a notification from a domain Notification object.
    pub fn show(&self, notification: &Notification) -> Result<(), SystemError> {
        let level = self.detail_level();

        match level {
            NotificationDetailLevel::None => {
                self.show_platform_notification("Chatter", "New notification", NotificationPriority::Normal)
            }
            _ => {
                #[cfg(not(target_os = "android"))]
                { self.desktop.show(notification) }
                #[cfg(target_os = "android")]
                {
                    self.android.show(&notification.title, &notification.body, 3)
                }
            }
        }
    }

    /// Show a notification from a push event (ntfy SSE).
    pub fn show_push_event(&self, event: &PushEvent) -> Result<(), SystemError> {
        let priority = match event.priority {
            5 => NotificationPriority::Urgent,
            4 => NotificationPriority::High,
            2 => NotificationPriority::Low,
            _ => NotificationPriority::Normal,
        };

        self.show_platform_notification(&event.title, &event.message, priority)
    }

    /// Show a simple text notification (caller controls content).
    pub fn show_message(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show_platform_notification(title, body, priority)
    }

    /// Platform-specific notification display.
    /// Desktop: notify-rust → D-Bus/WinRT
    /// Android: JNI → NotificationManager
    fn show_platform_notification(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        #[cfg(not(target_os = "android"))]
        {
            self.desktop.show_message(title, body, priority)
        }

        #[cfg(target_os = "android")]
        {
            let android_priority = match priority {
                NotificationPriority::Urgent => 5,
                NotificationPriority::High => 4,
                NotificationPriority::Low => 2,
                NotificationPriority::Normal => 3,
            };
            self.android.show(title, body, android_priority)
        }
    }
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_detail_level_is_none() {
        let service = NotificationService::new();
        assert_eq!(service.detail_level(), NotificationDetailLevel::None);
    }

    #[test]
    fn test_set_detail_level() {
        let service = NotificationService::new();
        service.set_detail_level(NotificationDetailLevel::NameAndMessage);
        assert_eq!(service.detail_level(), NotificationDetailLevel::NameAndMessage);
    }

    #[test]
    fn test_detail_level_from_str() {
        assert_eq!(NotificationDetailLevel::from_str("none"), NotificationDetailLevel::None);
        assert_eq!(NotificationDetailLevel::from_str("name_only"), NotificationDetailLevel::NameOnly);
        assert_eq!(NotificationDetailLevel::from_str("name_and_message"), NotificationDetailLevel::NameAndMessage);
        assert_eq!(NotificationDetailLevel::from_str("garbage"), NotificationDetailLevel::None);
        assert_eq!(NotificationDetailLevel::from_str(""), NotificationDetailLevel::None);
    }

    #[test]
    fn test_detail_level_as_str() {
        assert_eq!(NotificationDetailLevel::None.as_str(), "none");
        assert_eq!(NotificationDetailLevel::NameOnly.as_str(), "name_only");
        assert_eq!(NotificationDetailLevel::NameAndMessage.as_str(), "name_and_message");
    }

    #[test]
    fn test_detail_level_roundtrip() {
        for level in [NotificationDetailLevel::None, NotificationDetailLevel::NameOnly, NotificationDetailLevel::NameAndMessage] {
            assert_eq!(NotificationDetailLevel::from_str(level.as_str()), level);
        }
    }
}
