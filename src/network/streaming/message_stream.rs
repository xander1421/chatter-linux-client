//! Message stream event routing.
//!
//! Routes message events from the global stream to typed broadcast channels.
//! Consumers can subscribe to specific event types without parsing oneofs.

use tokio::sync::broadcast;

use crate::proto::chat_v1::{Message, ReactionUpdate, ReadReceipt, TypingIndicator};

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
}

impl Default for MessageEventRouter {
    fn default() -> Self {
        Self::new()
    }
}
