//! Event bus for unified event routing.
//!
//! Provides centralized pub/sub for domain and UI events using broadcast channels.
//!
//! # Architecture
//!
//! ```text
//! +----------------+     +-------------+     +----------------+
//! | Inbox Handlers | --> | EventBus    | --> | EventHandlers  |
//! | (emit domain)  |     | (routes)    |     | (updates state)|
//! +----------------+     +-------------+     +----------------+
//!                              |
//!                              v
//!                        +-------------+
//!                        | UI Layer    |
//!                        | (subscribe) |
//!                        +-------------+
//! ```
//!
//! # Event Types
//!
//! - **DomainEvent**: Business logic events (message received, typing, etc.)
//! - **UiEvent**: UI-specific events (navigation, modals, toasts)

use tokio::sync::broadcast;

use crate::inbox::handlers::DomainEvent;
use crate::events::ui_events::UiEvent;

/// Channel capacity for domain events.
///
/// Higher capacity to handle bursts of incoming messages.
const DOMAIN_CHANNEL_CAPACITY: usize = 64;

/// Channel capacity for UI events.
///
/// Lower capacity as UI events are typically less frequent.
const UI_CHANNEL_CAPACITY: usize = 32;

/// Unified event bus for domain and UI events.
///
/// The EventBus provides centralized routing for all application events:
/// - Domain events from inbox handlers (message received, typing, etc.)
/// - UI events from the view layer (navigation, modals, etc.)
///
/// # Cloning
///
/// EventBus implements Clone efficiently - all clones share the same
/// underlying broadcast channels.
///
/// # Example
///
/// ```ignore
/// use chatter_linux::events::{EventBus, DomainEvent, UiEvent};
///
/// let bus = EventBus::new();
///
/// // Subscribe to domain events
/// let mut domain_rx = bus.subscribe_domain();
///
/// // Subscribe to UI events
/// let mut ui_rx = bus.subscribe_ui();
///
/// // Emit events
/// bus.emit_domain(DomainEvent::TypingStarted { user_id, channel_id });
/// bus.emit_ui(UiEvent::ChannelSelected { channel_id });
///
/// // Or get the sender for use in other components
/// let domain_tx = bus.domain_sender();
/// domain_tx.send(DomainEvent::MessageReceived { message })?;
/// ```
#[derive(Clone)]
pub struct EventBus {
    /// Sender for domain events.
    domain_tx: broadcast::Sender<DomainEvent>,
    /// Sender for UI events.
    ui_tx: broadcast::Sender<UiEvent>,
}

impl EventBus {
    /// Create a new event bus with default channel capacities.
    ///
    /// - Domain channel: 64 events
    /// - UI channel: 32 events
    #[must_use]
    pub fn new() -> Self {
        let (domain_tx, _) = broadcast::channel(DOMAIN_CHANNEL_CAPACITY);
        let (ui_tx, _) = broadcast::channel(UI_CHANNEL_CAPACITY);

        Self { domain_tx, ui_tx }
    }

    /// Get the domain event sender.
    ///
    /// Use this to pass to components that need to emit domain events,
    /// such as inbox handlers via `HandlerContext`.
    #[must_use]
    pub fn domain_sender(&self) -> broadcast::Sender<DomainEvent> {
        self.domain_tx.clone()
    }

    /// Get the UI event sender.
    ///
    /// Use this to pass to UI components that need to emit UI events.
    #[must_use]
    pub fn ui_sender(&self) -> broadcast::Sender<UiEvent> {
        self.ui_tx.clone()
    }

    /// Subscribe to domain events.
    ///
    /// Returns a receiver that will receive all domain events.
    /// Multiple subscribers can exist simultaneously.
    ///
    /// # Note
    ///
    /// The receiver does NOT receive events that were sent before subscription.
    /// Lagged events (when receiver is too slow) will result in `RecvError::Lagged`.
    #[must_use]
    pub fn subscribe_domain(&self) -> broadcast::Receiver<DomainEvent> {
        self.domain_tx.subscribe()
    }

