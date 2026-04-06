//! Anonymous conversation lifecycle event handlers (DM + Group).
//!
//! Lifecycle events (requests, invites, membership) remain here.
//! Message-related events are now unified in messaging.rs.

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

// =========================================================================
// Anonymous DM Lifecycle Events
// =========================================================================

pub(super) fn handle_dm_request_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            if let Ok(requests) = ui_ctx.anon_dms().list_message_requests() {
                w.set_pending_message_requests(requests.len() as i32);
            }
        }
    });
}

pub(super) fn handle_dm_request_accepted(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    peer_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            tracing::info!(
                conversation_id = %conversation_id,
                peer_id = %peer_id,
                "Anon DM request accepted"
            );
        }
    });
}

pub(super) fn handle_dm_init_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
        }
    });
}

// =========================================================================
// Anonymous Group Lifecycle Events
// =========================================================================

pub(super) fn handle_group_invite_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    _group_id: AnonGroupId,
    _group_name: String,
    _inviter_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
        }
    });
}

pub(super) fn handle_group_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
            tracing::info!(group_id = %group_id, "Joined anon group - refreshed sidebar");
        }
    });
}

pub(super) fn handle_group_bootstrap_complete(group_id: AnonGroupId) {
    tracing::debug!(group_id = %group_id, "Anon group bootstrap complete");
}

pub(super) fn handle_group_member_list_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    member_count: usize,
) {
    tracing::debug!(
        group_id = %group_id,
        member_count,
        "Received anon group member list"
    );

    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let group_id_str = group_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let selected_group = w.get_selected_private_group_id().to_string();
            if selected_group == group_id_str {
                tracing::debug!("Refreshing message list for group {} after member list update", group_id_str);
                crate::bin_ui::features::sidebar::private_group::load_private_group_messages(
                    &w,
                    &ui_ctx,
                    &group_id_str,
                );
            }
        }
    });
}

pub(super) fn handle_group_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    updated_by: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
            tracing::info!(
                group_id = %group_id,
                updated_by = %updated_by,
                "Anon group updated"
            );
        }
    });
}
