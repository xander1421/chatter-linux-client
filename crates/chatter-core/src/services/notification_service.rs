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
    /// Respects privacy settings and sealed sender status.
    pub fn show_message_notification(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
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
        };

        self.deliver(|sink| {
            sink.show_conversation(&notification_key(conversation_id), &title, &body, priority)
        })
    }

    /// Dismiss `conversation_id`'s notification (it was read).
    pub fn cancel_conversation(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), SystemError> {
        self.deliver(|sink| sink.cancel_conversation(&notification_key(conversation_id)))
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
        self.deliver(|sink| sink.show_incoming_call(call_id, title, body))
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

/// The key a conversation's notification is filed under. Not the id itself:
/// the platform can hand the key to other apps (an Android tag is visible to
/// every notification listener), and a friend DM's id IS the peer's user id —
/// a contact identifier leaking whatever the detail level. A hash still lets
/// the same conversation replace and cancel its notification.
fn notification_key(conversation_id: &crate::domain::ids::ChannelId) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(b"chatter-notification-key:")
        .chain_update(conversation_id.to_string())
        .finalize();
    hex::encode(&digest[..16])
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::noop()
    }
}

/// Spawn a task that listens to the domain event bus and dispatches user-
/// visible notifications via `NotificationService`.
///
/// Filter policy lives here (not in the UI / client facade) — see
/// [`should_notify_message`].
/// * Show reactions from other users as low-priority notifications.
///
/// "Who am I" comes from the settings-backed `UserService::current_user_id`,
/// NOT `AuthState`: the device-link bring-up never calls
/// `AuthState::set_logged_in`, so on a freshly linked device `AuthState` says
/// nobody is logged in and every self-synced copy of our own send (SyncSent)
/// used to raise a notification (F11).
///
/// Task exits when the event bus closes; returns the `JoinHandle` for the
/// caller to track alongside other lifecycle tasks.
pub fn spawn_notification_listener(
    event_bus: crate::events::EventBus,
    // Weak: the client owns this task, so a strong handle would keep the
    // client alive forever. Used for the per-conversation rules — display
    // name in context, mute, what is on screen.
    client: std::sync::Weak<crate::client::ChatterClient>,
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

            let me = user_service.current_user_id().ok();

            match &event {
                DomainEvent::MessageReceived { message } => {
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    if let Err(error) =
                        crate::services::conversation::ConversationHandle::mark_received_as_read(
                            &client, message,
                        )
                        .await
                    {
                        tracing::warn!(%error, "Failed to mark visible incoming message read");
                    }
                    let (on_screen, muted) = conversation_attention(&client, message);
                    if !should_notify_message(message, me.as_ref(), on_screen, muted) {
                        continue;
                    }

                    // In an anon group the per-group name, never the global
                    // profile name of a contact who happens to be in it.
                    let sender_name = message.sender_id.map(|id| {
                        client.display_name_in(
                            &message.conversation_type,
                            &message.conversation_id,
                            &id,
                        )
                    });
                    let preview = message.as_text();

                    let _ = notification_service.show_message_notification(
                        &message.conversation_id,
                        sender_name.as_deref(),
                        preview,
                        message.is_sealed_sender(),
                        NotificationPriority::Normal,
                    );
                }
                DomainEvent::ReactionAdded {
                    sender_id,
                    sent_at_ns,
                    emoji,
                    reactor_id,
                    ..
                } => {
                    if me.is_none() || Some(*reactor_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    // Same mute / on-screen rules as the message it reacts
                    // to. A target we do not hold names no conversation to
                    // check, so it stays silent.
                    let Some(target) =
                        client.get_message_by_sender_timestamp(sender_id, *sent_at_ns)
                    else {
                        continue;
                    };
                    let (on_screen, muted) = conversation_attention(&client, &target);
                    if on_screen || muted {
                        continue;
                    }
                    let _ = notification_service.show_message_notification(
                        &target.conversation_id,
                        None,
                        Some(&format!("Reacted {}", emoji)),
                        false,
                        NotificationPriority::Low,
                    );
                }

                // Read here or on another of our devices: the notification is
                // stale (Signal cancels a thread's notification either way).
                DomainEvent::ConversationsRead {
                    conversation_ids, ..
                } => {
                    for id in conversation_ids {
                        let _ = notification_service.cancel_conversation(id);
                    }
                }

                // ================================================================
                // Typed lifecycle events (from gRPC notification stream).
                // Side-effects run in spawned tasks so the event loop stays hot.
                // ================================================================
                DomainEvent::DeviceLinked {
                    device_id,
                    device_name,
                    device_info,
                    approved_by_self,
                } => {
                    // Skip the toast on the device that just approved the
                    // pairing — the UI there already showed a confirm
                    // screen. Only fan-out to siblings raises a toast.
                    if *approved_by_self {
                        continue;
                    }
                    let body = match (device_name.as_deref(), device_info.as_deref()) {
                        (Some(name), _) if !name.is_empty() => {
                            format!("\"{}\" is now linked to your account.", name)
                        }
                        (_, Some(info)) if !info.is_empty() => {
                            format!("A {} was just linked to your account.", info)
                        }
                        _ => "A new device was just linked to your account.".to_string(),
                    };
                    tracing::info!(device_id, "DeviceLinked notification → surfacing toast");
                    let _ = notification_service.show_message(
                        "New device linked",
                        &body,
                        NotificationPriority::High,
                    );
                }

                DomainEvent::PeerDevicesChanged {
                    peer_user_id,
                    device_id,
                    change_type,
                } => {
                    // Per Signal: an added device of a staying member needs
                    // only our SKDM, a removed one no key reset — see
                    // `MessageService::handle_peer_device_change`.
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    let messages = client.messages().clone();
                    let (peer, device, change) = (*peer_user_id, *device_id, change_type.clone());
                    tokio::spawn(async move {
                        messages
                            .handle_peer_device_change(peer, device, &change)
                            .await;
                    });
                }

                _ => {}
            }
        }
    })
}

