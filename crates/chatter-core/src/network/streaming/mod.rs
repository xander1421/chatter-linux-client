//! Real-time streaming layer.
//!
//! Manages long-lived gRPC streams for real-time updates including:
//! - Message events (new, edit, delete)
//! - Reactions (added, removed)
//! - Typing indicators
//! - Read receipts
//! - Presence updates

pub mod message_stream;
pub mod presence_stream;
pub mod push_listener;
pub mod reaction_stream;
pub mod read_receipt_stream;
pub mod stream_manager;
pub mod typing_stream;

// Re-exports for convenience
pub use message_stream::{MessageDeleted, MessageEventRouter, MessageStream, SubscriberCounts};
pub use presence_stream::{CachedPresence, PresenceRouter, PresenceStream};
pub use push_listener::{PushEvent, PushListener, PushListenerConfig};
pub use reaction_stream::{ReactionAggregator, ReactionEvent, ReactionStream};
pub use read_receipt_stream::{ReadReceiptAggregator, ReadReceiptStream, ReadState};
pub use stream_manager::{StreamConfig, StreamHandle, StreamManager};
pub use typing_stream::{TypingAggregator, TypingStateChange, TypingStream};
