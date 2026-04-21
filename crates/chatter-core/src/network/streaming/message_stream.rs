//! Message stream event routing.
//!
//! Routes message events from the global stream to typed broadcast channels.
//! Consumers can subscribe to specific event types without parsing oneofs.

use tokio::sync::broadcast;

use crate::proto::chat_v1::{
    message_event::Event as MessageEventType, Message, MessageEvent, ReactionUpdate, ReadReceipt,
    TypingIndicator,
};

/// Capacity for event broadcast channels.
const CHANNEL_CAPACITY: usize = 256;

/// Routed message events with typed broadcast channels.
///
/// Receives raw `MessageEvent` from the global stream and routes
/// them to typed channels for easy consumption.
///
/// # Example
///
/// ```ignore
/// let router = MessageEventRouter::new();
///
/// // Subscribe to specific events
/// let mut messages_rx = router.on_message_sent();
/// let mut typing_rx = router.on_typing_indicator();
///
/// // Process incoming events
/// router.route(message_event);
///
/// // Consumers receive typed events
/// while let Ok(msg) = messages_rx.recv().await {
///     println!("New message: {}", msg.id);
/// }
/// ```
pub struct MessageEventRouter {
    /// New messages received.
    message_sent: broadcast::Sender<Message>,
    /// Message content updated.
    message_updated: broadcast::Sender<Message>,
    /// Message deleted (ID only).
    message_deleted: broadcast::Sender<MessageDeleted>,
    /// Reaction added to a message.
    reaction_added: broadcast::Sender<ReactionUpdate>,
    /// Reaction removed from a message.
    reaction_removed: broadcast::Sender<ReactionUpdate>,
    /// Read receipt received.
    read_receipt: broadcast::Sender<ReadReceipt>,
    /// Typing indicator received.
    typing_indicator: broadcast::Sender<TypingIndicator>,
}

/// Message deletion event with context.
#[derive(Clone, Debug)]
pub struct MessageDeleted {
    /// ID of the deleted message.
    pub message_id: String,
    /// Channel the message was in.
    pub channel_id: String,
    /// Timestamp of the deletion event.
    pub timestamp: u64,
}

impl MessageEventRouter {
    /// Create a new MessageEventRouter with all channels initialized.
    pub fn new() -> Self {
        Self {
            message_sent: broadcast::channel(CHANNEL_CAPACITY).0,
            message_updated: broadcast::channel(CHANNEL_CAPACITY).0,
            message_deleted: broadcast::channel(CHANNEL_CAPACITY).0,
            reaction_added: broadcast::channel(CHANNEL_CAPACITY).0,
            reaction_removed: broadcast::channel(CHANNEL_CAPACITY).0,
            read_receipt: broadcast::channel(CHANNEL_CAPACITY).0,
            typing_indicator: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Subscribe to new message events.
    pub fn on_message_sent(&self) -> broadcast::Receiver<Message> {
        self.message_sent.subscribe()
    }

    /// Subscribe to message update events.
    pub fn on_message_updated(&self) -> broadcast::Receiver<Message> {
        self.message_updated.subscribe()
    }

    /// Subscribe to message deletion events.
    pub fn on_message_deleted(&self) -> broadcast::Receiver<MessageDeleted> {
        self.message_deleted.subscribe()
    }

    /// Subscribe to reaction added events.
    pub fn on_reaction_added(&self) -> broadcast::Receiver<ReactionUpdate> {
        self.reaction_added.subscribe()
    }

    /// Subscribe to reaction removed events.
    pub fn on_reaction_removed(&self) -> broadcast::Receiver<ReactionUpdate> {
        self.reaction_removed.subscribe()
    }

    /// Subscribe to read receipt events.
    pub fn on_read_receipt(&self) -> broadcast::Receiver<ReadReceipt> {
        self.read_receipt.subscribe()
    }

    /// Subscribe to typing indicator events.
    pub fn on_typing_indicator(&self) -> broadcast::Receiver<TypingIndicator> {
        self.typing_indicator.subscribe()
    }

    /// Route a raw MessageEvent to the appropriate typed channel.
    ///
    /// Returns true if the event was successfully routed, false if no
    /// subscribers or the event type was empty.
    pub fn route(&self, event: MessageEvent) -> bool {
        let channel_id = event.channel_id.clone();
        let timestamp = event.timestamp;

        match event.event {
            Some(MessageEventType::MessageSent(msg)) => self.message_sent.send(msg).is_ok(),
            Some(MessageEventType::MessageUpdated(msg)) => self.message_updated.send(msg).is_ok(),
            Some(MessageEventType::MessageDeletedId(id)) => {
                let deleted = MessageDeleted {
                    message_id: id,
                    channel_id,
                    timestamp,
                };
                self.message_deleted.send(deleted).is_ok()
            }
            Some(MessageEventType::ReactionAdded(reaction)) => {
                self.reaction_added.send(reaction).is_ok()
            }
            Some(MessageEventType::ReactionRemoved(reaction)) => {
                self.reaction_removed.send(reaction).is_ok()
            }
            Some(MessageEventType::ReadReceipt(receipt)) => self.read_receipt.send(receipt).is_ok(),
            Some(MessageEventType::TypingIndicator(typing)) => {
                self.typing_indicator.send(typing).is_ok()
            }
            None => {
                tracing::warn!("Empty MessageEvent received");
                false
            }
        }
    }

    /// Get the number of active subscribers for each channel.
    ///
    /// Useful for debugging and monitoring.
    pub fn subscriber_counts(&self) -> SubscriberCounts {
        SubscriberCounts {
            message_sent: self.message_sent.receiver_count(),
            message_updated: self.message_updated.receiver_count(),
            message_deleted: self.message_deleted.receiver_count(),
            reaction_added: self.reaction_added.receiver_count(),
            reaction_removed: self.reaction_removed.receiver_count(),
            read_receipt: self.read_receipt.receiver_count(),
            typing_indicator: self.typing_indicator.receiver_count(),
        }
    }
}

/// Subscriber counts for monitoring.
#[derive(Clone, Debug, Default)]
pub struct SubscriberCounts {
    pub message_sent: usize,
    pub message_updated: usize,
    pub message_deleted: usize,
    pub reaction_added: usize,
    pub reaction_removed: usize,
    pub read_receipt: usize,
    pub typing_indicator: usize,
}

impl Default for MessageEventRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_message(id: &str, channel_id: &str) -> Message {
        Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: "user-123".to_string(),
            parent_id: String::new(),
            content: None,
            timestamp: 1234567890000,
            signature: vec![],
            ephemeral: false,
            ephemeral_ttl: None,
            pinned: false,
            reply_count: 0,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            crypto_status: None,
            updated_at: 0,
            edited: false,
            edited_at: 0,
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            channel_type: 0,
        }
    }

