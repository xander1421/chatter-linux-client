//! Anonymous group-specific event handlers.
//!
//! Handles anonymous group member events: joined, left, kicked.
//! Typing, reactions, pins, edits, and deletes have been consolidated into unified_actions.rs.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;

use crate::MainWindow;
use crate::MessageItemData;

/// Helper to get a member's display name, trying the group profile first, then falling back to user profile.
pub(crate) fn get_anon_member_name(ui_ctx: &Arc<UIContext>, group_id: &AnonGroupId, user_id: &UserId) -> String {
    if let Ok(Some(name)) = ui_ctx.anon_group_service.get_member_display_name(group_id, user_id) {
        if !name.is_empty() {
            return name;
        }
    }
    ui_ctx.user_service.get_display_name(user_id)
}

// =============================================================================
// Member Event Handlers
// =============================================================================

/// Handle anon group member joined.
///
/// Adds a system message to the chat indicating someone joined.
pub(crate) fn handle_anon_group_member_joined(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    user_id: UserId,
) {
    let member_name = get_anon_member_name(ui_ctx, &group_id, &user_id);
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::info!(
        group_id = %group_id_str,
        user_id = %user_id,
        member_name = %member_name,
        "ANON_GROUP_MEMBER: Member joined"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        // Create a system message for the join event
        let messages = window.get_messages();
        if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
            let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let system_msg = MessageItemData {
                message_id: format!("system-join-{}", timestamp_ns).into(),
                sender_id: "".into(),
                sender_name: "".into(),
                sender_initials: "".into(),
                sender_avatar_url: "".into(),
                sender_avatar_source: slint::Image::default(),
                has_avatar: false,
                content: format!("{} joined the group", member_name).into(),
                timestamp: chrono::Utc::now().format("%H:%M").to_string().into(),
                timestamp_ns: timestamp_ns.to_string().into(),
                is_own: false,
                is_edited: false,
                is_deleted: false,
                is_system: true,
                is_ai: false,
                has_attachments: false,
                attachment_count: 0,
                attachments: slint::ModelRc::default(),
                has_reactions: false,
                reaction_summary: "".into(),
                reactions: ModelRc::default(),
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
                is_reply: false,
                reply_to_sender_name: "".into(),
                reply_to_preview: "".into(),
                show_sender: false,
                delivery_status: "".into(),
                is_pinned: false,
                is_call: false,
                call_type: "".into(),
                call_duration: "".into(),
            };

            model.push(system_msg);
            tracing::debug!(
                member_name = %member_name,
                "ANON_GROUP_MEMBER: Added join system message"
            );
        }
    });
}

/// Handle anon group member left.
///
/// Adds a system message to the chat indicating someone left.
pub(crate) fn handle_anon_group_member_left(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    user_id: UserId,
) {
    let member_name = get_anon_member_name(ui_ctx, &group_id, &user_id);
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::info!(
        group_id = %group_id_str,
        user_id = %user_id,
        member_name = %member_name,
        "ANON_GROUP_MEMBER: Member left"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        // Create a system message for the leave event
        let messages = window.get_messages();
        if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
            let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let system_msg = MessageItemData {
                message_id: format!("system-leave-{}", timestamp_ns).into(),
                sender_id: "".into(),
                sender_name: "".into(),
                sender_initials: "".into(),
                sender_avatar_url: "".into(),
                sender_avatar_source: slint::Image::default(),
                has_avatar: false,
                content: format!("{} left the group", member_name).into(),
                timestamp: chrono::Utc::now().format("%H:%M").to_string().into(),
                timestamp_ns: timestamp_ns.to_string().into(),
                is_own: false,
                is_edited: false,
                is_deleted: false,
                is_system: true,
                is_ai: false,
                has_attachments: false,
                attachment_count: 0,
                attachments: slint::ModelRc::default(),
                has_reactions: false,
                reaction_summary: "".into(),
                reactions: ModelRc::default(),
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
                is_reply: false,
                reply_to_sender_name: "".into(),
                reply_to_preview: "".into(),
                show_sender: false,
                delivery_status: "".into(),
                is_pinned: false,
                is_call: false,
                call_type: "".into(),
                call_duration: "".into(),
            };

            model.push(system_msg);
            tracing::debug!(
                member_name = %member_name,
                "ANON_GROUP_MEMBER: Added leave system message"
            );
        }
    });
}

/// Handle anon group member kicked.
///
/// Adds a system message to the chat indicating someone was kicked.
pub(crate) fn handle_anon_group_member_kicked(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    user_id: UserId,
    kicked_by: UserId,
) {
    let member_name = get_anon_member_name(ui_ctx, &group_id, &user_id);
    let kicker_name = get_anon_member_name(ui_ctx, &group_id, &kicked_by);
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::info!(
        group_id = %group_id_str,
        user_id = %user_id,
        member_name = %member_name,
        kicked_by = %kicked_by,
        kicker_name = %kicker_name,
        "ANON_GROUP_MEMBER: Member kicked"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        // Create a system message for the kick event
        let messages = window.get_messages();
        if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
            let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let system_msg = MessageItemData {
                message_id: format!("system-kick-{}", timestamp_ns).into(),
                sender_id: "".into(),
                sender_name: "".into(),
                sender_initials: "".into(),
                sender_avatar_url: "".into(),
                sender_avatar_source: slint::Image::default(),
                has_avatar: false,
                content: format!("{} was removed by {}", member_name, kicker_name).into(),
                timestamp: chrono::Utc::now().format("%H:%M").to_string().into(),
                timestamp_ns: timestamp_ns.to_string().into(),
                is_own: false,
                is_edited: false,
                is_deleted: false,
                is_system: true,
                is_ai: false,
                has_attachments: false,
                attachment_count: 0,
                attachments: slint::ModelRc::default(),
                has_reactions: false,
                reaction_summary: "".into(),
                reactions: ModelRc::default(),
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
                is_reply: false,
                reply_to_sender_name: "".into(),
                reply_to_preview: "".into(),
                show_sender: false,
                delivery_status: "".into(),
                is_pinned: false,
                is_call: false,
                call_type: "".into(),
                call_duration: "".into(),
            };

            model.push(system_msg);
            tracing::debug!(
                member_name = %member_name,
                kicker_name = %kicker_name,
                "ANON_GROUP_MEMBER: Added kick system message"
            );
        }
    });
}
