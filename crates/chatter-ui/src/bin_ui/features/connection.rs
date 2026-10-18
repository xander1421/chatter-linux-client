//! Server-reachability wiring.
//!
//! Pushes chatter-core's `OfflineState` into the Slint properties the connection
//! banner and the sign-in notices read, and pushes core's answer to "is there
//! anything usable without an account" alongside it.
//!
//! Both of the things this feeds make a claim to the user about whether the
//! server is there, so both need an authoritative current reading.
//!
//! Subscribe before reading the present state, and recover from broadcast lag
//! by reading that authoritative state again. Events are wakeups, not a second
//! source of truth for the label and unreachable flag.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::{ChatterClient, ConnectionState};
#[cfg(test)]
use chatter_core::state::{OfflineState, OfflineStateData, StateSnapshot};
use tokio::sync::broadcast;

use crate::MainWindow;

#[cfg(test)]
fn subscribe_current(
    state: &OfflineState,
) -> (
    broadcast::Receiver<StateSnapshot<OfflineStateData>>,
    ConnectionState,
) {
    subscribe_then_read(state, || state.connection_state())
}

#[cfg(test)]
fn subscribe_then_read(
    state: &OfflineState,
    read: impl FnOnce() -> ConnectionState,
) -> (
    broadcast::Receiver<StateSnapshot<OfflineStateData>>,
    ConnectionState,
) {
    let rx = state.subscribe();
    (rx, read())
}

#[cfg(test)]
async fn next_current(
    state: &OfflineState,
    rx: &mut broadcast::Receiver<StateSnapshot<OfflineStateData>>,
) -> Option<ConnectionState> {
    match rx.recv().await {
        Ok(_) => Some(state.connection_state()),
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!(
                dropped = n,
                "Connection state receiver lagged; resynchronizing"
            );
            Some(state.connection_state())
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

fn reconnect_attempt(state: ConnectionState) -> i32 {
    match state {
        ConnectionState::Reconnecting { attempt } => attempt as i32,
        _ => 0,
    }
}

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

fn presentation(client: &ChatterClient, state: ConnectionState) -> (&'static str, bool, i32) {
    (
        state_label(state),
        client.server_unreachable_for_state(state),
        reconnect_attempt(state),
    )
}

/// Wire connection state to the UI banner.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut rx = ui_ctx.subscribe_offline_snapshots();
    let state = ui_ctx.offline_snapshot().connection_state;
    let (label, unreachable, attempt) = presentation(ui_ctx, state);
    window.set_connection_status(label.into());
    window.set_server_unreachable(unreachable);
    window.set_connection_reconnect_attempt(attempt);

    // Whether the sign-in screens have anywhere to offer as an alternative to
    // themselves. Core's call, not ours.
    window.set_account_free_surfaces(ui_ctx.has_account_free_surfaces());
    window
        .global::<crate::AuthFlow>()
        .set_is_signed_in(ui_ctx.is_logged_in());
    if ui_ctx.is_logged_in() {
        match ui_ctx.unacknowledged_recovery_codes() {
            Ok(Some(pending)) if !pending.codes.is_empty() => {
                let model: Vec<slint::SharedString> = pending
                    .codes
                    .iter()
                    .map(|code| code.as_str().into())
                    .collect();
                window
                    .global::<crate::AuthFlow>()
                    .set_recovery_codes(slint::ModelRc::new(slint::VecModel::from(model)));
                window
                    .global::<crate::AuthFlow>()
                    .set_recovery_codes_regenerated(pending.regenerated);
                crate::bin_ui::features::back::push(window, "recovery-codes");
            }
            Ok(_) => {}
            Err(error) => tracing::error!(%error, "Could not load pending recovery codes"),
        }
    }

    let weak = window.as_weak();
    let ui_ctx_for_task = ui_ctx.clone();

    tokio::spawn(async move {
        loop {
            let state = match rx.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    ui_ctx_for_task.offline_snapshot().connection_state
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let (label, unreachable, attempt) = presentation(&ui_ctx_for_task, state);
            let state_string = label.to_owned();
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
                    w.set_account_free_surfaces(account_free);
                    w.global::<crate::AuthFlow>().set_is_signed_in(signed_in);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signed_out_deferred_presentation_then_explicit_contact_failure() {
        let dir = tempfile::tempdir().unwrap();
        // Local marker selects the existing file-backed test storage path,
        // without a process-global environment change or an OS keyring.
        std::fs::create_dir(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/no-keyring"), "").unwrap();
        let client = ChatterClient::builder()
            .server_url("http://127.0.0.1:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        assert!(
            !client
                .startup_connect(std::time::Duration::from_secs(1))
                .await
        );
        let _rx = client.subscribe_offline_snapshots();
        let captured = client.offline_snapshot().connection_state;
        assert_eq!(presentation(&client, captured), ("disconnected", false, 0));
        assert!(client.connect().await.is_err());
        let failed = client.offline_snapshot().connection_state;
        assert_eq!(presentation(&client, failed), ("disconnected", true, 0));
        client.set_offline_connection_state_for_testing(ConnectionState::Connected);
        assert_eq!(presentation(&client, failed), ("disconnected", true, 0));
        assert_eq!(
            presentation(&client, ConnectionState::Connected),
            ("connected", false, 0)
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn startup_transition_after_snapshot_remains_observable() {
        let state = OfflineState::new();
        let (mut rx, initial) = subscribe_then_read(&state, || {
            let initial = state.connection_state();
            // Force the old snapshot-before-subscribe transition window.
            state.set_connection_state(ConnectionState::Connected);
            initial
        });
        assert_eq!(initial, ConnectionState::Disconnected);
        let current = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_current(&state, &mut rx),
        )
        .await
        .expect("startup transition must remain observable")
        .unwrap();
        assert_eq!(state_label(current), "connected");
        assert!(!current.is_unreachable());
    }

    #[tokio::test]
    async fn lag_recovers_latest_state_without_another_publication() {
        let state = OfflineState::new();
        let (mut rx, _) = subscribe_current(&state);
        for _ in 0..64 {
            state.set_connection_state(ConnectionState::Disconnected);
        }
        state.set_connection_state(ConnectionState::Connected);
        // First recv returns Lagged. Recovery must return immediately, rather
        // than waiting for another event or rendering retained stale events.
        let current = next_current(&state, &mut rx).await.unwrap();
        assert_eq!(state_label(current), "connected");
        assert!(!current.is_unreachable());
        let queued = next_current(&state, &mut rx).await.unwrap();
        assert_eq!(queued, ConnectionState::Connected);
    }
}
