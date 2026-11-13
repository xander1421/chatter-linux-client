//! Real-time streaming layer.
//!
//! - `heartbeat` — liveness of the long-lived gRPC subscription streams.
//! - `push_listener` — self-hosted ntfy SSE wake-up hints (Android background push).
//!
//! Presence delivery does NOT live here: friend presence arrives as
//! `EventTypeUserPresenceUpdated` on the `StreamUserEvents` gRPC stream
//! (see `inbox::streaming::user_events`), friend-scoped by the server.

pub mod heartbeat;
pub mod push_listener;

// Re-exports for convenience
pub use push_listener::{PushListener, PushListenerConfig};

pub(crate) mod mailbox_wakes;
