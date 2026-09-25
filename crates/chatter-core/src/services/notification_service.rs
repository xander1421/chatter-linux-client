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
        self.sink.show_incoming_call(call_id, title, body)
    }

    /// Dismiss the incoming-call notification for `call_id` (accepted,
    /// declined, withdrawn, or ended).
    pub fn cancel_incoming_call(&self, call_id: &str) -> Result<(), SystemError> {
        self.sink.cancel_incoming_call(call_id)
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
    signal_service: std::sync::Arc<crate::services::signal_service::SignalService>,
    channel_repo: std::sync::Arc<crate::storage::ChannelRepository>,
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
                    let Some(client) = client.upgrade() else { break };
                    let (on_screen, muted) = conversation_attention(&client, message);
                    if !should_notify_message(message, me.as_ref(), on_screen, muted) {
                        continue;
                    }

                    // In an anon group the per-group name, never the global
                    // profile name of a contact who happens to be in it.
                    let sender_name = message.sender_id.map(|id| {
                        client.display_name_in(&message.conversation_type, &message.conversation_id, &id)
                    });
                    let preview = message.as_text();

                    let _ = notification_service.show_message_notification(
                        sender_name.as_deref(),
                        preview,
                        message.is_sealed_sender(),
                        NotificationPriority::Normal,
                    );
                }
                DomainEvent::ReactionAdded { sender_id, sent_at_ns, emoji, reactor_id, .. } => {
                    if me.is_none() || Some(*reactor_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else { break };
                    // Same mute / on-screen rules as the message it reacts
                    // to. A target we do not hold names no conversation to
                    // check, so it stays silent.
                    let Some(target) = client.get_message_by_sender_timestamp(sender_id, *sent_at_ns) else {
                        continue;
                    };
                    let (on_screen, muted) = conversation_attention(&client, &target);
                    if on_screen || muted {
                        continue;
                    }
                    let _ = notification_service.show_message_notification(
                        None,
                        Some(&format!("Reacted {}", emoji)),
                        false,
                        NotificationPriority::Low,
                    );
                }

                // ================================================================
                // Typed lifecycle events (from gRPC notification stream).
                // Side-effects run in spawned tasks so the event loop stays hot.
                // ================================================================
                DomainEvent::IncomingCall { call_id, peer_id: _, peer_name, is_video, channel_context } => {
                    // Urgent priority so the OS surfaces with sound even
                    // when device is on focus-assist — matches Signal's
                    // ring notification behaviour. Title carries media
                    // flag so the user knows whether to expect video.
                    let title = if *is_video { "Incoming video call" } else { "Incoming call" };
                    let body = match channel_context.as_deref() {
                        Some(ch) if !ch.is_empty() => format!("{} is calling in {}", peer_name, ch),
                        _ => format!("{} is calling", peer_name),
                    };
                    tracing::info!(
                        call_id = %call_id,
                        is_video = *is_video,
                        "IncomingCall notification → ringing call notification"
                    );
                    let _ = notification_service.show_incoming_call(call_id, title, &body);
                }

                DomainEvent::DeviceLinked { device_id, device_name, device_info, approved_by_self } => {
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

                DomainEvent::PeerDevicesChanged { peer_user_id, device_id, change_type } => {
                    // Two-phase rekey on peer device change:
                    //   1. Drop every pairwise Signal session for the peer so
                    //      ensure_session on next send fetches the fresh
                    //      prekey set covering the peer's current device
                    //      list.
                    //   2. For every channel where the peer holds
                    //      membership, mark the channel for sender-key
                    //      rotation. Next group send will regenerate the
                    //      sender key + redistribute SKDM so the peer's
                    //      new device receives it (and revoked devices
                    //      are excluded).
                    let peer_id = *peer_user_id;
                    let peer_str = peer_id.to_string();
                    let signal = signal_service.clone();
                    let repo = channel_repo.clone();
                    let change = change_type.clone();
                    let did = *device_id;
                    tokio::spawn(async move {
                        // Phase 1: pairwise session invalidation.
                        match signal.actor.invalidate_peer_sessions(peer_str.clone()).await {
                            Ok(n) => tracing::info!(
                                peer = %peer_str,
                                device_id = did,
                                change_type = %change,
                                dropped_sessions = n,
                                "invalidated peer sessions after device change"
                            ),
                            Err(e) => tracing::warn!(
                                peer = %peer_str,
                                error = %e,
                                "failed to invalidate peer sessions on device change"
                            ),
                        }

                        // Phase 2: group sender-key rotation flags.
                        // Run on a blocking thread — channel_repo is a
                        // synchronous SQLite handle.
                        let peer_id_for_blocking = peer_id;
                        let repo_for_blocking = repo.clone();
                        let channels = tokio::task::spawn_blocking(move || {
                            repo_for_blocking.list_channels_for_user(&peer_id_for_blocking)
                        }).await;

                        let channel_ids = match channels {
                            Ok(Ok(ids)) => ids,
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    peer = %peer_str,
                                    error = %e,
                                    "failed to list peer channel memberships — skipping SKDM rotation"
                                );
                                return;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    peer = %peer_str,
                                    error = %e,
                                    "join error listing peer channels"
                                );
                                return;
                            }
                        };

                        for channel_id in &channel_ids {
                            if let Err(e) = signal.actor.mark_channel_for_rotation(*channel_id).await {
                                tracing::warn!(
                                    channel_id = %channel_id,
                                    peer = %peer_str,
                                    error = %e,
                                    "failed to mark channel for SKDM rotation"
                                );
                            }
                        }
                        tracing::info!(
                            peer = %peer_str,
                            channels_marked = channel_ids.len(),
                            "marked peer-shared channels for sender-key rotation"
                        );
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
fn should_notify_message(
    message: &crate::domain::message::Message,
    me: Option<&crate::domain::ids::UserId>,
    on_screen: bool,
    muted: bool,
) -> bool {
    let Some(me) = me else { return false };
    message.sender_id.as_ref() != Some(me) && !on_screen && !muted
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
        let msg = |sender| Message::new_text(ChannelId::new(), ConversationType::ServerDm, Some(sender), "hi".into(), 1);

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
