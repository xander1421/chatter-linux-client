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
