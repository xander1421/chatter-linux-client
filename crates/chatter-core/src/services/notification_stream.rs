//! gRPC notification stream consumer.
//!
//! Long-lived task that subscribes to `NotificationService.StreamNotifications`
//! and translates typed server notifications into [`DomainEvent`]s. Runs in
//! parallel with:
//!   * `mailbox::poller` — authoritative message delivery for DM/group
//!     content. Still owned by mailbox poll; this stream is notification-
//!     only, not a message transport.
//!   * `network::streaming::push_listener` — ntfy SSE wake-up hint. Push
//!     has no typed metadata so it stays wake-only.
//!
//! The server emits two typed events the client must act on:
//!   * `DEVICE_LINKED`          — toast + refresh device list UI.
//!   * `PEER_DEVICES_CHANGED`   — invalidate peer session cache + rekey groups.
//!
//! Desynced 1:1 ratchets self-heal E2E via Signal `DecryptionErrorMessage`
//! (see `MessageGateway::process_incoming_message`) — there is no
//! server-mediated session reset.
//!
//! Prekey rotation is client-driven by the `spawn_prekey_replenishment`
//! background task (Signal X3DH §3.2 / PQXDH §3.2); the server does not
//! emit a refresh nudge.
//!
//! Reconnect strategy: on stream close or transport error, reconnect with
//! exponential backoff capped at 60s. Respects `shutdown_token`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::domain::UserId;
use crate::events::bus::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::network::grpc::GrpcClientPool;
use crate::proto::chat_v1::{Notification, NotificationType};

/// Bounded LRU set of recently-seen notification IDs. Used to short-circuit
/// duplicate server deliveries (e.g. server retries the same notification
/// after a client-side disconnect before ACK semantics settle). Prevents
/// double-invalidating the same peer's Signal sessions or double-toasting
/// the same DeviceLinked event.
///
/// Capacity chosen so a burst of ~256 events can arrive during a
/// reconnect window without evicting still-relevant IDs. Notifications
/// landing outside this window are assumed to be genuinely new.
const DEDUP_CAPACITY: usize = 256;

struct RecentIds {
    queue: VecDeque<String>,
    set: std::collections::HashSet<String>,
}

impl RecentIds {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(DEDUP_CAPACITY),
            set: std::collections::HashSet::with_capacity(DEDUP_CAPACITY),
        }
    }

    /// Returns true if the ID was freshly inserted (first time seen).
    /// Returns false when it was already present (duplicate) — caller
    /// should skip side effects.
    fn insert(&mut self, id: String) -> bool {
        if self.set.contains(&id) {
            return false;
        }
        if self.queue.len() >= DEDUP_CAPACITY {
            if let Some(old) = self.queue.pop_front() {
                self.set.remove(&old);
            }
        }
        self.set.insert(id.clone());
        self.queue.push_back(id);
        true
    }
}

/// Metadata payload for DEVICE_LINKED notifications.
#[derive(Debug, Deserialize)]
struct DeviceLinkedMeta {
    #[serde(default)]
    device_id: u32,
    #[serde(default)]
    linked_device_id: u32,
    #[serde(default)]
    device_info: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
}

/// Metadata payload for PEER_DEVICES_CHANGED notifications.
#[derive(Debug, Deserialize)]
struct PeerDevicesChangedMeta {
    peer_user_id: String,
    #[serde(default)]
    device_id: u32,
    #[serde(default)]
    change_type: String,
}

