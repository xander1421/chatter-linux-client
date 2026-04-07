//! Connection status banner wiring.
//!
//! Subscribes to `OfflineState` changes and updates the Slint
//! connection banner properties.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::network::connection::ConnectionState;

use crate::MainWindow;

/// Wire connection state to the UI banner.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let mut rx = ui_ctx.offline_state().subscribe();

    tokio::spawn(async move {
        loop {
            let snapshot = match rx.recv().await {
                Ok(snap) => snap,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "Connection state receiver lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            let state_str: &str = match snapshot.connection_state {
                ConnectionState::Connected => "connected",
                ConnectionState::Connecting => "connecting",
                ConnectionState::Reconnecting { .. } => "reconnecting",
                ConnectionState::Disconnected => "disconnected",
                ConnectionState::Failed { .. } => "failed",
            };

            let attempt = snapshot.reconnect_attempt.unwrap_or(0) as i32;
            let queue = snapshot.queue_count as i32;
            let state_string = state_str.to_owned();
            let weak = weak.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_connection_status(state_string.into());
                    w.set_connection_reconnect_attempt(attempt);
                    w.set_connection_queue_count(queue);
                }
            });
        }
    });
}
