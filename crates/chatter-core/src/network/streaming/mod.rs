//! Real-time streaming layer.
//!
//! - `push_listener` — self-hosted ntfy SSE wake-up hints (Android background push).
//! - `reaction_stream` — reaction aggregation for efficient UI updates.
//!
//! Presence delivery does NOT live here: friend presence arrives as
//! `EventTypeUserPresenceUpdated` on the `StreamUserEvents` gRPC stream
//! (see `inbox::streaming::user_events`), friend-scoped by the server.

pub mod push_listener;
pub mod reaction_stream;

// Re-exports for convenience
pub use push_listener::{PushEvent, PushListener, PushListenerConfig};
pub use reaction_stream::{ReactionAggregator, ReactionEvent};
