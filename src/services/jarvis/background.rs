//! Background processing for Jarvis.
//!
//! Handles:
//! - Event bus subscription for passive learning
//! - Periodic memory maintenance (decay, compression)

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::domain::jarvis::JarvisContext;
use crate::events::{DomainEvent, EventBus};

use super::service::JarvisService;

/// Background worker for Jarvis.
///
/// Spawns tasks for:
/// - Observing messages from the event bus
/// - Periodic maintenance (decay, pruning)
pub struct JarvisBackground;

impl JarvisBackground {
    /// Spawn the message observer task.
    ///
    /// Subscribes to the event bus and observes messages for learning.
    pub fn spawn_observer(
        jarvis: Arc<JarvisService>,
        event_bus: EventBus,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut rx = event_bus.subscribe_domain();

            tracing::info!("Jarvis observer started");

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        Self::handle_event(&jarvis, event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "Jarvis observer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("Jarvis observer: event bus closed");
                        break;
                    }
                }
            }
        })
    }

    /// Spawn the maintenance task.
    ///
    /// Periodically applies decay and prunes old data.
    pub fn spawn_maintenance(
        jarvis: Arc<JarvisService>,
        maintenance_interval: Duration,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut timer = interval(maintenance_interval);

            tracing::info!(
                interval_secs = maintenance_interval.as_secs(),
                "Jarvis maintenance task started"
            );

            loop {
                timer.tick().await;

                // Apply fact decay (threshold 0.1 = 10% confidence minimum)
                match jarvis.apply_fact_decay(0.1) {
                    Ok(pruned) => {
                        if pruned > 0 {
                            tracing::debug!(pruned = pruned, "Jarvis maintenance: pruned facts");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Jarvis maintenance: decay failed");
                    }
                }

                // Prune old graph data (30 days)
                match jarvis.prune_graph(30) {
                    Ok((mentions, entities)) => {
                        if mentions > 0 || entities > 0 {
                            tracing::debug!(
                                mentions = mentions,
                                entities = entities,
                                "Jarvis maintenance: pruned graph data"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Jarvis maintenance: graph prune failed");
                    }
                }
            }
        })
    }

    /// Handle a domain event.
    async fn handle_event(jarvis: &JarvisService, event: DomainEvent) {
        if let DomainEvent::MessageReceived { message } = event {
            let context = JarvisContext::channel(message.channel_id);
            let author_id = message.sender_id;
            let message_id = message.id.to_string();

            // Get message text
            let content = Self::extract_message_text(&message);
            if content.is_empty() {
                return;
            }

            // Observe the message for learning
            if let Err(e) = jarvis
                .observe_message(&context, &content, author_id.as_ref(), Some(&message_id))
                .await
            {
                tracing::debug!(error = %e, "Failed to observe message");
            }
        }
        // Other events are ignored
    }

    /// Extract text content from a message.
    fn extract_message_text(message: &crate::domain::Message) -> String {
        match &message.content {
            crate::domain::MessageContent::Text(text) => text.clone(),
            crate::domain::MessageContent::Mixed(mixed) => mixed.text.clone().unwrap_or_default(),
            crate::domain::MessageContent::System(sys) => sys.description(),
            _ => String::new(),
        }
    }
}

/// Builder for configuring and spawning Jarvis background tasks.
pub struct JarvisBackgroundBuilder {
    jarvis: Arc<JarvisService>,
    event_bus: Option<EventBus>,
    maintenance_interval: Duration,
}

impl JarvisBackgroundBuilder {
    /// Create a new builder.
    pub fn new(jarvis: Arc<JarvisService>) -> Self {
        Self {
            jarvis,
            event_bus: None,
            maintenance_interval: Duration::from_secs(24 * 60 * 60), // Daily
        }
    }

    /// Set the event bus for message observation.
    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Set the maintenance interval.
    pub fn with_maintenance_interval(mut self, interval: Duration) -> Self {
        self.maintenance_interval = interval;
        self
    }

    /// Spawn all configured background tasks.
    ///
    /// Returns handles to the spawned tasks.
    pub fn spawn(self) -> JarvisBackgroundHandles {
        let observer = self.event_bus.map(|bus| {
            JarvisBackground::spawn_observer(self.jarvis.clone(), bus)
        });

        let maintenance = JarvisBackground::spawn_maintenance(
            self.jarvis.clone(),
            self.maintenance_interval,
        );

        JarvisBackgroundHandles {
            observer,
            maintenance,
        }
    }
}

/// Handles to spawned Jarvis background tasks.
pub struct JarvisBackgroundHandles {
    /// Observer task handle (None if no event bus provided).
    pub observer: Option<JoinHandle<()>>,
    /// Maintenance task handle.
    pub maintenance: JoinHandle<()>,
}

impl JarvisBackgroundHandles {
    /// Abort all background tasks.
    pub fn abort(&self) {
        if let Some(observer) = &self.observer {
            observer.abort();
        }
        self.maintenance.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_default_interval() {
        let jarvis = Arc::new(create_test_jarvis());
        let builder = JarvisBackgroundBuilder::new(jarvis);
        assert_eq!(builder.maintenance_interval, Duration::from_secs(24 * 60 * 60));
    }

    fn create_test_jarvis() -> JarvisService {
        // This would need proper mocking in a real test
        panic!("Test not implemented - requires database mocking")
    }
}
