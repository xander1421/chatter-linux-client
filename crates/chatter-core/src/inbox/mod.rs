//! Device Inbox (Signal-style message queue).
//!
//! All incoming encrypted messages flow through the device inbox.
//!
//! # Architecture
//!
//! - `DeviceInboxProvider` - Main coordinator for inbox operations
//! - `InboxPoller` - Polling loop with challenge-response authentication
//! - `LocalQueue` - Retry queue with exponential backoff
//! - `InboxConfig` - Configuration for polling intervals and batch sizes
//! - `InboxEvent` - Events for monitoring message flow
//! - `DomainEvent` - High-level events emitted to UI/state stores
//!
//! # Flow
//!
//! 1. `InboxPoller` fetches encrypted envelopes from server
//! 2. Messages pass through `MessageValidationPipeline` (dedup, validate, order)
//! 3. Polling handlers (`polling/*`) and service-layer dispatchers
//!    (`services::anon_dm::processing`, `services::anon_group::processing`,
//!    `services::inbox_processor`) decrypt, parse content and emit `DomainEvent`s
//! 4. Subscribers (state stores, UI) receive events via broadcast channel

pub mod ack_policy;
pub mod config;
pub mod error;
pub mod events;
pub mod handlers;
pub mod poller;
pub mod polling;
pub mod provider;
pub mod queue;
pub mod state;
pub mod streaming;

// Re-export main types for convenience
pub use config::{InboxConfig, InboxConfigBuilder};
pub use error::InboxError;
pub use events::InboxEvent;
pub use poller::InboxPoller;
pub use provider::DeviceInboxProvider;
pub use queue::{LocalQueue, QueuedInboxMessage};
pub use state::{InboxState, InboxStats};

// Re-export DomainEvent (emitted by all inbox dispatchers).
pub use handlers::DomainEvent;

// Re-export streaming handlers
pub use streaming::{spawn_dm_bootstrap_handler_from_bus, spawn_user_events_stream};
