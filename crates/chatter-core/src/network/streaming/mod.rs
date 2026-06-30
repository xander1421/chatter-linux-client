//! Real-time streaming layer.
//!
//! Manages long-lived gRPC streams for real-time updates including:
//! - Message events (new, edit, delete)
//! - Reactions (added, removed)
//! - Presence updates

pub mod message_stream;
pub mod presence_stream;
pub mod push_listener;
pub mod reaction_stream;
pub mod stream_manager;

// Re-exports for convenience
pub use message_stream::{MessageDeleted, MessageEventRouter, SubscriberCounts};
pub use presence_stream::{CachedPresence, PresenceRouter};
pub use push_listener::{PushEvent, PushListener, PushListenerConfig};
pub use reaction_stream::{ReactionAggregator, ReactionEvent};
pub use stream_manager::{StreamConfig, StreamHandle, StreamManager};
