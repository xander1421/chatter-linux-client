//! Domain event handlers for calls.
//!
//! Handles call key answer events.

use std::sync::Arc;

use slint::ComponentHandle;

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::client::events::CallKeyEvent;

use super::key_exchange::handle_call_key_answer;

/// Wire call key answer handler to process incoming answer events.
///
/// Subscribes to the typed [`CallKeyEvent`] stream from [`ChatterClient`]
/// and completes the ephemeral key exchange for the caller (via core).
pub fn wire_call_key_answer_handler(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let mut event_rx = ui_ctx.subscribe_call_key_events();
    let weak = window.as_weak();

    slint::spawn_local(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        dropped_count = n,
                        "Call key event receiver lagged, continuing"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            match event {
                CallKeyEvent::AnswerReceived {
                    call_id,
                    peer_id,
                    ephemeral_public_key,
                } => {
                    tracing::info!(
                        call_id = %call_id,
                        peer_id = %peer_id,
                        "Processing call key answer"
                    );

                    // Handle the answer on the UI thread (delegates to core)
                    if weak.upgrade().is_some() {
                        handle_call_key_answer(
                            &weak,
                            &ui_ctx,
                            ephemeral_public_key,
                            call_id,
                            peer_id,
                        )
                        .await;
                    }
                }
            }
        }
    })
    .unwrap();
}
