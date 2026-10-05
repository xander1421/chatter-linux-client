//! Event bus for domain-event routing.
//!
//! Provides centralized pub/sub for domain events using a broadcast channel.
//!
//! # Architecture
//!
//! ```text
//! +----------------+     +-------------+     +----------------+
//! | Inbox Handlers | --> | EventBus    | --> | EventHandlers  |
//! | (emit domain)  |     | (routes)    |     | (updates state)|
//! +----------------+     +-------------+     +----------------+
//! ```

use tokio::sync::broadcast;

use crate::inbox::handlers::DomainEvent;

/// Channel capacity for domain events.
///
/// Higher capacity to handle bursts of incoming messages.
const DOMAIN_CHANNEL_CAPACITY: usize = 64;

/// Event bus for domain events from inbox handlers (message received,
/// typing, etc.).
///
/// # Cloning
///
/// EventBus implements Clone efficiently - all clones share the same
/// underlying broadcast channel.
///
/// # Example
///
/// ```ignore
/// use chatter_core::events::{EventBus, DomainEvent};
///
/// let bus = EventBus::new();
/// let mut domain_rx = bus.subscribe_domain();
/// bus.emit_domain(DomainEvent::TypingStarted { user_id, channel_id, timestamp: 0 });
///
/// // Or get the sender for use in other components
/// let domain_tx = bus.domain_sender();
/// domain_tx.send(DomainEvent::MessageReceived { message })?;
/// ```
#[derive(Clone)]
pub struct EventBus {
    /// Sender for domain events.
    domain_tx: broadcast::Sender<DomainEvent>,
}

impl EventBus {
    /// Create a new event bus (domain channel: 64 events).
    #[must_use]
    pub fn new() -> Self {
        let (domain_tx, _) = broadcast::channel(DOMAIN_CHANNEL_CAPACITY);

        Self { domain_tx }
    }

    /// Get the domain event sender.
    ///
    /// Use this to pass to components that need to emit domain events
    /// (inbox dispatchers, service-layer processors).
    #[must_use]
    pub fn domain_sender(&self) -> broadcast::Sender<DomainEvent> {
        self.domain_tx.clone()
    }

    /// Subscribe to domain events.
    ///
    /// Returns a receiver that will receive all domain events.
    /// Multiple subscribers can exist simultaneously.
    ///
    /// # Note
    ///
    /// The receiver does NOT receive events that were sent before subscription.
    /// Lagged events (when receiver is too slow) will result in `RecvError::Lagged`.
    #[must_use]
    pub fn subscribe_domain(&self) -> broadcast::Receiver<DomainEvent> {
        self.domain_tx.subscribe()
    }

    /// Emit a domain event to all subscribers.
    ///
    /// Returns the number of active receivers, or 0 if no subscribers.
    /// This is a convenience method - for high-throughput scenarios,
    /// use `domain_sender()` directly.
    pub fn emit_domain(&self, event: DomainEvent) -> usize {
        self.domain_tx.send(event).unwrap_or(0)
    }

