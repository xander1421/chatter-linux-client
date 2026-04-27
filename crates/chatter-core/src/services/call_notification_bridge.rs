//! Bridge: call orchestrator events → domain event bus.
//!
//! `CallWorker` emits its own `CallOrchestratorEvent` stream consumed by
//! the in-app call modal. Desktop notification delivery lives on the
//! `DomainEvent` bus though, so we need a thin translator task that
//! watches for `IncomingPresented` and republishes as
//! `DomainEvent::IncomingCall`. Keeps `call_worker` free of `EventBus`
//! coupling and avoids duplicating delivery logic in the UI.

#[cfg(feature = "media")]
use tokio::sync::broadcast;

#[cfg(feature = "media")]
use crate::events::bus::EventBus;
#[cfg(feature = "media")]
use crate::inbox::handlers::DomainEvent;
#[cfg(feature = "media")]
use crate::services::call_worker::{CallOrchestratorEvent, CallWorkerHandle};

/// Spawn a task that mirrors `CallOrchestratorEvent::IncomingPresented`
/// onto the domain event bus as `DomainEvent::IncomingCall`. Exits when
/// the underlying broadcast channel closes.
#[cfg(feature = "media")]
pub fn spawn_call_notification_bridge(
    call: &CallWorkerHandle,
    event_bus: EventBus,
) -> tokio::task::JoinHandle<()> {
    let mut rx = call.subscribe_call_events();
    let sender = event_bus.domain_sender();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(CallOrchestratorEvent::IncomingPresented {
                    call,
                    peer_id,
                    peer_name,
                    is_video,
                    channel_context,
                    ..
                }) => {
                    let _ = sender.send(DomainEvent::IncomingCall {
                        call_id: call.as_str().to_string(),
                        peer_id,
                        peer_name,
                        is_video,
                        channel_context,
                    });
                }
                // Other orchestrator events (Withdrawn, Accepted, Ended,
                // etc.) don't need a toast — UI handles in-app state
                // directly. Skip without logging to keep the bridge quiet.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "call notification bridge lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("call notification bridge: channel closed, exiting");
                    break;
                }
            }
        }
    })
}
