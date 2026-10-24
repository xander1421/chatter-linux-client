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
//! - **Peer count, not a connection dot.** The nearby list from core drives
//!   the count too. Zero renders as "none nearby" in muted text — a normal
//!   state of a mesh, never an error.
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

mod attach;
mod conversation;
pub mod core_api;
mod send;

pub use attach::intercept_attachment;
pub use conversation::open;
pub use send::intercept_send;

use std::sync::{Arc, Mutex, OnceLock};

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::mesh::{MeshConversation, MeshEvent};

use crate::{
    MainWindow, MeshConversationData, MeshGlobal, MeshPeerData, PrivateChannelInviteData, run_ui,
};

/// The mesh conversation currently open, if any.
///
/// `ActiveConversation` in core has no mesh arm — mesh rows are keyed by a
/// transport scope, not by a `ChannelId` the server ever heard of — so the
/// open mesh scope is tracked here and cleared the moment any server-backed
/// conversation is selected (see [`clear_active`]).
// Keep identity and generation under one lock: returning to the same scope
// after navigation must not revive a picker launched in its previous session.
static ACTIVE: Mutex<(Option<MeshConversation>, u64)> = Mutex::new((None, 0));

/// This device's derived mesh account id, cached after the first lookup.
static LOCAL_USER: OnceLock<String> = OnceLock::new();

/// The mesh conversation currently open.
#[must_use]
pub fn active() -> Option<MeshConversation> {
    active_session().and_then(|(conversation, _)| conversation)
}

pub(super) fn active_session() -> Option<(Option<MeshConversation>, u64)> {
    ACTIVE.lock().ok().map(|guard| guard.clone())
}

