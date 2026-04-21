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

use std::sync::Arc;

use crate::domain::notification::{Notification, NotificationPriority};
use crate::error::SystemError;
use crate::network::streaming::push_listener::PushEvent;
use crate::system::notification_sink::{NoopNotificationSink, NotificationSink};

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
///
/// Business logic (privacy filtering, sealed-sender handling) lives here.
/// Platform delivery is delegated to a `NotificationSink` injected at construction.
pub struct NotificationService {
    sink: Arc<dyn NotificationSink>,
    detail_level: std::sync::RwLock<NotificationDetailLevel>,
}

impl NotificationService {
    /// Create a notification service with the given platform sink.
    pub fn new(sink: Arc<dyn NotificationSink>) -> Self {
        Self {
            sink,
            detail_level: std::sync::RwLock::new(NotificationDetailLevel::None),
        }
    }

    /// Create a notification service with a no-op sink (tests / headless).
    pub fn noop() -> Self {
        Self::new(Arc::new(NoopNotificationSink))
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
            _ => self.sink.show_domain(notification),
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

    /// Platform-neutral notification display via the injected sink.
    fn show_platform_notification(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.sink.show(title, body, priority)
    }
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::noop()
    }
}

/// Spawn a task that listens to the domain event bus and dispatches user-
/// visible notifications via `NotificationService`.
///
/// Filter policy lives here (not in the UI / client facade):
/// * Suppress messages the current user sent.
/// * Suppress messages for the currently active channel (user is looking
///   at them; notification would be noise).
/// * Show reactions from other users as low-priority notifications.
///
/// Task exits when the event bus closes; returns the `JoinHandle` for the
/// caller to track alongside other lifecycle tasks.
pub fn spawn_notification_listener(
    event_bus: crate::events::EventBus,
    auth_state: crate::state::AuthState,
    chat_state: crate::state::ChatState,
    user_service: std::sync::Arc<crate::services::user::UserService>,
    notification_service: std::sync::Arc<NotificationService>,
) -> tokio::task::JoinHandle<()> {
    use crate::domain::notification::NotificationPriority;
    use crate::inbox::handlers::DomainEvent;

    let mut domain_rx = event_bus.subscribe_domain();
    tokio::spawn(async move {
        loop {
            let event = match domain_rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Notification listener lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            let auth_data = auth_state.get();
            let chat_data = chat_state.get();

            match &event {
                DomainEvent::MessageReceived { message } => {
                    if let (Some(sender), Some(current)) =
                        (&message.sender_id, &auth_data.current_user_id)
                    {
                        if sender == current {
                            continue;
                        }
                    }
                    if chat_data.active_channel_id == Some(message.conversation_id) {
                        continue;
                    }

                    let sender_name = message
                        .sender_id
                        .map(|id| user_service.get_display_name(&id));
                    let preview = message.as_text();

                    let _ = notification_service.show_message_notification(
                        sender_name.as_deref(),
                        preview,
                        message.is_sealed_sender(),
                        NotificationPriority::Normal,
                    );
                }
                DomainEvent::ReactionAdded { emoji, reactor_id, .. } => {
                    if Some(*reactor_id) == auth_data.current_user_id {
                        continue;
                    }
                    let _ = notification_service.show_message_notification(
                        None,
                        Some(&format!("Reacted {}", emoji)),
                        false,
                        NotificationPriority::Low,
                    );
                }
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_detail_level_is_none() {
        let service = NotificationService::noop();
        assert_eq!(service.detail_level(), NotificationDetailLevel::None);
    }

    #[test]
    fn test_set_detail_level() {
        let service = NotificationService::noop();
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