    /// Get the number of active domain event subscribers.
    #[must_use]
    pub fn domain_subscriber_count(&self) -> usize {
        self.domain_tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("domain_subscribers", &self.domain_tx.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};

    #[test]
    fn test_event_bus_new() {
        let bus = EventBus::new();
        assert_eq!(bus.domain_subscriber_count(), 0);
    }

    #[test]
    fn test_event_bus_clone_shares_channels() {
        let bus1 = EventBus::new();
        let _rx = bus1.subscribe_domain();

        let bus2 = bus1.clone();
        // Both buses see the same subscriber count
        assert_eq!(bus2.domain_subscriber_count(), 1);
    }

    #[test]
    fn test_event_bus_default() {
        let bus = EventBus::default();
        assert_eq!(bus.domain_subscriber_count(), 0);
    }

    #[tokio::test]
    async fn test_emit_domain_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let count = bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });
        assert_eq!(count, 1);

        let event = rx.recv().await.unwrap();
        if let DomainEvent::TypingStarted {
            user_id: u,
            channel_id: c,
            ..
        } = event
        {
            assert_eq!(u, user_id);
            assert_eq!(c, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    #[tokio::test]
    async fn test_multiple_domain_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe_domain();
        let mut rx2 = bus.subscribe_domain();

        assert_eq!(bus.domain_subscriber_count(), 2);

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });

        // Both subscribers receive the event
        let event1 = rx1.recv().await.unwrap();
        let event2 = rx2.recv().await.unwrap();

        assert!(matches!(event1, DomainEvent::TypingStarted { .. }));
        assert!(matches!(event2, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_emit_returns_zero_with_no_subscribers() {
        let bus = EventBus::new();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let count = bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_domain_sender_works_independently() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();
        let tx = bus.domain_sender();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Send via the sender obtained from domain_sender()
        let _ = tx.send(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_message_received_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let message = Message::new_text(
            channel_id,
            ConversationType::Channel,
            Some(user_id),
            "Hello!".to_string(),
            0,
        );
        let msg_id = message.id;

        bus.emit_domain(DomainEvent::MessageReceived {
            message: message.clone(),
        });

        let event = rx.recv().await.unwrap();
        if let DomainEvent::MessageReceived { message: m } = event {
            assert_eq!(m.id, msg_id);
            assert_eq!(m.conversation_id, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    #[test]
    fn test_debug_impl() {
        let bus = EventBus::new();
        let _rx = bus.subscribe_domain();
        let debug_str = format!("{:?}", bus);
        assert!(debug_str.contains("EventBus"));
        assert!(debug_str.contains("domain_subscribers"));
    }

    #[tokio::test]
    async fn test_anon_dm_init_received_event() {
        use crate::domain::AnonDmId;

        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();
        let tx = bus.domain_sender();

        let conversation_id = AnonDmId::new();
        let sender_id = UserId::new();

        // Simulate what process_ecies_init_message does
        let event = DomainEvent::AnonDmInitReceived {
            conversation_id,
            sender_id,
            sender_display_name: "TestSender".to_string(),
            sender_avatar: None,
            peer_mailbox_pk: [0u8; 32],
            peer_identity_key: [0u8; 33],
            peer_delivery_token: [0u8; 12],
            intro_message: "Hello!".to_string(),
            server_timestamp: 12345,
        };

        // Send via the sender (simulating inbox provider's domain_tx.send())
        let receivers = tx.send(event).expect("send should succeed");
        assert_eq!(receivers, 1, "Should have one receiver");

        // Receive and verify
        let received_event = rx.recv().await.expect("should receive event");
        match received_event {
            DomainEvent::AnonDmInitReceived {
                conversation_id: cid,
                sender_id: sid,
                intro_message,
                ..
            } => {
                assert_eq!(cid, conversation_id);
                assert_eq!(sid, sender_id);
                assert_eq!(intro_message, "Hello!");
            }
            _ => panic!(
                "Wrong event type received: {:?}",
                std::mem::discriminant(&received_event)
            ),
        }
    }

    #[tokio::test]
    async fn test_domain_sender_across_cloned_bus() {
        // This tests the exact pattern used in inbox provider:
        // - event_bus is cloned from CoreServices
        // - domain_sender() is called on the clone
        // - subscribe_domain() is called on another clone
        let bus1 = EventBus::new();

        // Clone like CoreServices.event_bus.clone()
        let bus2 = bus1.clone();

        // Handler subscribes on one clone
        let mut rx = bus1.subscribe_domain();

        // Inbox provider gets sender from another clone
        let tx = bus2.domain_sender();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Send from the sender
        let receivers = tx
            .send(DomainEvent::TypingStarted {
                user_id,
                channel_id,
                timestamp: 0,
            })
            .expect("send should succeed");

        assert_eq!(receivers, 1, "Should have one receiver");

        // Receiver should get it
        let event = rx.recv().await.expect("should receive event");
        assert!(matches!(event, DomainEvent::TypingStarted { .. }));
    }
}
