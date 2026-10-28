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

use slint::{ComponentHandle, Model};

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

    // "last seen just now" ages while the DM stays open; recompute it every
    // half minute, as Telegram's header does, instead of only on events. The
    // dot too: a peer whose presence stops being refreshed turns offline here
    // with no event (`PRESENCE_STALE_SECS`).
    let weak_tick = window.as_weak();
    let ui_ctx_tick = ui_ctx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        let mut ticks: u32 = 0;
        loop {
            tick.tick().await;
            ticks = ticks.wrapping_add(1);
            // The chat list's dots expire the same way, every two minutes.
            if ticks % 4 == 0 {
                crate::bin_ui::features::sidebar::rebuild_chat_list(&weak_tick, &ui_ctx_tick);
                // And the Contacts rows: their dot and "last seen" line were
                // only patched on presence events, so an expired friend
                // stayed green there and "last seen 2 min ago" never aged.
                let friends: Vec<(slint::SharedString, slint::SharedString, slint::SharedString)> =
                    ui_ctx_tick
                        .listed_friends()
                        .iter()
                        .filter_map(|id| {
                            let (presence, at) = ui_ctx_tick.peer_last_seen(id)?;
                            Some((
                                id.to_string().into(),
                                presence.as_str().into(),
                                crate::bin_ui::converters::last_seen_label(presence, at).into(),
                            ))
                        })
                        .collect();
                let weak = weak_tick.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let rows = w.global::<crate::ModalHostState>().get_friends_list();
                    let mut changed = false;
                    for i in 0..rows.row_count() {
                        let Some(mut f) = rows.row_data(i) else { continue };
                        let Some((_, presence, label)) =
                            friends.iter().find(|(id, ..)| !f.is_anon && *id == f.user_id)
                        else {
                            continue;
                        };
                        if f.presence_status != *presence || f.last_seen_label != *label {
                            f.presence_status = presence.clone();
                            f.last_seen_label = label.clone();
                            rows.set_row_data(i, f);
                            changed = true;
                        }
                    }
                    if changed {
                        crate::bin_ui::features::modals::friends::refresh_new_chat_results(&w);
                    }
                });
            }
            let active = ui_ctx_tick.active_conversation();
            let chatter_core::state::ActiveConversation::ServerDm { peer_id } = &active else {
                continue;
            };
            let Some((presence, label)) = ui_ctx_tick.peer_last_seen(peer_id).map(|(presence, at)| {
                (presence, crate::bin_ui::converters::last_seen_label(presence, at))
            }) else {
                continue;
            };
            let weak = weak_tick.clone();
            let ui_ctx = ui_ctx_tick.clone();
            let posted = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade()
                    && ui_ctx.active_conversation() == active
                {
                    let pane = w.global::<crate::ChatPane>();
                    pane.set_header_presence_status(presence.as_str().into());
                    pane.set_header_last_seen(label.into());
                }
            });
            if posted.is_err() {
                break;
            }
        }
    });

    let weak = window.as_weak();
    let ui_ctx_for_task = ui_ctx.clone();

    tokio::spawn(async move {
        let mut previous_state = state;
        loop {
            let state = match rx.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    ui_ctx_for_task.offline_snapshot().connection_state
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let connection_changed = state != previous_state;
            previous_state = state;
            let (label, unreachable, attempt) = presentation(&ui_ctx_for_task, state);
            let state_string = label.to_owned();
            // Re-read rather than cache: the mesh can start, or fail, after this
            // task began, and an offer to use it has to be current at the moment
            // it is made.
            let account_free = ui_ctx_for_task.has_account_free_surfaces();
            let signed_in = ui_ctx_for_task.is_logged_in();
            let active_conversation = ui_ctx_for_task.active_conversation();
            // The header's "online / last seen" line follows the dot: a DM
            // opened while disconnected read "last seen 11 min ago" beside a
            // green dot once the connection came back (S9).
            let active_last_seen = match active_conversation {
                chatter_core::state::ActiveConversation::ServerDm { peer_id } => ui_ctx_for_task
                    .peer_last_seen(&peer_id)
                    .map(|(presence, at)| crate::bin_ui::converters::last_seen_label(presence, at)),
                _ => None,
            };
            let active_presence = ui_ctx_for_task.active_dm_presence();
            let weak_for_ui = weak.clone();
            let ui_ctx_for_ui = ui_ctx_for_task.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_ui.upgrade() {
                    w.set_connection_status(state_string.into());
                    w.set_server_unreachable(unreachable);
                    w.set_connection_reconnect_attempt(attempt);
                    w.set_account_free_surfaces(account_free);
                    w.global::<crate::AuthFlow>().set_is_signed_in(signed_in);
                    if let Some(presence) = active_presence
                        && ui_ctx_for_ui.active_conversation() == active_conversation
                    {
                        w.global::<crate::ChatPane>()
                            .set_header_presence_status(presence.as_str().into());
                        if let Some(label) = active_last_seen {
                            w.global::<crate::ChatPane>().set_header_last_seen(label.into());
                        }
                    }
                }
            });
            // The sidebar owns retained rows, so recompute them from core's
            // presence policy when reachability changes in either direction.
            if connection_changed {
                crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx_for_task);
            }
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
