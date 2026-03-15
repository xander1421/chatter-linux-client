//! Anonymous group event handlers.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::UserId;
use chatter_linux::domain::anon_group::AnonGroupId;
use crate::MainWindow;

/// Handle anonymous group invite received.
pub(super) fn handle_invite_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    _group_id: AnonGroupId,
    _group_name: String,
    _inviter_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anonymous groups to show the invite
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
        }
    });
}

/// Handle anonymous group joined.
pub(super) fn handle_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh sidebar to show new group
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
            tracing::info!(group_id = %group_id, "Joined anon group - refreshed sidebar");
        }
    });
}

/// Handle anonymous group member joined.
pub(super) fn handle_member_joined(
    group_id: AnonGroupId,
    user_id: UserId,
    display_name: Option<String>,
) {
    tracing::info!(
        group_id = %group_id,
        user_id = %user_id,
        display_name = ?display_name,
        "Member joined anon group"
    );
    // Could show a system message or notification
}

/// Handle anonymous group member left.
pub(super) fn handle_member_left(group_id: AnonGroupId, user_id: UserId) {
    tracing::info!(group_id = %group_id, user_id = %user_id, "Member left anon group");
}

/// Handle anonymous group member kicked.
pub(super) fn handle_member_kicked(group_id: AnonGroupId, user_id: UserId, kicked_by: UserId) {
    tracing::info!(
        group_id = %group_id,
        user_id = %user_id,
        kicked_by = %kicked_by,
        "Member kicked from anon group"
    );
}

/// Handle anonymous group message received.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    sender_id: UserId,
    message_id: chatter_linux::domain::anon_group::AnonGroupMessageId,
) {
    // Resolve sender display name from group member list
    let sender_name = super::super::anon_group_handlers::get_anon_member_name(ui_ctx, &group_id, &sender_id);
    let group_id_str = group_id.to_string();
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();

    // Fetch the stored message and push it to the UI
    let msg = match ui_ctx.anon_group_service.get_message(&message_id) {
        Ok(Some(m)) => m,
        _ => {
            tracing::warn!(message_id = %message_id, "AnonGroupMessageReceived: message not found in store");
            return;
        }
    };

    // Get sender avatar from member profile
    let sender_avatar = ui_ctx.anon_group_service
        .get_member(&group_id, &sender_id)
        .ok()
        .flatten()
        .and_then(|m| m.avatar.clone());

    // Resolve attachments
    let attachment_ids = msg.attachment_ids();
    let resolved_attachments: Vec<chatter_linux::domain::Attachment> = attachment_ids
        .iter()
        .filter_map(|att_id| ui_ctx.attachment_service.get(att_id).ok().flatten())
        .collect();

    let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
        &msg,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );
    display_data.decode_attachment_thumbnails();

    let current_user_id = ui_ctx.session_service.current_user_id()
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };
        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        let messages = window.get_messages();
        if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
            let msg_item = display_data.to_slint_with_user(&current_user_id);
            model.push(msg_item);
        }
    });
}

/// Handle anonymous group bootstrap complete.
pub(super) fn handle_bootstrap_complete(group_id: AnonGroupId) {
    tracing::debug!(group_id = %group_id, "Anon group bootstrap complete");
}

/// Handle anonymous group member list received.
pub(super) fn handle_member_list_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    members: Vec<chatter_linux::proto::content_v1::AnonGroupMemberInfo>,
) {
    tracing::debug!(
        group_id = %group_id,
        member_count = members.len(),
        "Received anon group member list"
    );

    // Refresh the message list if we're currently viewing this group
    // This ensures member names are updated in existing messages
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let group_id_str = group_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let selected_group = w.get_selected_private_group_id().to_string();
            if selected_group == group_id_str {
                tracing::debug!("Refreshing message list for group {} after member list update", group_id_str);
                // Reload messages to show updated member names
                crate::bin_ui::features::sidebar::private_group::load_private_group_messages(
                    &w,
                    &ui_ctx,
                    &group_id_str,
                );
            }
        }
    });
}

/// Handle anonymous group updated.
pub(super) fn handle_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    updated_by: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh group info
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
            tracing::info!(
                group_id = %group_id,
                updated_by = %updated_by,
                "Anon group updated"
            );
        }
    });
}
