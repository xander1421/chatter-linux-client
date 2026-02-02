//! Type-specific message handlers.
//!
//! Handlers for different inbox message types.

pub mod chat;
pub mod attachment;
pub mod reaction;
pub mod receipt;
pub mod typing;
pub mod key_exchange;
pub mod sender_key;
pub mod pin;
pub mod sync;

/// Content types the inbox can receive.
pub enum ContentType {
    /// Regular encrypted message.
    ChatMessage,
    /// File attachment metadata.
    AttachmentPointer,
    /// Emoji reaction.
    Reaction,
    /// Read receipt batch.
    ReadReceipt,
    /// Delivery confirmation.
    DeliveryReceipt,
    /// Typing start/stop.
    TypingIndicator,
    /// X3DH prekey message.
    KeyExchange,
    /// Group sender key.
    SenderKeyDistribution,
    /// Pin/unpin message.
    PinEvent,
    /// Message edit.
    EditEvent,
    /// Message delete.
    DeleteEvent,
    /// Multi-device sync.
    SyncMessage,
}
