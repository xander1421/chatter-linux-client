//! Anonymous conversation lifecycle event handlers (DM + Group).
//!
//! Lifecycle events (requests, invites, membership) remain here.
//! Message-related events are now unified in messaging.rs.

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{MessageId, UserId};
use chatter_core::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
use chrono::Utc;
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
            super::super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
            let requests = ui_ctx.list_anon_dm_message_requests();
            w.set_pending_message_requests(requests.len() as i32);
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
            super::super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
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
            super::super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
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
            let is_active = ui_ctx.active_conversation().matches_anon_group(&group_id);
            if is_active {
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
            super::super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
            tracing::info!(
                group_id = %group_id,
                updated_by = %updated_by,
                "Anon group updated"
            );
        }
    });
}

// =========================================================================
// Anonymous Group Membership Events (system-message synthesis)
//
// `Member{Joined,Left,Kicked}` events arrive as bare metadata; the UI shows
// them as "X joined", "X left", "X removed by Y" system bubbles. Build the
// synthetic Message here so mod.rs stays a pure dispatch table.
// =========================================================================

fn synthetic_system_message(
    group_id: AnonGroupId,
    actor_id: UserId,
    content: SystemMessage,
) -> Message {
    let now = Utc::now();
    Message {
        id: MessageId::new(),
        conversation_id: group_id.into(),
        conversation_type: ConversationType::AnonGroup,
        sender_id: Some(actor_id),
        device_id: None,
        content: MessageContent::System(content),
        encrypted_content: None,
        thread_id: None,
        reply_to: None,
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: true,
        sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
        created_at: now,
        updated_at: now,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
    }
}

pub(super) fn handle_group_member_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    user_id: UserId,
) {
    let message = synthetic_system_message(
        group_id,
        user_id,
        SystemMessage::UserJoined(user_id),
    );
    super::messaging::handle_message_received(window, ui_ctx, message);
    super::messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
}

pub(super) fn handle_group_member_left(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    user_id: UserId,
) {
    let message = synthetic_system_message(
        group_id,
        user_id,
        SystemMessage::UserLeft(user_id),
    );
    super::messaging::handle_message_received(window, ui_ctx, message);
    super::messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
}

pub(super) fn handle_group_member_kicked(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    user_id: UserId,
    kicked_by: UserId,
) {
    let message = synthetic_system_message(
        group_id,
        kicked_by, // sender = admin who did it
        SystemMessage::UserRemoved { by: kicked_by, removed: user_id },
    );
    super::messaging::handle_message_received(window, ui_ctx, message);
    super::messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
}
