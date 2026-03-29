//! Unified anonymous conversation event handlers (DM + Group).
//!
//! Single entry-point for message-receive that dispatches by conversation_type.
//! Lifecycle events (requests, invites, membership) remain as individual functions.

use std::sync::Arc;

use slint::{Model, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;
use chatter_core::domain::message::{ConversationType, Message};
use crate::{MainWindow, MessageItemData};

// =========================================================================
// Unified Message Received (from DomainEvent::MessageReceived)
// =========================================================================

/// Handle an incoming anonymous message (DM or Group) via the unified
/// `MessageReceived` event.  The `Message` struct carries `conversation_type`
/// so we dispatch here rather than maintaining two separate handler files.
pub(super) fn handle_message_received_unified(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    match message.conversation_type {
        ConversationType::AnonDm => handle_dm_message_unified(window, ui_ctx, message),
        ConversationType::AnonGroup => handle_group_message_unified(window, ui_ctx, message),
        _ => {
            tracing::warn!(
                conversation_type = ?message.conversation_type,
                "anon::handle_message_received_unified called with non-anon type"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// DM message (unified — takes Message directly)
// ---------------------------------------------------------------------------

fn handle_dm_message_unified(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let conversation_id = AnonDmId::from_uuid(*message.conversation_id.as_uuid());
    let sender_id = message.sender_id.unwrap_or_else(UserId::new);

    let sender_name = ui_ctx.users().get_display_name(&sender_id);

    let dm = ui_ctx.anon_dms().get_conversation(&conversation_id).ok().flatten();
    let current_user_id = ui_ctx.sessions().current_user_id();
    let is_own = current_user_id.map(|uid| sender_id == uid).unwrap_or(false);
    let current_user_id_str = current_user_id
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    let sender_avatar = if is_own {
        ui_ctx.anon_dms().get_my_avatar()
    } else {
        dm.as_ref().and_then(|d| d.peer_avatar.clone())
    };

    let resolved_attachments: Vec<chatter_core::domain::Attachment> = message.attachments
        .iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    let mut display_data = crate::bin_ui::converters::anon_dm_message_to_display_data(
        &message,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );

    // Resolve reply-to sender name if this is a reply/quote
    if display_data.is_reply {
        if let Some(ref author_id) = message.reply_to_author_id {
            display_data.reply_to_sender_name = ui_ctx.users().get_display_name(author_id);
        }
    }

    display_data.decode_attachment_thumbnails();

    let preview = build_preview(message.as_text(), resolved_attachments.len());
    let timestamp_str = super::super::utils::format_timestamp(message.sent_at);

    let conversation_id_str = conversation_id.to_string();
    let weak = window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        let selected_dm = w.get_selected_dm_id().to_string();
        let is_active = selected_dm
            .strip_prefix("anon_dm:")
            .map(|id_str| id_str == conversation_id_str)
            .unwrap_or(false);

        if is_active {
            let was_at_bottom = w.get_is_at_bottom();

            let new_msg = display_data.to_slint_with_user(&current_user_id_str);

            let current_model = w.get_messages();
            let ts_str = new_msg.timestamp_ns.to_string();
            let sender_str = new_msg.sender_id.to_string();
            let already_exists = (0..current_model.row_count()).any(|i| {
                current_model.row_data(i).map(|m| {
                    m.sender_id.as_str() == sender_str && m.timestamp_ns.as_str() == ts_str
                }).unwrap_or(false)
            });

            if !already_exists {
                if let Some(vec_model) = current_model.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                    vec_model.push(new_msg);
                } else {
                    let mut messages: Vec<crate::MessageItemData> = (0..current_model.row_count())
                        .filter_map(|i| current_model.row_data(i))
                        .collect();
                    messages.push(new_msg);
                    w.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
                }
            }

            if was_at_bottom {
                w.set_message_scroll_position(-99999.0);
                w.set_is_at_bottom(true);
            }
        }

        let conversation = chatter_core::state::ActiveConversation::AnonDm(conversation_id);
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, conversation, &preview, &timestamp_str, true,
        );
    });
}

// ---------------------------------------------------------------------------
// Group message (unified — takes Message directly)
// ---------------------------------------------------------------------------

fn handle_group_message_unified(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let group_id = AnonGroupId::from_uuid(*message.conversation_id.as_uuid());
    let sender_id = message.sender_id.unwrap_or_else(UserId::new);

    let sender_name = super::super::anon_group_handlers::get_anon_member_name(ui_ctx, &group_id, &sender_id);
    let group_id_str = group_id.to_string();

    let sender_avatar = ui_ctx.anon_groups()
        .get_member(&group_id, &sender_id)
        .ok()
        .flatten()
        .and_then(|m| m.avatar.clone());

    let resolved_attachments: Vec<chatter_core::domain::Attachment> = message.attachments
        .iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
        &message,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );

    // Resolve reply-to sender name if this is a reply/quote
    if display_data.is_reply {
        if let Some(ref author_id) = message.reply_to_author_id {
            display_data.reply_to_sender_name = ui_ctx.users().get_display_name(author_id);
        }
    }

    display_data.decode_attachment_thumbnails();

    let current_user_id = ui_ctx.sessions().current_user_id()
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    let preview = build_preview(message.as_text(), resolved_attachments.len());
    let timestamp_str = super::super::utils::format_timestamp(message.sent_at);

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group == group_id_str {
            let messages = window.get_messages();
            if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                let msg_item = display_data.to_slint_with_user(&current_user_id);
                model.push(msg_item);
            }

            if window.get_is_at_bottom() {
                window.set_message_scroll_position(-99999.0);
                window.set_is_at_bottom(true);
            }
        }

        let conversation = chatter_core::state::ActiveConversation::AnonGroup(group_id);
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &window, conversation, &preview, &timestamp_str, true,
        );
    });
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn build_preview(text: Option<&str>, attachment_count: usize) -> String {
    let msg_text = text.unwrap_or("");
    if msg_text.is_empty() && attachment_count > 0 {
        format!("\u{1F4CE} {} attachment{}", attachment_count, if attachment_count == 1 { "" } else { "s" })
    } else if msg_text.is_empty() {
        "[Message]".to_string()
    } else if msg_text.chars().count() > 50 {
        let truncated: String = msg_text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        msg_text.to_string()
    }
}

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

