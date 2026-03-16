//! Event system for decoupled communication.
//!
//! Provides unified event routing between application layers:
//!
//! - **EventBus**: Central pub/sub for domain and UI events
//! - **DomainEvent**: Business logic events (re-exported from inbox::handlers)
//! - **UiEvent**: UI-specific events (navigation, modals, etc.)
//! - **EventHandlers**: Bridge between domain events and state stores
//!
//! # Architecture
//!
//! ```text
//! +-----------------+       +-------------+       +----------------+
//! | Network/Inbox   | ----> | EventBus    | ----> | EventHandlers  |
//! | (DomainEvents)  |       |             |       | (State Updates)|
//! +-----------------+       +-------------+       +----------------+
//!                                  |
//!                                  v
//!                           +-----------+
//!                           | UI Layer  |
//!                           | (UiEvent) |
//!                           +-----------+
//! ```
//!
//! # Example
//!
//! ```ignore
//! use chatter_core::events::{EventBus, DomainEvent, UiEvent, EventHandlers};
//! use chatter_core::state::*;
//!
//! // Create event bus
//! let event_bus = EventBus::new();
//!
//! // Create state stores
//! let message_state = MessageState::new();
//! let streaming_state = StreamingState::new();
//! // ... other states
//!
//! // Create and spawn event handlers
//! let handlers = EventHandlers::new(message_state, chat_state, ...);
//! handlers.spawn(event_bus.clone());
//!
//! // UI subscribes to events
//! let mut ui_rx = event_bus.subscribe_ui();
//!
//! // Domain events flow from inbox handlers through the bus
//! event_bus.emit_domain(DomainEvent::MessageReceived { message });
//! ```

pub mod bus;
pub mod domain_events;
pub mod handlers;
pub mod pin_persistence;
pub mod ui_events;

// Re-export EventBus
pub use bus::EventBus;

// Re-export DomainEvent from inbox::handlers (the authoritative definition)
// Do NOT duplicate - use the canonical definition
pub use crate::inbox::handlers::DomainEvent;

// Re-export UI events
pub use ui_events::{SettingsTab, SidebarSection, ToastLevel, UiEvent};

// Re-export EventHandlers
pub use handlers::EventHandlers;