    /// Subscribe to UI events.
    ///
    /// Returns a receiver that will receive all UI events.
    /// Multiple subscribers can exist simultaneously.
    #[must_use]
    pub fn subscribe_ui(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_tx.subscribe()
    }

    /// Emit a domain event to all subscribers.
    ///
    /// Returns the number of active receivers, or 0 if no subscribers.
    /// This is a convenience method - for high-throughput scenarios,
    /// use `domain_sender()` directly.
    pub fn emit_domain(&self, event: DomainEvent) -> usize {
        self.domain_tx.send(event).unwrap_or(0)
    }

    /// Emit a UI event to all subscribers.
    ///
    /// Returns the number of active receivers, or 0 if no subscribers.
    pub fn emit_ui(&self, event: UiEvent) -> usize {
        self.ui_tx.send(event).unwrap_or(0)
    }

    /// Get the number of active domain event subscribers.
    #[must_use]
    pub fn domain_subscriber_count(&self) -> usize {
        self.domain_tx.receiver_count()
    }

    /// Get the number of active UI event subscribers.
    #[must_use]
    pub fn ui_subscriber_count(&self) -> usize {
        self.ui_tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("domain_subscribers", &self.domain_tx.receiver_count())
            .field("ui_subscribers", &self.ui_tx.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::Message;

    #[test]
    fn test_event_bus_new() {
        let bus = EventBus::new();
        assert_eq!(bus.domain_subscriber_count(), 0);
        assert_eq!(bus.ui_subscriber_count(), 0);
    }

    #[test]
    fn test_event_bus_clone_shares_channels() {
        let bus1 = EventBus::new();
        let _rx = bus1.subscribe_domain();

        let bus2 = bus1.clone();
        // Both buses see the same subscriber count
        assert_eq!(bus2.domain_subscriber_count(), 1);
    }

    #[test]
    fn test_event_bus_default() {
        let bus = EventBus::default();
        assert_eq!(bus.domain_subscriber_count(), 0);
    }

    #[tokio::test]
    async fn test_emit_domain_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let count = bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });
        assert_eq!(count, 1);

        let event = rx.recv().await.unwrap();
        if let DomainEvent::TypingStarted {
            user_id: u,
            channel_id: c,
        } = event
        {
            assert_eq!(u, user_id);
            assert_eq!(c, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    #[tokio::test]
    async fn test_emit_ui_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_ui();

        let channel_id = ChannelId::new();

        let count = bus.emit_ui(UiEvent::ChannelSelected { channel_id });
        assert_eq!(count, 1);

        let event = rx.recv().await.unwrap();
        if let UiEvent::ChannelSelected { channel_id: c } = event {
            assert_eq!(c, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    #[tokio::test]
    async fn test_multiple_domain_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe_domain();
        let mut rx2 = bus.subscribe_domain();

        assert_eq!(bus.domain_subscriber_count(), 2);

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });

        // Both subscribers receive the event
        let event1 = rx1.recv().await.unwrap();
        let event2 = rx2.recv().await.unwrap();

        assert!(matches!(event1, DomainEvent::TypingStarted { .. }));
        assert!(matches!(event2, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_emit_returns_zero_with_no_subscribers() {
        let bus = EventBus::new();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let count = bus.emit_domain(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_domain_sender_works_independently() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();
        let tx = bus.domain_sender();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Send via the sender obtained from domain_sender()
        let _ = tx.send(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_message_received_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_domain();

        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let message = Message::new_text(channel_id, Some(user_id), "Hello!".to_string());
        let msg_id = message.id;

        bus.emit_domain(DomainEvent::MessageReceived {
            message: message.clone(),
        });

        let event = rx.recv().await.unwrap();
        if let DomainEvent::MessageReceived { message: m } = event {
            assert_eq!(m.id, msg_id);
            assert_eq!(m.channel_id, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    #[test]
    fn test_debug_impl() {
        let bus = EventBus::new();
        let _rx = bus.subscribe_domain();
        let debug_str = format!("{:?}", bus);
        assert!(debug_str.contains("EventBus"));
        assert!(debug_str.contains("domain_subscribers"));
    }
}