/// Whether a `MessageReceived` should raise an OS notification.
///
/// * Never for our own message — every path that files one of ours (live
///   channel echo, SyncSent from our other device, local call-history rows)
///   stamps `sender_id` with our id.
/// * Never when we cannot say who we are (`me == None`): the self-check would
///   be a no-op, and a toast for your own message is the worse failure.
/// * Never for the conversation on screen (open AND the app visible — an open
///   chat behind the lock screen still notifies).
/// * Never for a muted conversation.
/// * Never for a conversation notice (a timer change and the like): it
///   records something about the chat, it is not a message to read.
/// * Never for a Call record: the call itself rang, and a missed one is on
///   the Calls screen.
fn should_notify_message(
    message: &crate::domain::message::Message,
    me: Option<&crate::domain::ids::UserId>,
    on_screen: bool,
    muted: bool,
) -> bool {
    let Some(me) = me else { return false };
    message.sender_id.as_ref() != Some(me)
        && !on_screen
        && !muted
        && !message.content.is_system()
        && !matches!(
            message.content,
            crate::domain::message::MessageContent::Call(_)
        )
}

/// `(on_screen, muted)` for the conversation `message` belongs to. On screen
/// is the same "is the user looking at this" rule that marks messages read:
/// the app is visible AND `is_showing` (which also matches a mailbox DM in
/// its peer's DM view — the old `is_for_channel` check notified for those).
fn conversation_attention(
    client: &std::sync::Arc<crate::client::ChatterClient>,
    message: &crate::domain::message::Message,
) -> (bool, bool) {
    let on_screen = client.chat_state().is_app_visible() && client.is_showing(message);
    let conversation = client.classify_active_conversation(
        &message.conversation_id,
        &message.conversation_type,
        message.sender_id,
    );
    let muted = client.is_conversation_muted(conversation).unwrap_or(false);
    (on_screen, muted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_message_never_notifies() {
        use crate::domain::ids::{ChannelId, UserId};
        use crate::domain::message::{ConversationType, Message};

        let me = UserId::new();
        let peer = UserId::new();
        let msg = |sender| {
            Message::new_text(
                ChannelId::new(),
                ConversationType::ServerDm,
                Some(sender),
                "hi".into(),
                1,
            )
        };

        // Ours: silent whatever else holds.
        assert!(!should_notify_message(&msg(me), Some(&me), false, false));
        // Identity unknown (e.g. AuthState never set): fail closed.
        assert!(!should_notify_message(&msg(peer), None, false, false));
        // A peer's message in a background chat notifies…
        let m = msg(peer);
        assert!(should_notify_message(&m, Some(&me), false, false));
        // …but not in the chat on screen, nor in a muted one.
        assert!(!should_notify_message(&m, Some(&me), true, false));
        assert!(!should_notify_message(&m, Some(&me), false, true));
        // A call record filed under the peer is not a message to read.
        let mut call = msg(peer);
        call.content = crate::domain::message::MessageContent::Call(
            crate::domain::message::CallMessage::started("c".into(), peer, "Peer".into()),
        );
        assert!(!should_notify_message(&call, Some(&me), false, false));
    }

    #[test]
    fn test_default_detail_level_is_none() {
        let service = NotificationService::noop();
        assert_eq!(service.detail_level(), NotificationDetailLevel::None);
    }

    #[test]
    fn test_set_detail_level() {
        let service = NotificationService::noop();
        service.set_detail_level(NotificationDetailLevel::NameAndMessage);
        assert_eq!(
            service.detail_level(),
            NotificationDetailLevel::NameAndMessage
        );
    }

    #[test]
    fn test_detail_level_from_str() {
        assert_eq!(
            NotificationDetailLevel::from_str("none"),
            NotificationDetailLevel::None
        );
        assert_eq!(
            NotificationDetailLevel::from_str("name_only"),
            NotificationDetailLevel::NameOnly
        );
        assert_eq!(
            NotificationDetailLevel::from_str("name_and_message"),
            NotificationDetailLevel::NameAndMessage
        );
        assert_eq!(
            NotificationDetailLevel::from_str("garbage"),
            NotificationDetailLevel::None
        );
        assert_eq!(
            NotificationDetailLevel::from_str(""),
            NotificationDetailLevel::None
        );
    }

    #[test]
    fn test_detail_level_as_str() {
        assert_eq!(NotificationDetailLevel::None.as_str(), "none");
        assert_eq!(NotificationDetailLevel::NameOnly.as_str(), "name_only");
        assert_eq!(
            NotificationDetailLevel::NameAndMessage.as_str(),
            "name_and_message"
        );
    }

    /// The level decides what reaches the platform sink (desktop notify-rust
    /// and Android JNI both render title/body verbatim), and a sealed-sender
    /// message never names a sender whatever the level.
    #[test]
    fn detail_level_shapes_what_reaches_the_sink() {
        struct Capture(std::sync::Mutex<Vec<(String, String)>>);
        impl NotificationSink for Capture {
            fn show(
                &self,
                title: &str,
                body: &str,
                _: NotificationPriority,
            ) -> Result<(), SystemError> {
                self.0.lock().unwrap().push((title.into(), body.into()));
                Ok(())
            }
        }
        let sink = Arc::new(Capture(Default::default()));
        let service = NotificationService::new(sink.clone());
        let conversation = crate::domain::ids::ChannelId::new();
        let shown = |level, sealed| {
            service.set_detail_level(level);
            service
                .show_message_notification(
                    &conversation,
                    Some("Alice"),
                    Some("are you free?"),
                    sealed,
                    NotificationPriority::Normal,
                )
                .unwrap();
            sink.0.lock().unwrap().pop().unwrap()
        };
        let pair = |t: &str, b: &str| (t.to_string(), b.to_string());

        assert_eq!(
            shown(NotificationDetailLevel::NameAndMessage, false),
            pair("Alice", "are you free?")
        );
        assert_eq!(
            shown(NotificationDetailLevel::NameOnly, false),
            pair("Alice", "New message")
        );
        assert_eq!(
            shown(NotificationDetailLevel::None, false),
            pair("Chatter", "New message")
        );
        assert_eq!(
            shown(NotificationDetailLevel::NameAndMessage, true),
            pair("Chatter", "New message")
        );
    }

    /// A panicking sink is one failed notification, not a dead caller loop.
    #[test]
    fn a_panicking_sink_is_an_error_not_a_panic() {
        struct Panics;
        impl NotificationSink for Panics {
            fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
                Ok(())
            }
            fn show_incoming_call(&self, _: &str, _: &str, _: &str) -> Result<(), SystemError> {
                panic!("attempt to multiply with overflow")
            }
        }
        let service = NotificationService::new(Arc::new(Panics));
        assert!(service.show_incoming_call("c", "t", "b").is_err());
        assert!(
            service
                .show_message("t", "b", NotificationPriority::Normal)
                .is_ok()
        );
    }

    #[test]
    fn notification_key_is_stable_and_hides_the_id() {
        let id = crate::domain::ids::ChannelId::new();
        assert_eq!(notification_key(&id), notification_key(&id));
        assert_ne!(
            notification_key(&id),
            notification_key(&crate::domain::ids::ChannelId::new())
        );
        assert!(!notification_key(&id).contains(&id.to_string()));
    }

    #[test]
    fn test_detail_level_roundtrip() {
        for level in [
            NotificationDetailLevel::None,
            NotificationDetailLevel::NameOnly,
            NotificationDetailLevel::NameAndMessage,
        ] {
            assert_eq!(NotificationDetailLevel::from_str(level.as_str()), level);
        }
    }
}