pub(super) fn set_active(conversation: Option<MeshConversation>) {
    if let Ok(mut guard) = ACTIVE.lock() {
        guard.1 = guard.1.wrapping_add(1);
        guard.0 = conversation;
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
    global.set_private_leave_pending(false);
    global.set_active_reachable(false);
    global.set_active_verified(false);
    global.set_active_binding_label("Mesh peer · self-asserted name".into());
    global.set_identity_panel_open(false);
    global.set_active_protection_label("Noise (bitchat)".into());
    global.set_active_signal_enabled(false);
    global.set_active_comparison_code("".into());
    global.set_binding_qr_visible(false);
    global.set_binding_scan_visible(false);
    global.set_private_invite_visible(false);
    global.set_private_remove_pending("".into());
    global.set_private_can_manage(false);
    global.set_private_removable_members(ModelRc::new(VecModel::from(Vec::<
        crate::PrivateChannelMemberData,
    >::new())));
    // The mesh set its own placeholder; leaving it would label a DM "everyone
    // in radio range".
    window
        .global::<crate::ChatPane>()
        .set_composer_placeholder("Type a message...".into());
    deselect_rows(window);
}

/// The open mesh conversation stopped existing (left, wiped): reset mesh state
/// and close the pane. `clear_active` alone left its title, messages and a
/// live composer on screen after Leave (desktop, ui151).
fn close_mesh_conversation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    if active().is_none() {
        return;
    }
    clear_active(window);
    crate::bin_ui::features::sidebar::close_open_conversation(window, ui_ctx);
    window
        .global::<crate::SidebarPane>()
        .set_active_tab("mesh".into());
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
    global.set_nickname(ui_ctx.mesh_nickname().into());
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_open_peer(move |peer| {
            if let Some(w) = weak.upgrade() {
                w.global::<MeshGlobal>().set_private_invite_visible(false);
                w.global::<MeshGlobal>()
                    .set_private_remove_pending("".into());
                open_peer_sheet(&w, &ui_ctx, peer.as_str());
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_open_peer_dm(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_open_dm(peer.as_str()) {
                Ok(id) => {
                    crate::bin_ui::features::back::pop(&w);
                    refresh_rows(&w, &ui_ctx);
                    conversation::open(&w, &ui_ctx, &id);
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(format!("Could not open mesh DM: {error}").into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_toggle_peer_mute(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            let muted = !ui_ctx.mesh_peer_muted(peer.as_str());
            match ui_ctx.mesh_set_peer_muted(peer.as_str(), muted) {
                Ok(()) => w.global::<MeshGlobal>().set_sheet_peer_muted(muted),
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(format!("Could not update mesh mute: {error}").into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_toggle_channel_mute(move |name| {
            let Some(w) = weak.upgrade() else { return };
            let muted = !ui_ctx.mesh_channel_muted(name.as_str());
            if ui_ctx.mesh_set_channel_muted(name.as_str(), muted).is_ok() {
                refresh_rows(&w, &ui_ctx);
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_toggle_peer_block(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            if ui_ctx
                .mesh_set_peer_blocked(peer.as_str(), !ui_ctx.mesh_peer_blocked(peer.as_str()))
                .is_ok()
            {
                refresh_rows(&w, &ui_ctx);
                w.global::<MeshGlobal>()
                    .set_sheet_peer_blocked(ui_ctx.mesh_peer_blocked(peer.as_str()));
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_join_channel(move |name| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_join_channel(name.as_str()) {
                Ok(_) => {
                    w.global::<MeshGlobal>()
                        .set_notice("Public channel joined.".into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(format!("Could not join channel: {error}").into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_create_private_channel(move |name| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_create_private_channel(name.as_str()) {
                Ok(scope) => {
                    refresh_rows(&w, &ui_ctx);
                    // Land in the new group with Group Info open, where Add
                    // Members is the next step.
                    if let Some(row) = core_api::rows(&ui_ctx)
                        .into_iter()
                        .find(|row| row.kind == "private-channel" && row.scope == scope)
                    {
                        crate::bin_ui::features::back::close(&w, "mesh-compose");
                        conversation::open(&w, &ui_ctx, &row.id);
                        crate::bin_ui::features::back::push(&w, "mesh-info");
                    }
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        global.on_open_setup(move || {
            if let Some(w) = weak.upgrade() {
                w.global::<crate::ModalHostState>()
                    .invoke_nav_push("mesh-setup".into());
            }
        });
    }
    {
        let weak = window.as_weak();
        global.on_open_sheet(move |sheet| {
            if let Some(w) = weak.upgrade() {
                w.global::<crate::ModalHostState>().invoke_nav_push(sheet);
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_join_private_channel(move |qr| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_join_private_channel(qr.as_str()) {
                Ok(()) => w.global::<MeshGlobal>().set_notice("Admission requested over Noise. Waiting for the creator to send the channel key.".into()),
                Err(error) => w.global::<MeshGlobal>().set_notice(error.to_string().into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_invite_private_channel(move |id, peer| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_private_channel_invite_png(id.as_str(), peer.as_str()) {
                Ok(png) => match image::load_from_memory(&png) {
                    Ok(decoded) => {
                        let rgba = decoded.to_rgba8();
                        let (width, height) = rgba.dimensions();
                        let mut pixels =
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
                        pixels.make_mut_bytes().copy_from_slice(&rgba.into_raw());
                        let mesh = w.global::<MeshGlobal>();
                        mesh.set_private_invite_image(slint::Image::from_rgba8(pixels));
                        mesh.set_private_invite_visible(true);
                        // The earlier "connecting securely" attempt succeeded now.
                        mesh.set_notice("".into());
                    }
                    Err(error) => w
                        .global::<MeshGlobal>()
                        .set_notice(error.to_string().into()),
                },
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_invite_private_channel_direct(move |id, peer| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_invite_private_channel_direct(id.as_str(), peer.as_str()) {
                Ok(()) => {
                    w.global::<MeshGlobal>()
                        .set_notice("Invitation queued — will deliver when connected.".into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_decide_private_invitation(move |id, join| {
            let Some(w) = weak.upgrade() else { return };
            let result = ui_ctx.mesh_decide_private_invitation(id.as_str(), join);
            let mesh = w.global::<MeshGlobal>();
            mesh.set_pending_private_id("".into());
            mesh.set_pending_private_name("".into());
            mesh.set_pending_private_peer("".into());
            match result {
                Ok(()) => {
                    refresh_rows(&w, &ui_ctx);
                    // A joined group opens, as a created one does.
                    let joined = join
                        .then(|| {
                            core_api::rows(&ui_ctx).into_iter().find(|row| {
                                row.kind == "private-channel" && row.scope == id.as_str()
                            })
                        })
                        .flatten();
                    if let Some(row) = joined {
                        crate::bin_ui::features::back::close(&w, "mesh-setup");
                        conversation::open(&w, &ui_ctx, &row.id);
                    } else {
                        mesh.set_notice(
                            if join {
                                "Private group joined."
                            } else {
                                "Invitation declined."
                            }
                            .into(),
                        );
                    }
                }
                Err(error) => mesh.set_notice(error.to_string().into()),
            }
            if let Some((next_id, next_name, next_peer)) =
                ui_ctx.mesh_pending_private_invitations().into_iter().next()
            {
                mesh.set_pending_private_id(next_id.into());
                mesh.set_pending_private_name(next_name.into());
                mesh.set_pending_private_peer(next_peer.into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_remove_private_member(move |id, peer| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_remove_private_channel_member(id.as_str(), peer.as_str()) {
                Ok(()) => {
                    w.global::<MeshGlobal>()
                        .set_notice("Member removed; the group key was rotated.".into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        global.on_open_private_scanner(move || {
            #[cfg(target_os = "android")]
            {
                let weak = weak.clone();
                crate::rt_handle().spawn(async move {
                    let result = crate::platform::android::qr_scanner::scan().await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        match result {
                            crate::platform::android::qr_scanner::ScanOutcome::Scanned(qr) => w
                                .global::<MeshGlobal>()
                                .invoke_join_private_channel(qr.trim().into()),
                            crate::platform::android::qr_scanner::ScanOutcome::CameraDenied => {
                                w.global::<MeshGlobal>().set_notice(
                                    "Camera access is required to scan the invite QR.".into(),
                                )
                            }
                            crate::platform::android::qr_scanner::ScanOutcome::Cancelled => {}
                        }
                    });
                });
            }
            #[cfg(not(target_os = "android"))]
            let _ = &weak;
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_leave_channel(move |name| {
            let Some(w) = weak.upgrade() else { return };
            if let Err(error) = ui_ctx.mesh_leave_channel(name.as_str()) {
                w.global::<MeshGlobal>().set_notice(format!("Could not leave channel: {error}").into());
            } else {
                if matches!(active(), Some(MeshConversation::Channel(ref open)) if open == name.as_str()) { close_mesh_conversation(&w, &ui_ctx); }
                refresh_rows(&w, &ui_ctx);
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_leave_private_channel(move |id| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.mesh_leave_private_channel(id.as_str()) {
                Ok(()) => {
                    if matches!(active(), Some(MeshConversation::PrivateChannel(ref open)) if open == id.as_str()) {
                        close_mesh_conversation(&w, &ui_ctx);
                    }
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w.global::<MeshGlobal>().set_notice(
                    format!("Could not leave private channel: {error}").into(),
                ),
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_set_nickname(move |name| {
            let Some(w) = weak.upgrade() else { return };
            match ui_ctx.set_mesh_nickname(name.as_str()) {
                Ok(()) => {
                    w.global::<MeshGlobal>()
                        .set_nickname(ui_ctx.mesh_nickname().into());
                    w.global::<MeshGlobal>()
                        .set_notice("Mesh name updated. Everyone nearby can see it.".into());
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(format!("Could not set mesh name: {error}").into()),
            }
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_share_identity(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            match core_api::share_identity(&ui_ctx, peer.as_str()) {
                Ok(()) => {
                    w.global::<MeshGlobal>().set_notice("Account proof sent over this peer's encrypted Noise link. They still need to verify it.".into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w.global::<MeshGlobal>().set_notice(error.to_string().into()),
            }
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_show_binding_qr(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            match core_api::identity_qr_png(&ui_ctx, peer.as_str()) {
                Ok(png) => match image::load_from_memory(&png) {
                    Ok(decoded) => {
                        let rgba = decoded.to_rgba8();
                        let (width, height) = rgba.dimensions();
                        let mut pixels =
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
                        pixels.make_mut_bytes().copy_from_slice(&rgba.into_raw());
                        let global = w.global::<MeshGlobal>();
                        global.set_binding_qr_image(slint::Image::from_rgba8(pixels));
                        global.set_binding_qr_visible(true);
                    }
                    Err(error) => w
                        .global::<MeshGlobal>()
                        .set_notice(format!("Could not show QR: {error}").into()),
                },
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_scan_binding_qr(move |peer, qr| {
            let Some(w) = weak.upgrade() else { return };
            let status = match core_api::verify_binding(&ui_ctx, peer.as_str(), qr.as_str()) {
                Ok(state) if state.label() == "Verified Chatter contact" => {
                    let mesh = w.global::<MeshGlobal>();
                    mesh.set_identity_panel_open(false);
                    mesh.set_binding_qr_visible(false);
                    mesh.set_binding_scan_visible(false);
                    String::new()
                }
                Ok(state) => state.label().to_string(),
                Err(error) => error.to_string(),
            };
            w.global::<MeshGlobal>().set_notice(status.into());
            refresh_rows(&w, &ui_ctx);
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_verify_known_contact(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            let status = match core_api::verify_known_contact(&ui_ctx, peer.as_str()) {
                Ok(state) if state.label() == "Verified Chatter contact" => {
                    let mesh = w.global::<MeshGlobal>();
                    mesh.set_identity_panel_open(false);
                    mesh.set_binding_qr_visible(false);
                    mesh.set_binding_scan_visible(false);
                    String::new()
                }
                Ok(state) => state.label().to_string(),
                Err(error) => error.to_string(),
            };
            w.global::<MeshGlobal>().set_notice(status.into());
            refresh_rows(&w, &ui_ctx);
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_toggle_signal_mesh(move |peer| {
            let Some(w) = weak.upgrade() else { return };
            let enabled = match core_api::signal_opted_in(&ui_ctx, peer.as_str()) {
                Ok(value) => !value,
                Err(error) => {
                    w.global::<MeshGlobal>()
                        .set_notice(error.to_string().into());
                    return;
                }
            };
            match core_api::set_signal_opt_in(&ui_ctx, peer.as_str(), enabled) {
                Ok(()) => {
                    w.global::<MeshGlobal>().set_notice(if enabled {
                        "Signal mesh enabled. Both people must enable it and exchange live keys before sending."
                    } else {
                        "Noise (bitchat) selected for this mesh conversation."
                    }.into());
                    refresh_rows(&w, &ui_ctx);
                }
                Err(error) => w
                    .global::<MeshGlobal>()
                    .set_notice(error.to_string().into()),
            }
        });
    }

    {
        let weak = window.as_weak();
        global.on_open_binding_scanner(move |peer| {
            #[cfg(target_os = "android")]
            {
                let weak = weak.clone();
                crate::rt_handle().spawn(async move {
                    let result = crate::platform::android::qr_scanner::scan().await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        match result {
                            crate::platform::android::qr_scanner::ScanOutcome::Scanned(qr) => w
                                .global::<MeshGlobal>()
                                .invoke_scan_binding_qr(peer, qr.trim().into()),
                            crate::platform::android::qr_scanner::ScanOutcome::CameraDenied => {
                                w.global::<MeshGlobal>().set_notice(
                                    "Camera access is required to scan this peer's QR.".into(),
                                )
                            }
                            crate::platform::android::qr_scanner::ScanOutcome::Cancelled => {}
                        }
                    });
                });
            }
            #[cfg(not(target_os = "android"))]
            let _ = (&weak, peer); // The inline field accepts a pasted QR on desktop.
        });
    }

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
                close_mesh_conversation(&w, &ui_ctx);
            }
            refresh_rows(&w, &ui_ctx);
        });
    }

    // Panic wipe, from the foot of the mesh section. The running service keeps
    // its identity in memory, so the radio is switched off afterwards too:
    // otherwise the device would go on announcing the key it just "destroyed".
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_panic_wipe(move || {
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            tokio::spawn(async move {
                // Stop ingress before clearing pins so an in-flight announce
                // cannot write them back while the wipe is in progress.
                ui_ctx.stop_mesh().await;
                let wiped = core_api::panic_wipe(&ui_ctx);
                run_ui!(weak, |w| {
                    close_mesh_conversation(&w, &ui_ctx);
                    refresh_rows(&w, &ui_ctx);
                    w.global::<MeshGlobal>()
                        .set_nickname(ui_ctx.mesh_nickname().into());
                    if let Err(error) = &wiped {
                        tracing::warn!(%error, "mesh: panic wipe incomplete");
                        w.global::<MeshGlobal>()
                            .set_notice(format!("Mesh wipe incomplete: {error}").into());
                    } else {
                        w.global::<MeshGlobal>()
                            .set_notice("Mesh data wiped from this device.".into());
                    }
                    w.global::<MeshGlobal>().invoke_set_mesh_on(false);
                });
            });
        });
    }

    // The mesh switch, from the header of any mesh conversation.
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        global.on_set_mesh_on(move |wanted| {
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            // Optimistic, then corrected: the switch has to move under the
            // finger, and starting a radio is not instant. `push_status` below
            // republishes what actually happened.
            if let Some(w) = weak.upgrade() {
                w.global::<MeshGlobal>().set_mesh_on(wanted);
            }
            tokio::spawn(async move {
                #[cfg(target_os = "android")]
                if wanted {
                    if !crate::platform::android::permissions::ensure_mesh().await {
                        run_ui!(weak, |w| {
                            w.global::<MeshGlobal>().set_mesh_on(false);
                            w.global::<MeshGlobal>().set_notice("Bluetooth permissions are required for mesh.".into());
                        });
                        return;
                    }
                    // A radio start refused for missing permissions ends its runtime.
                    if !core_api::status(&ui_ctx).radio_running {
                        ui_ctx.stop_mesh().await;
                    }
                    if !crate::platform::android::permissions::mesh_location_enabled() {
                        run_ui!(weak, |w| w.global::<MeshGlobal>().set_notice(
                            "Turn on Location in Android settings to discover nearby Bluetooth peers.".into()));
                    }
                }
                let outcome = ui_ctx.set_mesh_enabled(wanted).await;
                let ui_ctx = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    match outcome {
                        Ok(()) => {
                            // Deliberately does not clear the notice. Turning the
                            // mesh on returns as soon as the service accepts the
                            // request — the radio itself comes up later, inside the
                            // spawned runtime — so a failure to start arrives
                            // *after* this point and clearing here would erase the
                            // one explanation the user was going to get.
                            tracing::info!(wanted, "mesh: switched by the user");
                        }
                        Err(error) => {
                            // Put the switch back. A control that stays where it
                            // was put while the thing it controls did not move is
                            // worse than one that springs back and says why.
                            tracing::warn!(%error, wanted, "mesh: could not switch");
                            let global = w.global::<MeshGlobal>();
                            global.set_mesh_on(!wanted);
                            global.set_notice(
                                format!("Could not turn mesh {}: {error}",
                                    if wanted { "on" } else { "off" }).into(),
                            );
                        }
                    }
                    push_status(&w, &ui_ctx);
                    refresh_rows(&w, &ui_ctx);
                });
            });
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
    use slint::Model;

    let peer_rows = match active() {
        Some(MeshConversation::PrivateChannel(id)) => ui_ctx
            .mesh_private_channel_nearby_peers(&id)
            .unwrap_or_default(),
        _ => ui_ctx
            .mesh_nearby_peers()
            .into_iter()
            .map(|peer| (peer, false))
            .collect(),
    };
    let nearby: Vec<MeshPeerData> = peer_rows
        .into_iter()
        .map(|(peer, member)| MeshPeerData {
            id: peer.peer.as_str().into(),
            name: peer.nickname.into(),
            reachable: peer.reachable || peer.connected,
            member,
            binding_label: ui_ctx
                .mesh_binding_state(peer.peer.as_str())
                .map(|state| state.label())
                .unwrap_or("Mesh peer · self-asserted name")
                .into(),
        })
        .collect();
    let global = window.global::<MeshGlobal>();
    // Count the same core-selected peers shown in the setup sheet. Transport
    // status counts only connected peers, while this list also includes peers
    // still reachable through the mesh after their direct link drops.
    global.set_peer_count(i32::try_from(nearby.len()).unwrap_or(i32::MAX));
    let previous = global.get_nearby_peers();
    if previous.row_count() != nearby.len()
        || previous.iter().zip(&nearby).any(|(a, b)| {
            a.id != b.id
                || a.name != b.name
                || a.reachable != b.reachable
                || a.member != b.member
                || a.binding_label != b.binding_label
        })
    {
        global.set_nearby_peers(ModelRc::new(VecModel::from(nearby)));
    }
    let sheet_peer = global.get_sheet_peer_id();
    if !sheet_peer.is_empty() {
        refresh_invitable_channels(window, ui_ctx, sheet_peer.as_str());
        global.set_sheet_binding_label(
            ui_ctx
                .mesh_binding_state(sheet_peer.as_str())
                .map(|state| state.label())
                .unwrap_or("Mesh peer · self-asserted name")
                .into(),
        );
        global.set_sheet_comparison_code(
            ui_ctx
                .mesh_comparison_code(sheet_peer.as_str())
                .unwrap_or_default()
                .into(),
        );
    }
    let open = active();
    let rows = core_api::rows(ui_ctx);
    let nearby: Vec<slint::SharedString> = ui_ctx
        .mesh_nearby_channels()
        .into_iter()
        .map(Into::into)
        .collect();
    window
        .global::<MeshGlobal>()
        .set_nearby_channels(ModelRc::new(VecModel::from(nearby)));

    // The strip above the conversation shows live values (peer count,
    // reachability), so re-derive them from the same rows.
    if let Some(ref open) = open {
        if let Some(row) = rows.iter().find(|row| row.conversation == *open) {
            let global = window.global::<MeshGlobal>();
            global.set_active_title(row.title.clone().into());
            global.set_active_reachable(row.reachable);
            global.set_active_verified(row.verified);
            global.set_active_binding_label(row.binding_label.into());
            if row.kind == "private-channel" {
                global.set_private_rekey_pending(
                    ui_ctx
                        .mesh_private_channel_rekey_pending(row.scope.as_str())
                        .unwrap_or(false),
                );
                global.set_private_can_manage(
                    ui_ctx
                        .mesh_private_channel_is_creator(row.scope.as_str())
                        .unwrap_or(false),
                );
                window.global::<crate::ChatPane>().set_member_count(
                    ui_ctx
                        .mesh_private_channel_member_count(row.scope.as_str())
                        .unwrap_or(0) as i32,
                );
                global.set_active_signal_enabled(false);
                global.set_active_protection_label("Private epoch key · invited devices".into());
                let members: Vec<crate::PrivateChannelMemberData> = ui_ctx
                    .mesh_private_channel_removable_members(row.scope.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|member| crate::PrivateChannelMemberData {
                        peer_id: member.peer_id.into(),
                        display_name: member.display_name.into(),
                        status_label: member.status_label.into(),
                    })
                    .collect();
                global.set_private_removable_members(ModelRc::new(VecModel::from(members)));
            } else {
                global.set_private_rekey_pending(false);
                global.set_private_can_manage(false);
                global.set_private_removable_members(ModelRc::new(VecModel::from(Vec::<
                    crate::PrivateChannelMemberData,
                >::new(
                ))));
                global.set_active_signal_enabled(
                    core_api::signal_opted_in(ui_ctx, row.scope.as_str()).unwrap_or(false),
                );
                global.set_active_protection_label(
                    core_api::signal_protection_label(ui_ctx, row.scope.as_str())
                        .unwrap_or("Noise (bitchat)")
                        .into(),
                );
            }
            global.set_active_comparison_code(
                core_api::comparison_code(ui_ctx, row.scope.as_str())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }

    let items: Vec<MeshConversationData> = rows
        .into_iter()
        .map(|row| {
            let is_selected = open.as_ref().is_some_and(|open| *open == row.conversation);
            MeshConversationData {
                id: row.id.into(),
                kind: row.kind.into(),
                binding_label: row.binding_label.into(),
                title: row.title.into(),
                scope: row.scope.into(),
                timestamp: row.timestamp.into(),
                unread_count: row.unread_count,
                reachable: row.reachable,
                verified: row.verified,
                muted: row.muted,
                blocked: row.blocked,
                is_selected,
            }
        })
        .collect();

    let unread_count = items.iter().map(|row| row.unread_count).sum();
    let global = window.global::<MeshGlobal>();
    global.set_unread_count(unread_count);
    set_rows(&global, items);
}

/// Write the mesh rows and their per-section splits for the grouped home list.
fn set_rows(global: &MeshGlobal, rows: Vec<MeshConversationData>) {
    let section = |kinds: &[&str]| {
        let rows: Vec<_> = rows
            .iter()
            .filter(|row| kinds.contains(&row.kind.as_str()))
            .cloned()
            .collect();
        ModelRc::new(VecModel::from(rows))
    };
    global.set_group_conversations(section(&["private-channel"]));
    global.set_channel_conversations(section(&["room", "channel", "geohash"]));
    global.set_dm_conversations(section(&["dm"]));
    global.set_conversations(ModelRc::new(VecModel::from(rows)));
}

/// A mesh-derived sender id never enters server profile routing.
pub fn open_sender_sheet(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    sender: &chatter_core::domain::ids::UserId,
) {
    if let Some(peer) = ui_ctx.mesh_peer_for_sender(sender) {
        open_peer_sheet(window, ui_ctx, peer.peer.as_str());
    } else {
        let global = window.global::<MeshGlobal>();
        global.set_sheet_peer_id("".into());
        global.set_private_invitable_channels(ModelRc::new(VecModel::from(Vec::<
            PrivateChannelInviteData,
        >::new())));
        global.set_sheet_peer_name(
            ui_ctx
                .mesh_sender_display_name(sender)
                .unwrap_or_else(|| "Mesh peer".into())
                .into(),
        );
        global.set_sheet_binding_label("Self-asserted · peer no longer nearby".into());
        global.set_sheet_fingerprint("".into());
        global.set_sheet_comparison_code("".into());
        global.set_sheet_peer_blocked(false);
        global.set_sheet_peer_muted(false);
        global.set_binding_qr_visible(false);
        crate::bin_ui::features::back::push(window, "mesh-peer");
    }
}

fn open_peer_sheet(window: &MainWindow, ui_ctx: &Arc<ChatterClient>, peer_id: &str) {
    let Some(peer) = ui_ctx.mesh_peer_by_id(peer_id) else {
        return;
    };
    ui_ctx.mesh_open_session(peer_id);
    let global = window.global::<MeshGlobal>();
    global.set_sheet_peer_id(peer_id.into());
    refresh_invitable_channels(window, ui_ctx, peer_id);
    global.set_sheet_peer_name(peer.nickname.into());
    global.set_sheet_binding_label(
        ui_ctx
            .mesh_binding_state(peer_id)
            .map(|state| state.label())
            .unwrap_or("Mesh peer · self-asserted name")
            .into(),
    );
    global.set_sheet_fingerprint(peer.fingerprint.unwrap_or_default().into());
    global.set_sheet_comparison_code(
        ui_ctx
            .mesh_comparison_code(peer_id)
            .unwrap_or_default()
            .into(),
    );
    global.set_sheet_peer_blocked(ui_ctx.mesh_peer_blocked(peer_id));
    global.set_sheet_peer_muted(ui_ctx.mesh_peer_muted(peer_id));
    global.set_binding_qr_visible(false);
    global.set_binding_scan_visible(false);
    crate::bin_ui::features::back::push(window, "mesh-peer");
}

fn refresh_invitable_channels(window: &MainWindow, ui_ctx: &Arc<ChatterClient>, peer_id: &str) {
    let global = window.global::<MeshGlobal>();
    let active_scope = if global.get_active_kind().as_str() == "private-channel" {
        global.get_active_scope().to_string()
    } else {
        String::new()
    };
    let channels = ui_ctx
        .mesh_private_channels_invitable_for(peer_id, &active_scope)
        .unwrap_or_default();
    let items = channels
        .into_iter()
        .map(|(id, name)| PrivateChannelInviteData {
            id: id.into(),
            name: name.into(),
        })
        .collect::<Vec<_>>();
    global.set_private_invitable_channels(ModelRc::new(VecModel::from(items)));
}

/// Drop the selection highlight from every mesh row without asking core again.
fn deselect_rows(window: &MainWindow) {
    use slint::Model;

    let model = window.global::<MeshGlobal>().get_conversations();
    let rows: Vec<MeshConversationData> = model
        .iter()
        .map(|mut row| {
            row.is_selected = false;
            row
        })
        .collect();
    set_rows(&window.global::<MeshGlobal>(), rows);
}

/// Push transport state into the Slint global.
fn push_status(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    apply_status(window, ui_ctx, core_api::status(ui_ctx));
}

fn apply_status(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    status: chatter_core::services::mesh::MeshStatus,
) {
    let global = window.global::<MeshGlobal>();
    // Asked separately, because `MeshStatus` is only about transports — a relay
    // flapping rebuilds it, and a service flag inside it would read as the mesh
    // dying. See `MeshService::is_running`.
    global.set_running(core_api::service_running(ui_ctx));
    global.set_radio_running(status.radio_running);
    global.set_radio_down(core_api::radio_down_label(&status).into());
    global.set_relay_count(i32::try_from(status.connected_relays).unwrap_or(i32::MAX));

    // A running mesh is the whole of what makes the app usable without an
    // account, so this is the one place where that answer can change. The
    // sign-in screens read it to decide whether to offer a way past themselves.
    window.set_account_free_surfaces(ui_ctx.has_account_free_surfaces());
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
            let mesh_owner = active_session();
            let owner =
                super::history_owner::HistoryOwner::capture(ui_ctx, ui_ctx.active_conversation());
            let session = super::history_owner::SessionOwner::capture(ui_ctx);
            let is_open = mesh_owner
                .as_ref()
                .is_some_and(|(scope, _)| scope.as_ref() == Some(&message.conversation));

            // It arrived into a conversation that is on screen, so it has been
            // read. Done here, on the event-pump task, rather than from inside
            // the UI closure — this touches the message store.
            if is_open && !message.is_local {
                if let Err(error) = core_api::mark_read(ui_ctx, &message.conversation) {
                    tracing::debug!(%error, "mesh: could not mark the conversation read");
                }
            }

            let ui_ctx = ui_ctx.clone();
            crate::run_history_ui!(weak, session, |w| {
                if is_open
                    && owner.is_current()
                    && active_session() == mesh_owner
                    && active().as_ref() == Some(&message.conversation)
                {
                    conversation::append_incoming(&w, &ui_ctx, &message);
                }
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::Delivery {
            message_id, state, ..
        } => {
            let label = core_api::delivery_label(state);
            let domain_id =
                chatter_core::services::mesh::mapping::wire_message_id(&message_id).to_string();
            run_ui!(weak, |w| conversation::apply_delivery(
                &w, &domain_id, label
            ));
        }
        MeshEvent::Peers(_) => {
            // Refresh the count and rows from the same nearby list.
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                push_status(&w, &ui_ctx);
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::Status(status) => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                apply_status(&w, &ui_ctx, status);
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::GeohashJoined(_)
        | MeshEvent::GeohashLeft(_)
        | MeshEvent::ChannelObserved(_)
        | MeshEvent::PrivateChannelChanged => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| refresh_rows(&w, &ui_ctx));
        }
        MeshEvent::PrivateChannelClosed(id) => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                if matches!(active(), Some(MeshConversation::PrivateChannel(ref open)) if *open == id)
                {
                    close_mesh_conversation(&w, &ui_ctx);
                    w.global::<MeshGlobal>()
                        .set_notice("The creator removed this device from the group.".into());
                }
                refresh_rows(&w, &ui_ctx);
            });
        }
        MeshEvent::PrivateChannelInvitation {
            id,
            name,
            peer: _,
            nickname,
        } => {
            run_ui!(weak, |w| {
                let mesh = w.global::<MeshGlobal>();
                let first = mesh.get_pending_private_id().is_empty();
                if first || mesh.get_pending_private_id().as_str() == id {
                    mesh.set_pending_private_id(id.into());
                    mesh.set_pending_private_name(name.into());
                    mesh.set_pending_private_peer(nickname.into());
                    if first {
                        w.global::<crate::ModalHostState>()
                            .invoke_nav_push("mesh-setup".into());
                    }
                }
            });
        }
        MeshEvent::PrivateChannelInvitationExpired(id) => {
            let ui_ctx = ui_ctx.clone();
            run_ui!(weak, |w| {
                let mesh = w.global::<MeshGlobal>();
                if mesh.get_pending_private_id().as_str() == id {
                    mesh.set_pending_private_id("".into());
                    mesh.set_pending_private_name("".into());
                    mesh.set_pending_private_peer("".into());
                    if let Some((next_id, next_name, next_peer)) =
                        ui_ctx.mesh_pending_private_invitations().into_iter().next()
                    {
                        mesh.set_pending_private_id(next_id.into());
                        mesh.set_pending_private_name(next_name.into());
                        mesh.set_pending_private_peer(next_peer.into());
                    }
                }
            });
        }
        MeshEvent::IdentityKeyChanged { peer } => {
            run_ui!(weak, |w| w.global::<MeshGlobal>().set_notice(
                format!(
                    "Identity key change refused for {peer}. A new identity appears as a new peer."
                )
                .into()
            ));
        }
        MeshEvent::Degraded(reason) => {
            tracing::warn!(%reason, "Mesh transport unavailable");
            run_ui!(weak, |w| w
                .global::<MeshGlobal>()
                // Core's own reason. A generic "check Bluetooth" sent the user to
                // a switch that was already on while the mesh had refused to
                // start over its own stored state.
                .set_notice(format!("Mesh: {reason}").into()));
        }
    }
}
