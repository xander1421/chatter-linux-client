//! Event system for decoupled communication.
//!
//! Provides unified event routing between application layers:
//!
//! - **EventBus**: Central pub/sub for domain events
//! - **DomainEvent**: Business logic events (re-exported from inbox::handlers)
//! - **EventHandlers**: Bridge between domain events and state stores
//!
//! # Architecture
//!
//! ```text
//! +-----------------+       +-------------+       +----------------+
//! | Network/Inbox   | ----> | EventBus    | ----> | EventHandlers  |
//! | (DomainEvents)  |       |             |       | (State Updates)|
//! +-----------------+       +-------------+       +----------------+
//! ```
//!
//! # Example
//!
//! ```ignore
//! use chatter_core::events::{EventBus, DomainEvent, EventHandlers};
//! use chatter_core::state::*;
//!
//! // Create event bus
//! let event_bus = EventBus::new();
//!
//! // Create state stores
//! let message_state = MessageState::new();
//! let chat_state = ChatState::new();
//! // ... other states
//!
//! // Create and spawn event handlers
//! let handlers = EventHandlers::new(message_state, chat_state);
//! handlers.spawn(event_bus.clone());
//!
//! // Domain events flow from inbox handlers through the bus
//! event_bus.emit_domain(DomainEvent::MessageReceived { message });
//! ```

pub mod bus;
pub mod handlers;

// Re-export EventBus
pub use bus::EventBus;

// Re-export DomainEvent from inbox::handlers (the authoritative definition)
// Do NOT duplicate - use the canonical definition
pub use crate::inbox::handlers::{AttachmentUploadState, DomainEvent};

// Re-export EventHandlers
pub use handlers::EventHandlers;
