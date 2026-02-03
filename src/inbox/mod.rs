//! Device Inbox (Signal-style message queue).
//!
//! All incoming encrypted messages flow through the device inbox.
//!
//! # Architecture
//!
//! The inbox system consists of:
//! - `DeviceInboxProvider` - Main coordinator for inbox operations
//! - `InboxPoller` - Polling loop with challenge-response authentication
//! - `InboxConfig` - Configuration for polling intervals and batch sizes
//! - `InboxEvent` - Events for monitoring message flow
//!
//! # Flow
//!
//! 1. `InboxPoller` fetches encrypted envelopes from server
//! 2. Messages pass through `MessageValidationPipeline` (dedup, validate, order)
//! 3. `InboxEvent::MessageReceived` emitted for handlers to process
//! 4. Handlers (Phase 24) decrypt and process the messages
//! 5. Processed messages are acknowledged to server

pub mod handlers;
pub mod poller;
pub mod processor;
pub mod provider;
pub mod queue;

// Re-export main types for convenience
pub use poller::InboxPoller;
pub use provider::{
    DeviceInboxProvider, InboxConfig, InboxConfigBuilder, InboxError, InboxEvent, InboxState,
    InboxStats,
};
