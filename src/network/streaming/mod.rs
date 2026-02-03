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
pub mod reaction_stream;
pub mod read_receipt_stream;
pub mod stream_manager;
pub mod typing_stream;

pub use stream_manager::{StreamConfig, StreamHandle, StreamManager};