pub(super) fn handle_group_member_joined(
    group_id: AnonGroupId,
    user_id: UserId,
    display_name: Option<String>,
) {
    tracing::info!(
        group_id = %group_id, user_id = %user_id, display_name = ?display_name,
        "Member joined anon group"
    );
}

pub(super) fn handle_group_member_left(group_id: AnonGroupId, user_id: UserId) {
    tracing::info!(group_id = %group_id, user_id = %user_id, "Member left anon group");
}

pub(super) fn handle_group_member_kicked(group_id: AnonGroupId, user_id: UserId, kicked_by: UserId) {
    tracing::info!(
        group_id = %group_id, user_id = %user_id, kicked_by = %kicked_by,
        "Member kicked from anon group"
    );
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
// Delivery/Read Receipt Status Updates
// =========================================================================

/// Handle delivery/read receipt UI update for anonymous conversations.
///
/// Updates checkmarks (✓ → ✓✓) on the active message list for own messages
/// with matching Signal timestamps. Mirrors the logic in
/// `messaging::handle_delivery_status_update` but is dispatched separately
/// for anon conversations to keep routing explicit and allow independent
/// evolution of anon receipt handling.
pub(super) fn handle_anon_delivery_status_update(
    window: &Weak<MainWindow>,
    message_timestamps: Vec<u64>,
    new_status: &'static str,
) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        let messages = w.get_messages();
        let timestamp_strs: Vec<String> =
            message_timestamps.iter().map(|ts| ts.to_string()).collect();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.is_own
                    && timestamp_strs
                        .iter()
                        .any(|ts| msg.timestamp_ns.as_str() == ts.as_str())
                {
                    let mut updated = msg.clone();
                    updated.delivery_status = new_status.into();
                    if let Some(model) =
                        messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated);
                    }
                }
            }
        }
    });
}
