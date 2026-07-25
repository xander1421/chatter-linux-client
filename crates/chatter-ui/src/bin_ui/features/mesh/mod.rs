//! bitchat mesh surfaces.
//!
//! A mesh conversation is an ordinary conversation with an extraordinary
//! transport, so this module is mostly wiring: the rows go into the existing
//! sidebar, selection opens the existing message list, and the composer's
//! existing `send` callback is intercepted before it reaches the server path.
//! `ui/mesh/` holds only what a mesh conversation has to say that a server one
//! does not.
//!
//! # What the surfaces are for
//!
//! - **No server, no internet.** Stated in the sidebar caption and again in the
//!   strip above an open mesh conversation, rather than left to be inferred.
//! - **Peer count, not a connection dot.** `MeshStatus::connected_peers` drives
//!   both. Zero renders as "none nearby" in muted text — a normal state of a
//!   mesh, never an error.
//! - **The geohash cell is public and location-scoped.** It is published with
//!   every message, so it is shown verbatim on the row, in the strip, and in
//!   the composer placeholder.
//! - **Delivery is best-effort and may be deferred.** `MeshDelivery` maps to
//!   words rather than ticks (see [`core_api::delivery_label`]).
//!
//! # Where the line with core sits
//!
//! Everything factual is core's: which conversations exist, their titles,
//! previews, unread counts, peer counts and reachability, which transport a
//! send takes, and whether a cell is a cell. This module chooses wording and
//! glyphs, and nothing else. [`core_api`] is the single seam.

pub mod core_api;
mod conversation;
mod send;

pub use conversation::open;
pub use send::intercept_send;

use std::sync::{Arc, Mutex, OnceLock};

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::mesh::{MeshConversation, MeshEvent};

use crate::{MainWindow, MeshConversationData, MeshGlobal, run_ui};

/// The mesh conversation currently open, if any.
///
/// `ActiveConversation` in core has no mesh arm — mesh rows are keyed by a
/// transport scope, not by a `ChannelId` the server ever heard of — so the
/// open mesh scope is tracked here and cleared the moment any server-backed
/// conversation is selected (see [`clear_active`]).
static ACTIVE: Mutex<Option<MeshConversation>> = Mutex::new(None);

/// This device's derived mesh account id, cached after the first lookup.
static LOCAL_USER: OnceLock<String> = OnceLock::new();

/// The mesh conversation currently open.
#[must_use]
pub fn active() -> Option<MeshConversation> {
    ACTIVE.lock().ok().and_then(|guard| guard.clone())
}

pub(super) fn set_active(conversation: Option<MeshConversation>) {
    if let Ok(mut guard) = ACTIVE.lock() {
        *guard = conversation;
    }
}

/// Forget the open mesh conversation.
///
/// Called from `sidebar::refresh_selection` whenever a server-backed
/// conversation is selected: without it the composer would keep routing to the
/// mesh after the user had moved on.
pub fn clear_active(window: &MainWindow) {
    if active().is_none() {
        return;
    }
    set_active(None);
    let global = window.global::<MeshGlobal>();
    global.set_active_id("".into());
    global.set_active_kind("".into());
    global.set_active_title("".into());
    global.set_active_scope("".into());
    global.set_active_reachable(false);
    global.set_active_verified(false);
    deselect_rows(window);
}

/// This device's mesh account id as a string, for "is this message mine?".
pub(super) fn local_user_id(ui_ctx: &Arc<ChatterClient>) -> String {
    if let Some(cached) = LOCAL_USER.get() {
        return cached.clone();
    }
    let resolved = core_api::local_user_id(ui_ctx)
        .map(|id| id.to_string())
        .unwrap_or_default();
    // Only cache a real answer: before the mesh identity is loaded the lookup
    // can legitimately come back empty, and caching that would make every
    // message we ever send look like someone else's.
    if !resolved.is_empty() {
        let _ = LOCAL_USER.set(resolved.clone());
    }
    resolved
}

