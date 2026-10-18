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
//! For sealed sender messages and anything in a secret chat (anonymous DM or
//! group) — messages and reactions alike — the notification is always the
//! generic one: no sender, no text, no emoji, whatever the detail level. See
//! [`listener::is_private_notification`].
mod account_keys;
mod listener;

pub use account_keys::is_opaque_notification_key;
#[cfg(feature = "mesh")]
pub use listener::spawn_mesh_notification_listener;
pub use listener::spawn_notification_listener;

use crate::domain::notification::{Notification, NotificationPriority};
use crate::error::SystemError;
use crate::system::notification_sink::{NoopNotificationSink, NotificationSink};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

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
    account_keys: Option<AccountNotificationKeys>,
    message_notifications: Mutex<HashMap<String, Vec<MessageNotification>>>,
}

struct MessageNotification {
    id: crate::domain::ids::MessageId,
    sender_name: Option<String>,
    message_preview: Option<String>,
    private: bool,
    priority: NotificationPriority,
}

struct AccountNotificationKeys {
    settings: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    auth: crate::state::AuthState,
    creation_lock: Mutex<()>,
}

impl NotificationService {
    /// Create a notification service with the given platform sink.
    pub fn new(sink: Arc<dyn NotificationSink>) -> Self {
        Self {
            sink,
            detail_level: std::sync::RwLock::new(NotificationDetailLevel::None),
            account_keys: None,
            message_notifications: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_account_keys(
        sink: Arc<dyn NotificationSink>,
        settings: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        auth: crate::state::AuthState,
    ) -> Self {
        let mut service = Self::new(sink);
        service.account_keys = Some(AccountNotificationKeys {
            settings,
            auth,
            creation_lock: Mutex::new(()),
        });
        service
    }

    /// Create a notification service with a no-op sink (tests / headless).
    pub fn noop() -> Self {
        Self::new(Arc::new(NoopNotificationSink))
    }

    /// Set the notification detail level.
    pub fn set_detail_level(&self, level: NotificationDetailLevel) {
        *self.detail_level.write().unwrap() = level;
    }

    /// Apply the notification-sound preference to the platform sink.
    pub fn set_sound_enabled(&self, enabled: bool) {
        self.sink.set_sound(enabled);
    }

    /// Get the current detail level.
    pub fn detail_level(&self) -> NotificationDetailLevel {
        *self.detail_level.read().unwrap()
    }

    /// Show `conversation_id`'s notification for an incoming message,
    /// replacing the conversation's earlier one.
    ///
    /// Respects privacy settings; `private` (see [`listener::is_private_notification`])
    /// forces the generic notification at every level.
    pub fn show_message_notification(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let (title, body) = self.message_text(sender_name, message_preview, private);
        let key = self.current_key(conversation_id)?;
        let mut notifications = self
            .message_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.deliver(|sink| sink.show_conversation(&key, &title, &body, priority))?;
        // Requests and reactions replace this conversation's OS notification.
        // Its earlier message entries can no longer be restored.
        notifications.remove(&key);
        Ok(())
    }

    fn message_text(
        &self,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
    ) -> (String, String) {
        let level = self.detail_level();

        match (level, private) {
            // Sealed sender / secret chat: nothing but "New message", ever
            (_, true) => ("Chatter".to_string(), "New message".to_string()),
            // None: maximum privacy
            (NotificationDetailLevel::None, false) => {
                ("Chatter".to_string(), "New message".to_string())
            }
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
        }
    }

    /// Post an incoming message and retain enough state to restore the
    /// preceding notification if this message disappears. Display text is
    /// filtered against the current privacy setting each time it is posted.
    pub fn show_tracked_message_notification(
        &self,
        message_id: crate::domain::ids::MessageId,
        conversation_id: &crate::domain::ids::ChannelId,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let key = self.current_key(conversation_id)?;
        let mut notifications = self
            .message_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (title, body) = self.message_text(sender_name, message_preview, private);
        self.deliver(|sink| sink.show_conversation(&key, &title, &body, priority))?;
        let entries = notifications.entry(key).or_default();
        entries.retain(|entry| entry.id != message_id);
        entries.push(MessageNotification {
            id: message_id,
            sender_name: sender_name.map(str::to_owned),
            message_preview: message_preview.map(str::to_owned),
            private,
            priority,
        });
        Ok(())
    }

    /// Remove deleted or expired messages from whichever conversations posted
    /// them. A prior message remains visible when the newest one is removed.
    pub fn remove_message_notifications(&self, message_ids: &[crate::domain::ids::MessageId]) {
        let mut notifications = self
            .message_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (key, entries) in notifications.iter_mut() {
            let was_latest = entries
                .last()
                .is_some_and(|entry| message_ids.contains(&entry.id));
            let old_len = entries.len();
            entries.retain(|entry| !message_ids.contains(&entry.id));
            if entries.len() == old_len || !was_latest {
                continue;
            }
            let result = if let Some(entry) = entries.last() {
                let (title, body) = self.message_text(
                    entry.sender_name.as_deref(),
                    entry.message_preview.as_deref(),
                    entry.private,
                );
                self.deliver(|sink| sink.show_conversation(key, &title, &body, entry.priority))
            } else {
                self.deliver(|sink| sink.cancel_conversation(key))
            };
            if let Err(error) = result {
                tracing::warn!(%error, "Could not update notification for removed message");
            }
        }
        notifications.retain(|_, entries| !entries.is_empty());
    }

    /// Dismiss `conversation_id`'s notification (it was read).
    pub fn cancel_conversation(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), SystemError> {
        let key = self.current_key(conversation_id)?;
        let mut notifications = self
            .message_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.deliver(|sink| sink.cancel_conversation(&key))?;
        notifications.remove(&key);
        Ok(())
    }

    /// Show a notification from a domain Notification object.
    pub fn show(&self, notification: &Notification) -> Result<(), SystemError> {
        let level = self.detail_level();

        match level {
            NotificationDetailLevel::None => self.show_platform_notification(
                "Chatter",
                "New notification",
                NotificationPriority::Normal,
            ),
            _ => self.deliver(|sink| sink.show_domain(notification)),
        }
    }

    // `show_push_event` was removed alongside push_listener's
    // show_push_notification. Rationale: ntfy SSE is a wake-up transport
    // only; user-facing toasts flow via DomainEvent from the typed gRPC
    // stream consumer (see services::notification_stream) so server can
    // run ntfy in content-free mode without losing notification
    // fidelity.

    /// Show a simple text notification (caller controls content).
    pub fn show_message(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show_platform_notification(title, body, priority)
    }

    /// Show an actionable incoming-call notification (Accept/Decline) for
    /// `call_id`. On Android this is a full-screen ringing notification with
    /// buttons; elsewhere it falls back to a plain urgent toast.
    pub fn show_incoming_call(
        &self,
        call_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), SystemError> {
        match self.detail_level() {
            NotificationDetailLevel::None => {
                self.deliver(|sink| sink.show_incoming_call(call_id, "Chatter", "Incoming call"))
            }
            _ => self.deliver(|sink| sink.show_incoming_call(call_id, title, body)),
        }
    }

    /// Dismiss the incoming-call notification for `call_id` (accepted,
    /// declined, withdrawn, or ended).
    pub fn cancel_incoming_call(&self, call_id: &str) -> Result<(), SystemError> {
        self.deliver(|sink| sink.cancel_incoming_call(call_id))
    }

    /// Platform-neutral notification display via the injected sink.
    fn show_platform_notification(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.deliver(|sink| sink.show(title, body, priority))
    }

    /// Every sink call goes through here. The sinks are long-lived event
    /// loops' only contact with platform code (JNI on Android), and a panic
    /// there used to end the loop: one call whose id overflowed a request-code
    /// multiply killed the ring-notification bridge, so every later call rang
    /// silently. A panic is one failed notification, reported as an error.
    fn deliver(
        &self,
        f: impl FnOnce(&dyn NotificationSink) -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self.sink.as_ref())))
            .unwrap_or_else(|_| {
                Err(SystemError::NotificationSend(
                    "notification sink panicked".into(),
                ))
            })
    }
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::noop()
    }
}

#[cfg(test)]
mod tests;
