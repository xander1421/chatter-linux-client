//! Domain event handlers for calls.
//!
//! Handles call key answer and group call key events.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_linux::app::UIContext;
use chatter_linux::inbox::DomainEvent;
use crate::MainWindow;

use super::key_exchange::handle_call_key_answer;

/// Wire call key answer handler to process incoming answer events.
///
/// Subscribes to `DomainEvent::CallKeyAnswerReceived` from the event bus
/// and completes the ephemeral key exchange for the caller.
pub fn wire_call_key_answer_handler(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let call_service = ui_ctx.call_service.clone();
    let identity_manager = ui_ctx.identity_manager.clone();
    let key_store = ui_ctx.key_store.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();
    let weak = window.as_weak();

    slint::spawn_local(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                DomainEvent::CallKeyAnswerReceived {
                    call_id,
                    peer_id,
                    ephemeral_public_key,
                } => {
                    tracing::info!(
                        call_id = %call_id,
                        peer_id = %peer_id,
                        "Processing call key answer"
                    );

                    // Get the call service
                    let service = match call_service.get() {
                        Some(s) => s.clone(),
                        None => {
                            tracing::error!("No CallService available for key answer handling");
                            continue;
                        }
                    };

                    // Handle the answer on the UI thread
                    if let Some(w) = weak.upgrade() {
                        handle_call_key_answer(
                            &w.as_weak(),
                            &service,
                            &identity_manager,
                            &key_store,
                            ephemeral_public_key,
                            call_id,
                            peer_id,
                        );
                    }
                }
                DomainEvent::GroupCallKeyReceived {
                    call_id,
                    sender_id: _,
                    key_material,
                    key_epoch,
                    channel_id: _,
                } => {
                    tracing::info!(
                        call_id = %call_id,
                        key_epoch = key_epoch,
                        "Processing group call key update"
                    );

                    // Update LiveKit encryption key
                    if let Some(service) = call_service.get() {
                        let service = service.clone();
                        let key = key_material.to_vec();

                        tokio::spawn(async move {
                            let success = service.set_encryption_key(key).await;
                            if success {
                                tracing::info!(
                                    call_id = %call_id,
                                    key_epoch = key_epoch,
                                    "Group call key updated"
                                );
                            } else {
                                tracing::error!("Failed to update group call key");
                            }
                        });
                    }
                }
                _ => {} // Ignore other events
            }
        }
    }).unwrap();
}
