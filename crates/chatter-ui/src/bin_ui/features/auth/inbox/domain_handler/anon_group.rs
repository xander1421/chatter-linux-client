//! Anonymous group event handlers.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::domain::anon_group::AnonGroupId;
use crate::MainWindow;

/// Handle anonymous group invite received.
pub(super) fn handle_invite_received(
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
            // Refresh anonymous groups to show the invite
            super::super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
        }
    });
}

/// Handle anonymous group joined.
pub(super) fn handle_joined(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
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

/// Handle anonymous group member joined — log only.
/// System message creation should be done in core (like ChannelMemberJoined).
pub(super) fn handle_member_joined(
    group_id: AnonGroupId,
    user_id: UserId,
    display_name: Option<String>,
) {
    tracing::info!(
        group_id = %group_id, user_id = %user_id, display_name = ?display_name,
        "Member joined anon group"
    );
}

/// Handle anonymous group member left.
pub(super) fn handle_member_left(group_id: AnonGroupId, user_id: UserId) {
    tracing::info!(group_id = %group_id, user_id = %user_id, "Member left anon group");
}

/// Handle anonymous group member kicked.
pub(super) fn handle_member_kicked(group_id: AnonGroupId, user_id: UserId, kicked_by: UserId) {
    tracing::info!(
        group_id = %group_id, user_id = %user_id, kicked_by = %kicked_by,
        "Member kicked from anon group"
    );
}

/// Handle anonymous group message received.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    sender_id: UserId,
    message_id: chatter_core::domain::anon_group::AnonGroupMessageId,
) {
    // Resolve sender display name from group member list
    let sender_name = super::super::anon_group_handlers::get_anon_member_name(ui_ctx, &group_id, &sender_id);
    let group_id_str = group_id.to_string();
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();

    // Fetch the stored message and push it to the UI
    let msg = match ui_ctx.anon_groups().get_message(&message_id) {
        Ok(Some(m)) => m,
        _ => {
            tracing::warn!(message_id = %message_id, "AnonGroupMessageReceived: message not found in store");
            return;
        }
    };

    // Get sender avatar from member profile
    let sender_avatar = ui_ctx.anon_groups()
        .get_member(&group_id, &sender_id)
        .ok()
        .flatten()
        .and_then(|m| m.avatar.clone());

    // Resolve attachments
    let attachment_ids = msg.attachment_ids();
    let resolved_attachments: Vec<chatter_core::domain::Attachment> = attachment_ids
        .iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
        &msg,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );
    display_data.decode_attachment_thumbnails();

    let current_user_id = ui_ctx.sessions().current_user_id()
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    // Build sidebar preview from the message
    let msg_text = msg.as_text().unwrap_or_default();
    let preview = if msg_text.is_empty() {
        "[Attachment]".to_string()
    } else if msg_text.chars().count() > 50 {
        let truncated: String = msg_text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        msg_text.to_string()
    };
    let timestamp_str = super::super::utils::format_timestamp(msg.sent_at);

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        // Push to message model if this group is active
        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group == group_id_str {
            let messages = window.get_messages();
            if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                let msg_item = display_data.to_slint_with_user(&current_user_id);
                model.push(msg_item);
            }

            // Auto-scroll if at bottom
            if window.get_is_at_bottom() {
                window.set_message_scroll_position(-99999.0);
                window.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview + unread
        let conversation = chatter_core::state::ActiveConversation::AnonGroup(group_id);
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &window, conversation, &preview, &timestamp_str, true,
        );
    });
}

/// Handle anonymous group bootstrap complete.
pub(super) fn handle_bootstrap_complete(group_id: AnonGroupId) {
    tracing::debug!(group_id = %group_id, "Anon group bootstrap complete");
}

/// Handle anonymous group member list received.
pub(super) fn handle_member_list_received(
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
    ui_ctx: &Arc<ChatterClient>,
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
