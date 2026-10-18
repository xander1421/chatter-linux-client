//! Anonymous conversation lifecycle event handlers (DM + Group).
//!
//! Lifecycle events (requests, invites, membership) remain here.
//! Message-related events are now unified in messaging.rs.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::Weak;

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;

// =========================================================================
// Anonymous DM Lifecycle Events
// =========================================================================

pub(super) fn handle_dm_request_received(window: &Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            let requests = ui_ctx.list_anon_dm_message_requests();
            w.global::<crate::ModalHostState>()
                .set_pending_message_requests(requests.len() as i32);
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
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            tracing::info!(
                conversation_id = %conversation_id,
                peer_id = %peer_id,
                "Anon DM request accepted"
            );
        }
    });
}

pub(super) fn refresh_chat_list(window: &Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
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
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
        }
    });
}

pub(super) fn handle_group_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
) {
    let weak = window.clone();
    let ui_ctx_for_ui = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx_for_ui);
            tracing::info!(group_id = %group_id, "Joined anon group - refreshed sidebar");
        }
    });
    refresh_active_group_counts(window, ui_ctx, group_id);
}

/// The private group header uses its local roster, not channel memberships.
fn refresh_active_group_counts(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
) {
    crate::bin_ui::features::modals::refresh_members_if_open(
        window.clone(),
        ui_ctx.clone(),
        group_id.into(),
    );
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if !ui_ctx.active_conversation().matches_anon_group(&group_id) {
                return;
            }
            let (member_count, invited_count) = ui_ctx.anon_group_member_counts(&group_id);
            w.global::<crate::ChatPane>().set_member_count(member_count);
            w.global::<crate::ChatPane>()
                .set_invited_count(invited_count);
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
    // The whole roster was replaced: refresh the open header and settings.
    refresh_active_group_counts(window, ui_ctx, group_id);

    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let group_id_str = group_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let is_active = ui_ctx.active_conversation().matches_anon_group(&group_id);
            if is_active {
                // Older group sends used the DM bootstrap placeholder. A
                // roster refresh restores the group composer as well.
                if w.global::<crate::ChatPane>()
                    .get_composer_placeholder()
                    .as_str()
                    == crate::bin_ui::features::chat::send_message::BOOTSTRAP_PLACEHOLDER
                {
                    w.global::<crate::ChatPane>()
                        .set_composer_placeholder("Type a message...".into());
                }
                tracing::debug!(
                    "Refreshing message list for group {} after member list update",
                    group_id_str
                );
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
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            tracing::info!(
                group_id = %group_id,
                updated_by = %updated_by,
                "Anon group updated"
            );
        }
    });
}

// =========================================================================
// Anonymous Group Membership Events
//
// Core persists membership notices with their event timestamps before these
// handlers run. Reload the open chat so live and reopened history agree.
// =========================================================================

pub(super) fn handle_group_member_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    _user_id: UserId,
) {
    refresh_group_membership_notice(window, ui_ctx, group_id);
}

pub(super) fn handle_group_member_left(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    _user_id: UserId,
) {
    refresh_group_membership_notice(window, ui_ctx, group_id);
}

pub(super) fn handle_group_member_kicked(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    _user_id: UserId,
    _kicked_by: UserId,
) {
    // Core retained a removed user's history, including the verified notice.
    refresh_group_membership_notice(window, ui_ctx, group_id);
}

fn refresh_group_membership_notice(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
) {
    refresh_active_group_counts(window, ui_ctx, group_id);
    let (weak, ui_ctx) = (window.clone(), ui_ctx.clone());
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            if ui_ctx.active_conversation().matches_anon_group(&group_id) {
                crate::bin_ui::features::sidebar::private_group::load_private_group_messages(
                    &w,
                    &ui_ctx,
                    &group_id.to_string(),
                );
            }
        }
    });
}