/// Spawn a background task that consumes the typed notification stream.
///
/// Reconnects on failure with exponential backoff (capped at 60s). Runs
/// indefinitely; caller stores the returned `JoinHandle` and `.abort()`s
/// it during graceful shutdown (see `ChatterClient::shutdown`).
pub fn spawn_grpc_notification_consumer(
    pool: Arc<GrpcClientPool>,
    event_bus: EventBus,
    notifications: Arc<crate::services::notification_service::NotificationService>,
    user_id: UserId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        // Dedup persists across reconnects so a server-side retry of the
        // same notification (redelivered after the client's previous
        // stream dropped mid-batch) doesn't fire side effects twice.
        let mut recent_ids = RecentIds::new();

        loop {
            let mut client = pool.notification_client();
            let user_id_str = user_id.to_string();

            info!(user_id = %user_id_str, "notification_stream: connecting");
            match client.stream_notifications(Vec::new()).await {
                Ok(stream) => {
                    // Connected — reset backoff for next reconnect.
                    backoff = Duration::from_secs(1);
                    tokio::pin!(stream);

                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(resp) => {
                                if let Some(notif) = resp.notification {
                                    // Empty ID on the proto means the
                                    // server didn't populate one; fall
                                    // through without dedup so we don't
                                    // drop legitimate events. Server
                                    // always assigns IDs today, but this
                                    // keeps the consumer forward-compat.
                                    if notif.id.is_empty() || recent_ids.insert(notif.id.clone()) {
                                        dispatch_notification(&event_bus, &notifications, notif);
                                    } else {
                                        debug!(
                                            notif_id = %notif.id,
                                            "notification_stream: duplicate notification, skipping"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "notification_stream: item error, reconnecting");
                                break;
                            }
                        }
                    }
                    debug!("notification_stream: stream closed by server, reconnecting");
                }
                Err(e) => {
                    warn!(error = %e, "notification_stream: connect failed");
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
}

/// Route one server-sent [`Notification`].
///
/// INFRA-33: this used to handle 2 of the 13 types the server can emit and end
/// in `other => debug!(...)`, so CHANNEL_INVITE and FRIEND_REQUEST — which have
/// no other in-app route while ntfy push is dead — were silently dropped. The
/// match is now exhaustive, with the routing decision written down per type:
///
/// * MESSAGE / MENTION / REPLY / REACTION — **ignored on purpose.** Content
///   arrives over the device-inbox poll with its ciphertext; surfacing the
///   server's plaintext-metadata copy as well would double-notify AND would
///   show content the server is not supposed to be the source of.
/// * DEVICE_LINKED / PEER_DEVICES_CHANGED — machine-consumed: emitted as
///   domain events that drive session/device side effects.
/// * CHANNEL_INVITE / FRIEND_REQUEST / FRIEND_ACCEPTED / SYSTEM / INTEGRATION —
///   user-facing and delivered nowhere else, so they become a local toast.
///   These carry no message content, only "something happened", so showing them
///   costs no confidentiality the server does not already have.
/// * CALL_INCOMING / CALL_MISSED — **ignored on purpose.** Ringing is driven by
///   the encrypted call offer through the inbox (`call_signaling`), which is
///   authenticated end to end; a server-pushed ring would be a second,
///   unauthenticated way to make a device ring.
///
/// The arms are spelled out rather than caught by a wildcard so that a new
/// notification type is a compile error here instead of a silent drop.
fn dispatch_notification(
    event_bus: &EventBus,
    notifications: &Arc<crate::services::notification_service::NotificationService>,
    notif: Notification,
) {
    let ty = NotificationType::try_from(notif.r#type).unwrap_or(NotificationType::Unspecified);
    let sender = event_bus.domain_sender();

    match ty {
        NotificationType::DeviceLinked => {
            let meta: DeviceLinkedMeta = match parse_meta(&notif) {
                Some(m) => m,
                None => return,
            };
            // linked_device_id is the new device's ID; device_id may also
            // appear in older server versions. Prefer linked_device_id.
            let device_id = if meta.linked_device_id > 0 {
                meta.linked_device_id
            } else {
                meta.device_id
            };
            let _ = sender.send(DomainEvent::DeviceLinked {
                device_id,
                device_name: meta.device_name.filter(|s| !s.is_empty()),
                device_info: meta.device_info.filter(|s| !s.is_empty()),
                // approved_by_self cannot be known from the gRPC stream
                // alone — the provisioning path sets this true when the
                // primary itself emits the event locally. Here we're on
                // the receiving side of the fan-out; always false.
                approved_by_self: false,
            });
        }
        NotificationType::PeerDevicesChanged => {
            let meta: PeerDevicesChangedMeta = match parse_meta(&notif) {
                Some(m) => m,
                None => return,
            };
            let peer_user_id = match meta.peer_user_id.parse::<UserId>() {
                Ok(id) => id,
                Err(e) => {
                    warn!(error = %e, value = %meta.peer_user_id, "PEER_DEVICES_CHANGED: invalid peer_user_id");
                    return;
                }
            };
            let _ = sender.send(DomainEvent::PeerDevicesChanged {
                peer_user_id,
                device_id: meta.device_id,
                change_type: meta.change_type,
            });
        }
        // Content types: the inbox poll owns these (see the doc comment).
        NotificationType::Message
        | NotificationType::Mention
        | NotificationType::Reply
        | NotificationType::Reaction => {
            debug!(ty = ?ty, "notification_stream: content type, owned by the inbox poll");
        }

        // Ringing is owned by the authenticated call-offer path.
        NotificationType::CallIncoming | NotificationType::CallMissed => {
            debug!(ty = ?ty, "notification_stream: call type, owned by call signalling");
        }

        // No other in-app route: surface as a local toast.
        NotificationType::ChannelInvite
        | NotificationType::FriendRequest
        | NotificationType::FriendAccepted
        | NotificationType::System
        | NotificationType::Integration => {
            // Local strings on purpose: the server's title/body are not
            // rendered, so a compromised or buggy server cannot put arbitrary
            // text on the user's desktop. The event type is all we need.
            let (title, body) = match ty {
                NotificationType::ChannelInvite => ("Chatter", "You were invited to a channel"),
                NotificationType::FriendRequest => ("Chatter", "New friend request"),
                NotificationType::FriendAccepted => ("Chatter", "Friend request accepted"),
                NotificationType::Integration => ("Chatter", "An integration needs your attention"),
                _ => ("Chatter", "New notification"),
            };
            if let Err(e) = notifications.show_message(
                title,
                body,
                crate::domain::notification::NotificationPriority::Normal,
            ) {
                warn!(ty = ?ty, error = %e, "notification_stream: failed to show notification");
            }
        }

        NotificationType::Unspecified => {
            warn!(raw = notif.r#type, "notification_stream: unspecified notification type");
        }
    }
}

/// Parse the `metadata` JSON string into the expected shape. Logs + drops
/// on parse failure so one malformed event doesn't poison the stream.
fn parse_meta<T: for<'de> Deserialize<'de>>(notif: &Notification) -> Option<T> {
    if notif.metadata.is_empty() {
        warn!(notif_id = %notif.id, ty = notif.r#type, "typed notification has empty metadata");
        return None;
    }
    match serde_json::from_str::<T>(&notif.metadata) {
        Ok(v) => Some(v),
        Err(e) => {
            error!(error = %e, notif_id = %notif.id, "failed to parse notification metadata JSON");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::notification::NotificationPriority;
    use crate::events::bus::EventBus;
    use crate::services::notification_service::NotificationService;
    use crate::system::notification_sink::NotificationSink;
    use std::sync::Mutex;
    use tokio::sync::broadcast::error::TryRecvError;

    /// Records every toast the router asks for.
    struct RecordingSink(Mutex<Vec<(String, String)>>);

    impl NotificationSink for RecordingSink {
        fn show(
            &self,
            title: &str,
            body: &str,
            _priority: NotificationPriority,
        ) -> Result<(), crate::error::SystemError> {
            self.0.lock().unwrap().push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    fn recording_service() -> (Arc<NotificationService>, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        (Arc::new(NotificationService::new(sink.clone())), sink)
    }

    fn bus_with_receiver() -> (EventBus, tokio::sync::broadcast::Receiver<DomainEvent>) {
        let bus = EventBus::new();
        let rx = bus.subscribe_domain();
        (bus, rx)
    }

    fn notif(ty: NotificationType, metadata: &str) -> Notification {
        Notification {
            id: "test-id-1".to_string(),
            user_id: String::new(),
            r#type: ty as i32,
            priority: 0,
            title: String::new(),
            body: String::new(),
            metadata: metadata.to_string(),
            channel_id: String::new(),
            message_id: String::new(),
            actor_id: String::new(),
            read: false,
            read_at: 0,
            created_at: 0,
            expires_at: 0,
            action_url: String::new(),
        }
    }

    #[test]
    fn recent_ids_dedup_basic() {
        let mut r = RecentIds::new();
        assert!(r.insert("a".into()));
        assert!(!r.insert("a".into()), "second insert must be dup");
        assert!(r.insert("b".into()));
    }

    #[test]
    fn recent_ids_evicts_oldest_over_capacity() {
        let mut r = RecentIds::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(r.insert(format!("id-{}", i)));
        }
        // Capacity reached. Insert one more — oldest ("id-0") should evict.
        assert!(r.insert("id-new".into()));
        // "id-0" no longer present, so re-inserting it returns true.
        assert!(r.insert("id-0".into()));
    }

    #[test]
    fn dispatch_device_linked_emits_event() {
        let (bus, mut rx) = bus_with_receiver();
        let meta = r#"{"device_id":7,"linked_device_id":7,"device_info":"chrome-linux","device_name":"Alex Desktop"}"#;
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::DeviceLinked, meta));
        match rx.try_recv().expect("event expected") {
            DomainEvent::DeviceLinked { device_id, device_name, device_info, approved_by_self } => {
                assert_eq!(device_id, 7);
                assert_eq!(device_name.as_deref(), Some("Alex Desktop"));
                assert_eq!(device_info.as_deref(), Some("chrome-linux"));
                assert!(!approved_by_self, "stream-side receiver is never the approver");
            }
            other => panic!("wrong event variant: {:?}", other),
        }
    }

    #[test]
    fn dispatch_device_linked_prefers_linked_device_id() {
        // Legacy-shape payload with only `device_id` — client must still
        // parse it (forward-compat for older servers).
        let (bus, mut rx) = bus_with_receiver();
        let meta = r#"{"device_id":5,"linked_device_id":0}"#;
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::DeviceLinked, meta));
        match rx.try_recv().expect("event expected") {
            DomainEvent::DeviceLinked { device_id, .. } => assert_eq!(device_id, 5),
            other => panic!("wrong event variant: {:?}", other),
        }
    }

    #[test]
    fn dispatch_peer_devices_changed_emits_event() {
        let (bus, mut rx) = bus_with_receiver();
        let peer = uuid::Uuid::new_v4().to_string();
        let meta = format!(
            r#"{{"peer_user_id":"{}","device_id":3,"change_type":"added"}}"#,
            peer
        );
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::PeerDevicesChanged, &meta));
        match rx.try_recv().expect("event expected") {
            DomainEvent::PeerDevicesChanged { peer_user_id, device_id, change_type } => {
                assert_eq!(peer_user_id.to_string(), peer);
                assert_eq!(device_id, 3);
                assert_eq!(change_type, "added");
            }
            other => panic!("wrong event variant: {:?}", other),
        }
    }

    #[test]
    fn dispatch_unknown_type_drops_silently() {
        let (bus, mut rx) = bus_with_receiver();
        // MESSAGE type is defined but intentionally ignored — mailbox poll
        // is the authoritative content path for it.
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::Message, "{}"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dispatch_unspecified_type_drops_silently() {
        let (bus, mut rx) = bus_with_receiver();
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::Unspecified, "{}"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dispatch_empty_metadata_drops() {
        let (bus, mut rx) = bus_with_receiver();
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::DeviceLinked, ""));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dispatch_malformed_json_drops() {
        let (bus, mut rx) = bus_with_receiver();
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::DeviceLinked, "{not valid json"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dispatch_peer_devices_changed_rejects_invalid_user_id() {
        let (bus, mut rx) = bus_with_receiver();
        let meta = r#"{"peer_user_id":"not-a-uuid","device_id":3,"change_type":"added"}"#;
        dispatch_notification(&bus, &recording_service().0, notif(NotificationType::PeerDevicesChanged, meta));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// INFRA-33 regression: the router handled 2 of the 13 notification types
    /// the server can emit and ended in `other => debug!(...)`. CHANNEL_INVITE
    /// and FRIEND_REQUEST have no other in-app route while push is dead, so
    /// they reached the user nowhere at all.
    #[test]
    fn user_facing_notifications_without_another_route_are_surfaced() {
        let (service, sink) = recording_service();
        let bus = EventBus::new();

        for ty in [
            NotificationType::ChannelInvite,
            NotificationType::FriendRequest,
            NotificationType::FriendAccepted,
            NotificationType::System,
            NotificationType::Integration,
        ] {
            let mut n = notif(ty, "{}");
            n.title = "server-supplied title".to_string();
            n.body = "server-supplied body".to_string();
            dispatch_notification(&bus, &service, n);
        }

        let shown = sink.0.lock().unwrap().clone();
        assert_eq!(
            shown.len(),
            5,
            "a user-facing notification type reached the user nowhere: {shown:?}"
        );
        assert!(
            shown.iter().any(|(_, body)| body.contains("invited to a channel")),
            "CHANNEL_INVITE produced no invite toast: {shown:?}"
        );
        assert!(
            shown.iter().any(|(_, body)| body.contains("friend request")),
            "FRIEND_REQUEST produced no friend-request toast: {shown:?}"
        );
        // The server's own strings are never rendered.
        assert!(
            !shown
                .iter()
                .any(|(t, b)| t.contains("server-supplied") || b.contains("server-supplied")),
            "server-supplied notification text was rendered: {shown:?}"
        );
    }

    /// Content and call types must stay silent here: the inbox poll and the
    /// authenticated call-offer path own them, and double-surfacing would both
    /// duplicate the toast and render text the server should not be the source
    /// of.
    #[test]
    fn content_and_call_notifications_are_not_double_surfaced() {
        let (service, sink) = recording_service();
        let bus = EventBus::new();

        for ty in [
            NotificationType::Message,
            NotificationType::Mention,
            NotificationType::Reply,
            NotificationType::Reaction,
            NotificationType::CallIncoming,
            NotificationType::CallMissed,
        ] {
            dispatch_notification(&bus, &service, notif(ty, "{}"));
        }

        let shown = sink.0.lock().unwrap().clone();
        assert!(
            shown.is_empty(),
            "content/call notifications were surfaced twice: {shown:?}"
        );
    }
}
