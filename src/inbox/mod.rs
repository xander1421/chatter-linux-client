//! Device Inbox (Signal-style message queue).
//!
//! All incoming encrypted messages flow through the device inbox.
//!
//! # Architecture
//!
//! The inbox system consists of:
//! - `DeviceInboxProvider` - Main coordinator for inbox operations
//! - `InboxPoller` - Polling loop with challenge-response authentication
//! - `MessageProcessor` - Decryption coordination and content type detection
//! - `LocalQueue` - Retry queue with exponential backoff
//! - `InboxConfig` - Configuration for polling intervals and batch sizes
//! - `InboxEvent` - Events for monitoring message flow
//!
//! # Flow
//!
//! 1. `InboxPoller` fetches encrypted envelopes from server
//! 2. Messages pass through `MessageValidationPipeline` (dedup, validate, order)
//! 3. `MessageProcessor` decrypts and determines content type
//! 4. Failed messages go to `LocalQueue` for retry
//! 5. Handlers (Phase 24) process decrypted messages
//! 6. Processed messages are acknowledged to server

pub mod handlers;
pub mod poller;
pub mod processor;
pub mod provider;
pub mod queue;

// Re-export main types for convenience
pub use poller::InboxPoller;
pub use processor::{ContentType, MessageProcessor, ProcessedMessage, ProcessingError, ProcessingResult};
pub use provider::{
    DeviceInboxProvider, InboxConfig, InboxConfigBuilder, InboxError, InboxEvent, InboxState,
    InboxStats,
};
pub use queue::{LocalQueue, QueuedInboxMessage};
