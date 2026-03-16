//! Pin event persistence handler.
//!
//! Subscribes to pin/unpin events from the event bus and persists them to the database
//! so that pin state survives app restarts.

use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{debug, error};

use chatter_linux::events::EventBus;
use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::services::message::MessageService;

/// Spawn a task that listens for pin events and persists them to the database.
///
/// This ensures that when pin/unpin events are received from other users,
/// the pin state is saved to the database and survives app restarts.
pub fn spawn_pin_persistence_handler(
    event_bus: EventBus,
    message_service: Arc<MessageService>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = event_bus.subscribe_domain();
        
        loop {
            match rx.recv().await {
                Ok(event) => {
                    match event {
                        DomainEvent::MessagePinned { message_id, .. } => {
                            debug!(?message_id, "Persisting pin state to database");
                            if let Err(e) = message_service.set_message_pinned(&message_id, true) {
                                error!(?message_id, error = %e, "Failed to persist pin state");
                            }
                        }
                        DomainEvent::MessageUnpinned { message_id, .. } => {
                            debug!(?message_id, "Persisting unpin state to database");
                            if let Err(e) = message_service.set_message_pinned(&message_id, false) {
                                error!(?message_id, error = %e, "Failed to persist unpin state");
                            }
                        }
                        _ => {
                            // Ignore other events
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    debug!("Pin persistence handler lagged by {} events", count);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!("Pin persistence handler: event bus closed");
                    break;
                }
            }
        }
    })
}