/// Wire the mesh surfaces: flip the Slint feature flag, hook the callbacks,
/// push the first state, and start listening.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let global = window.global::<MeshGlobal>();
    global.set_enabled(true);

    // Selection → open the conversation in the ordinary message list.
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_conversation_selected(move |id| {
            if let Some(w) = weak.upgrade() {
                conversation::open(&w, &ui_ctx, id.as_str());
            }
        });
    }

    // Join a geohash cell. Whether the string is a cell at all is core's call.
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_join_geohash(move |cell| {
            let cell = cell.trim().to_string();
            if cell.is_empty() {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            match core_api::join(&ui_ctx, &cell) {
                Ok(()) => {
                    w.global::<MeshGlobal>().set_notice("".into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => {
                    tracing::warn!(%error, cell = %cell, "mesh: could not join cell");
                    w.global::<MeshGlobal>()
                        .set_notice(format!("Could not join {cell}: {error}").into());
                }
            }
        });
    }

    // Leave a cell. Only offered next to the cell name, so it is never
    // ambiguous which one is being left.
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_leave_geohash(move |cell| {
            let cell = cell.trim().to_string();
            if cell.is_empty() {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            if let Err(error) = core_api::leave(&ui_ctx, &cell) {
                tracing::warn!(%error, cell = %cell, "mesh: could not leave cell");
                w.global::<MeshGlobal>()
                    .set_notice(format!("Could not leave {cell}: {error}").into());
                return;
            }
            // The cell we were reading just went away.
            if matches!(active(), Some(MeshConversation::Geohash(ref open)) if *open == cell) {
                clear_active(&w);
            }
            refresh_rows(&w, &ui_ctx);
        });
    }

    push_status(window, ui_ctx);
    refresh_rows(window, ui_ctx);
    spawn_event_pump(window.as_weak(), ui_ctx.clone());
}

/// Re-read the conversation list from core and republish it.
///
/// Called after anything that can change a row: a message, a peer change, a
/// join, a leave, a selection.
pub fn refresh_rows(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let open = active();
    let rows = core_api::rows(ui_ctx);

    // The strip above the conversation shows live values (peer count,
    // reachability), so re-derive them from the same rows.
    if let Some(ref open) = open {
        if let Some(row) = rows.iter().find(|row| row.conversation == *open) {
            let global = window.global::<MeshGlobal>();
            global.set_active_title(row.title.clone().into());
            global.set_active_reachable(row.reachable);
            global.set_active_verified(row.verified);
        }
    }

    let items: Vec<MeshConversationData> = rows
        .into_iter()
        .map(|row| {
            let is_selected = open.as_ref().is_some_and(|open| *open == row.conversation);
            MeshConversationData {
                id: row.id.into(),
                kind: row.kind.into(),
                title: row.title.into(),
                scope: row.scope.into(),
                timestamp: row.timestamp.into(),
                unread_count: row.unread_count,
                reachable: row.reachable,
                verified: row.verified,
                is_selected,
            }
        })
        .collect();

    window
        .global::<MeshGlobal>()
        .set_conversations(ModelRc::new(VecModel::from(items)));
}

/// Drop the selection highlight from every mesh row without asking core again.
fn deselect_rows(window: &MainWindow) {
    use slint::Model;

    let model = window.global::<MeshGlobal>().get_conversations();
    let rows: Vec<MeshConversationData> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .map(|mut row| {
            row.is_selected = false;
            row
        })
        .collect();
    window
        .global::<MeshGlobal>()
        .set_conversations(ModelRc::new(VecModel::from(rows)));
}

/// Push transport state into the Slint global.
fn push_status(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    apply_status(window, core_api::status(ui_ctx));
}

fn apply_status(window: &MainWindow, status: chatter_core::services::mesh::MeshStatus) {
    let global = window.global::<MeshGlobal>();
    global.set_radio_running(status.radio_running);
    global.set_peer_count(i32::try_from(status.connected_peers).unwrap_or(i32::MAX));
    global.set_relay_count(i32::try_from(status.connected_relays).unwrap_or(i32::MAX));
}

/// Follow the mesh event stream for the life of the window.
fn spawn_event_pump(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    let mut events = core_api::subscribe(&ui_ctx);
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => handle_event(event, &weak, &ui_ctx),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    // A burst outran the subscriber. The rows are rebuilt from
                    // core rather than accumulated here, so a resync is enough.
                    tracing::warn!(dropped, "mesh: event receiver lagged");
                    let weak = weak.clone();
                    let ui_ctx = ui_ctx.clone();
                    run_ui!(weak, |w| refresh_rows(&w, &ui_ctx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn handle_event(event: MeshEvent, weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    match event {
        MeshEvent::Message(message) => {
            let message = *message;
            let is_open = active().as_ref() == Some(&message.conversation);

            // It arrived into a conversation that is on screen, so it has been
            // read. Done here, on the event-pump task, rather than from inside
            // the UI closure — this touches the message store.
            if is_open && !message.is_local {
                if let Err(error) = core_api::mark_read(ui_ctx, &message.conversation) {
                    tracing::debug!(%error, "mesh: could not mark the conversation read");
                }
            }

            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                if is_open {
                    conversation::append_incoming(&w, &ui_ctx, &message);
                }
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::Delivery { message_id, state, .. } => {
            let label = core_api::delivery_label(state);
            let domain_id =
                chatter_core::services::mesh::mapping::wire_message_id(&message_id).to_string();
            run_ui!(weak, |w| conversation::apply_delivery(&w, &domain_id, label));
        }
        MeshEvent::Peers(_) => {
            // The count itself comes from `status()`, which is the same number
            // the rest of the mesh reasons about; the peer list only tells us
            // it is time to ask again.
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                push_status(&w, &ui_ctx);
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::Status(status) => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                apply_status(&w, status);
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::GeohashJoined(_) | MeshEvent::GeohashLeft(_) => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| refresh_rows(&w, &ui_ctx));
        }
        MeshEvent::Degraded(reason) => {
            run_ui!(weak, |w| w
                .global::<MeshGlobal>()
                .set_notice(reason.clone().into()));
        }
    }
}
