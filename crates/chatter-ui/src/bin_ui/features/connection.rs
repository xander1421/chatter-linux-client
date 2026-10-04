//! Server-reachability wiring.
//!
//! Pushes chatter-core's `OfflineState` into the Slint properties the connection
//! banner and the sign-in notices read, and pushes core's answer to "is there
//! anything usable without an account" alongside it.
//!
//! Both of the things this feeds make a claim to the user about whether the
//! server is there, so both are only ever as honest as this module. Two things
//! were wrong before and are worth not reintroducing:
//!
//! * It subscribed and waited. `OfflineState::subscribe` is a broadcast channel,
//!   so it delivers changes from that moment on and nothing about the present —
//!   and the two writes that existed both happened during boot, before
//!   `wire_all` ran. A client that started with the server down therefore
//!   rendered the Slint default, `"connected"`, forever. The snapshot below is
//!   pushed before the subscription for that reason.
//! * Nothing revised the state after boot, so even a correct first reading went
//!   stale within seconds. That is now
//!   `chatter-core/src/network/server_monitor.rs`'s job; this module only
//!   renders what it publishes.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::network::connection::ConnectionState;

use crate::MainWindow;

/// Map a core connection state to the string the Slint surfaces switch on.
fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "connected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Reconnecting { .. } => "reconnecting",
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Failed { .. } => "failed",
    }
}

/// Wire connection state to the UI banner.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // The present state, before anyone subscribes to changes to it.
    let state = ui_ctx.offline_state().connection_state();
    window.set_connection_status(state_label(state).into());
    window.set_server_unreachable(ui_ctx.server_unreachable());
    window.set_connection_reconnect_attempt(
        ui_ctx.offline_state().reconnect_attempt().unwrap_or(0) as i32,
    );
    window.set_connection_queue_count(ui_ctx.offline_state().queue_count() as i32);

    // Whether the sign-in screens have anywhere to offer as an alternative to
    // themselves. Core's call, not ours.
    window.set_account_free_surfaces(ui_ctx.has_account_free_surfaces());
    window.set_is_signed_in(ui_ctx.is_logged_in());

    let weak = window.as_weak();
    let mut rx = ui_ctx.offline_state().subscribe();
    let ui_ctx_for_task = ui_ctx.clone();

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

            let state_string = state_label(snapshot.connection_state).to_owned();
            // "Settled-unreachable" is core's distinction, not a `!= connected`
            // test here — see `ConnectionState::is_unreachable`.
            let unreachable = ui_ctx_for_task.server_unreachable();
            let attempt = snapshot.reconnect_attempt.unwrap_or(0) as i32;
            let queue = snapshot.queue_count as i32;
            // Re-read rather than cache: the mesh can start, or fail, after this
            // task began, and an offer to use it has to be current at the moment
            // it is made.
            let account_free = ui_ctx_for_task.has_account_free_surfaces();
            let signed_in = ui_ctx_for_task.is_logged_in();
            let weak = weak.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_connection_status(state_string.into());
                    w.set_server_unreachable(unreachable);
                    w.set_connection_reconnect_attempt(attempt);
                    w.set_connection_queue_count(queue);
                    w.set_account_free_surfaces(account_free);
                    w.set_is_signed_in(signed_in);
                }
            });
        }
    });
}
