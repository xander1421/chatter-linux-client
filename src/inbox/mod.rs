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

// Re-export handler types
pub use handlers::{
    AttachmentHandler, ChatHandler, DomainEvent, HandlerContext, HandlerError, HandlerOutcome,
    HandlerRegistry, HandlerResult, InboxHandler, KeyExchangeHandler, PinHandler, ReactionHandler,
    ReceiptHandler, SenderKeyHandler, SyncHandler, TypingHandler,
};
