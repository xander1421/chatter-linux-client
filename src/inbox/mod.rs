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
//! - `HandlerRegistry` - Routes messages to type-specific handlers
//!
//! # Module Organization
//!
//! - `config` - Polling configuration (intervals, batch sizes, retries)
//! - `events` - Event types emitted by the inbox system
//! - `state` - Inbox state and statistics tracking
//! - `error` - Error types for inbox operations
//! - `polling` - Handlers for device, DM, and group mailbox polling
//! - `handlers` - Content-type specific message handlers
//!
//! # Handler Flow
//!
//! 1. `MessageProcessor` decrypts envelope, determines `ContentType`
//! 2. `HandlerRegistry` routes to handler by `ContentType`
//! 3. Handler parses protobuf, validates, emits `DomainEvent`
//! 4. Subscribers (state stores, UI) receive events via broadcast channel
//!
//! # Adding New Handlers
//!
//! 1. Create handler struct implementing `InboxHandler`
//! 2. Add to `HandlerRegistry::new()` or use `registry.register()`
//! 3. Add corresponding `DomainEvent` variant if needed
//!
//! # Flow
//!
//! 1. `InboxPoller` fetches encrypted envelopes from server
//! 2. Messages pass through `MessageValidationPipeline` (dedup, validate, order)
//! 3. `MessageProcessor` decrypts and determines content type
//! 4. Failed messages go to `LocalQueue` for retry
//! 5. `HandlerRegistry` routes to appropriate handler
//! 6. Processed messages are acknowledged to server

pub mod config;
pub mod error;
pub mod events;
pub mod handlers;
pub mod poller;
pub mod polling;
pub mod processor;
pub mod provider;
pub mod queue;
pub mod state;
pub mod streaming;

// Re-export main types for convenience
pub use config::{InboxConfig, InboxConfigBuilder};
pub use error::InboxError;
pub use events::InboxEvent;
pub use poller::InboxPoller;
pub use processor::{ContentType, MessageProcessor, ProcessedMessage, ProcessingError, ProcessingResult};
pub use provider::DeviceInboxProvider;
pub use queue::{LocalQueue, QueuedInboxMessage};
pub use state::{InboxState, InboxStats};

// Re-export handler types
pub use handlers::{
    AttachmentHandler, ChatHandler, DomainEvent, HandlerContext, HandlerError, HandlerOutcome,
    HandlerRegistry, HandlerResult, InboxHandler, KeyExchangeHandler, PinHandler, ReactionHandler,
    ReceiptHandler, SenderKeyHandler, SyncHandler, TypingHandler,
};

// Re-export streaming handlers
pub use streaming::{spawn_dm_bootstrap_handler_from_bus, spawn_user_events_stream};