    fn make_message_event(event_type: MessageEventType, channel_id: &str) -> MessageEvent {
        MessageEvent {
            event_id: "evt-001".to_string(),
            timestamp: 1234567890000,
            channel_id: channel_id.to_string(),
            event: Some(event_type),
        }
    }

    #[test]
    fn test_router_new() {
        let router = MessageEventRouter::new();
        let counts = router.subscriber_counts();

        // No subscribers initially
        assert_eq!(counts.message_sent, 0);
        assert_eq!(counts.typing_indicator, 0);
    }

    #[test]
    fn test_subscribe_increases_count() {
        let router = MessageEventRouter::new();

        let _rx1 = router.on_message_sent();
        let _rx2 = router.on_message_sent();

        assert_eq!(router.subscriber_counts().message_sent, 2);
    }

    #[tokio::test]
    async fn test_route_message_sent() {
        let router = MessageEventRouter::new();
        let mut rx = router.on_message_sent();

        let msg = make_test_message("msg-123", "channel-456");
        let event = make_message_event(MessageEventType::MessageSent(msg), "channel-456");

        assert!(router.route(event));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.id, "msg-123");
        assert_eq!(received.channel_id, "channel-456");
    }

    #[tokio::test]
    async fn test_route_message_deleted() {
        let router = MessageEventRouter::new();
        let mut rx = router.on_message_deleted();

        let event = make_message_event(
            MessageEventType::MessageDeletedId("msg-999".to_string()),
            "channel-456",
        );

        assert!(router.route(event));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.message_id, "msg-999");
        assert_eq!(received.channel_id, "channel-456");
    }

    #[tokio::test]
    async fn test_route_typing_indicator() {
        let router = MessageEventRouter::new();
        let mut rx = router.on_typing_indicator();

        let typing = TypingIndicator {
            user_id: "user-123".to_string(),
            channel_id: "channel-456".to_string(),
            is_typing: true,
        };
        let event = make_message_event(MessageEventType::TypingIndicator(typing), "channel-456");

        assert!(router.route(event));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.user_id, "user-123");
        assert!(received.is_typing);
    }

    #[test]
    fn test_route_empty_event() {
        let router = MessageEventRouter::new();

        let event = MessageEvent {
            event_id: "evt-001".to_string(),
            timestamp: 0,
            channel_id: "channel-456".to_string(),
            event: None,
        };

        // Should return false for empty event
        assert!(!router.route(event));
    }

    #[test]
    fn test_route_no_subscribers() {
        let router = MessageEventRouter::new();

        let msg = make_test_message("msg-123", "channel-456");
        let event = make_message_event(MessageEventType::MessageSent(msg), "channel-456");

        // No subscribers - send returns Err, route returns false
        let result = router.route(event);
        assert!(!result);
    }

    #[test]
    fn test_message_deleted_struct() {
        let deleted = MessageDeleted {
            message_id: "msg-123".to_string(),
            channel_id: "channel-456".to_string(),
            timestamp: 1234567890000,
        };

        assert_eq!(deleted.message_id, "msg-123");
        assert_eq!(deleted.channel_id, "channel-456");
    }

    #[tokio::test]
    async fn test_route_reaction_added() {
        let router = MessageEventRouter::new();
        let mut rx = router.on_reaction_added();

        let reaction = ReactionUpdate {
            target_author_aci: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            target_sent_timestamp: 1704067200_000_000_000,
            emoji: "👍".to_string(),
            add: true,
            proof_of_work: vec![],
        };
        let event = make_message_event(MessageEventType::ReactionAdded(reaction), "channel-456");

        assert!(router.route(event));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.target_author_aci, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(received.emoji, "👍");
    }

    #[tokio::test]
    async fn test_route_read_receipt() {
        let router = MessageEventRouter::new();
        let mut rx = router.on_read_receipt();

        let receipt = ReadReceipt {
            user_id: "user-123".to_string(),
            channel_id: "channel-456".to_string(),
            last_read_id: "msg-456".to_string(),
            timestamp: 1234567890000,
        };
        let event = make_message_event(MessageEventType::ReadReceipt(receipt), "channel-456");

        assert!(router.route(event));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.user_id, "user-123");
        assert_eq!(received.last_read_id, "msg-456");
    }

    #[test]
    fn test_default_impl() {
        let router = MessageEventRouter::default();
        assert_eq!(router.subscriber_counts().message_sent, 0);
    }

}
