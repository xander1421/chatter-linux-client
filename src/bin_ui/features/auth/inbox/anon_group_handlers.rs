//! Anonymous group-specific event handlers.
//!
//! Handles anonymous group message events: typing, reactions, pins, edits, deletes,
//! member joined, member left.

use std::sync::Arc;

use slint::{Model, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;

use crate::MainWindow;

use super::typing::{clear_typing_for_peer, set_typing_with_timer};
use super::utils::update_reaction_summary;
use crate::MessageItemData;

/// Helper to get a member's display name, trying the group profile first, then falling back to user profile.
fn get_anon_member_name(ui_ctx: &Arc<UIContext>, group_id: &AnonGroupId, user_id: &UserId) -> String {
    if let Ok(Some(name)) = ui_ctx.anon_group_service.get_member_display_name(group_id, user_id) {
        if !name.is_empty() {
            return name;
        }
    }
    ui_ctx.user_service.get_display_name(user_id)
}

// =============================================================================
// Typing Indicator Handler
// =============================================================================

/// Handle anon group typing indicator.
///
/// Shows "{sender_name} is typing..." in the UI when the sender is typing
/// in the currently selected private group.
pub(crate) fn handle_anon_group_typing(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    group_id: AnonGroupId,
    sender_id: UserId,
    action: i32,
) {
    let sender_name = get_anon_member_name(ui_ctx, &group_id, &sender_id);
    let group_id_str = group_id.to_string();
    let sender_id_str = sender_id.to_string();
    let weak = weak_window.clone();

    tracing::debug!(
        group_id = %group_id_str,
        sender_id = %sender_id_str,
        sender_name = %sender_name,
        action,
        action_name = if action == 1 { "STARTED" } else { "STOPPED" },
        "ANON_GROUP_TYPING: Received typing indicator"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            tracing::debug!(
                selected_group = %selected_group,
                group_id = %group_id_str,
                "ANON_GROUP_TYPING: Not active group - skipping UI update"
            );
            return;
        }

        if action == 1 {
            // Use sender_id as the peer key for typing timer
            set_typing_with_timer(&window, &sender_id_str);
            tracing::debug!(
                sender_id = %sender_id_str,
                "ANON_GROUP_TYPING: Set typing indicator with timer"
            );
        } else {
            clear_typing_for_peer(&window, &sender_id_str);
            tracing::debug!(
                sender_id = %sender_id_str,
                "ANON_GROUP_TYPING: Cleared typing indicator"
            );
        }
    });
}

// =============================================================================
// Reaction Handler
// =============================================================================

/// Handle anon group reaction.
///
/// Updates the reaction summary on the target message in the UI.
pub(crate) fn handle_anon_group_reaction(
    weak_window: &slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::debug!(
        group_id = %group_id_str,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "ANON_GROUP_REACTION: Received reaction"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    let new_summary =
                        update_reaction_summary(&msg.reaction_summary.to_string(), &emoji, remove);
                    updated_msg.reaction_summary = new_summary.into();
                    updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                        tracing::debug!(
                            target_ts = target_sent_timestamp,
                            emoji = %emoji,
                            remove,
                            "ANON_GROUP_REACTION: Updated message reaction"
                        );
                    }
                    return;
                }
            }
        }

        tracing::debug!(
            target_ts = target_sent_timestamp,
            "ANON_GROUP_REACTION: Target message not found in current view"
        );
    });
}

// =============================================================================
// Pin Handler
// =============================================================================

/// Handle anon group pin/unpin.
///
/// Updates the pinned message state in the UI header.
pub(crate) fn handle_anon_group_pin(
    weak_window: &slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::debug!(
        group_id = %group_id_str,
        target = target_sent_timestamp,
        unpin,
        "ANON_GROUP_PIN: Received pin/unpin"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        // Find the message to get its preview text
        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    preview = msg.content.to_string();
                    if preview.len() > 100 {
                        preview = format!("{}...", &preview[..97]);
                    }

                    // Update message's is_pinned state
                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = !unpin;
                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    break;
                }
            }
        }

        // Update pinned message header state
        let current_count = window.get_pinned_total_count();
        if unpin {
            let new_count = (current_count - 1).max(0);
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(new_count > 0);
            if new_count == 0 {
                window.set_pinned_message_preview("".into());
            }
            tracing::debug!(
                new_count,
                "ANON_GROUP_PIN: Decremented pinned count"
            );
        } else {
            let new_count = current_count + 1;
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(true);
            if !preview.is_empty() {
                window.set_pinned_message_preview(preview.into());
                window.set_pinned_current_index(new_count);
            }
            tracing::debug!(
                new_count,
                "ANON_GROUP_PIN: Incremented pinned count"
            );
        }
    });
}

// =============================================================================
// Edit Handler
// =============================================================================

/// Handle anon group message edit.
///
/// Updates the message content in the UI and marks it as edited.
pub(crate) fn handle_anon_group_edit(
    weak_window: &slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::debug!(
        group_id = %group_id_str,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "ANON_GROUP_EDIT: Received message edit"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_body.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                        tracing::debug!(
                            target_ts = target_sent_timestamp,
                            "ANON_GROUP_EDIT: Updated message content"
                        );
                    }
                    return;
                }
            }
        }

        tracing::debug!(
            target_ts = target_sent_timestamp,
            "ANON_GROUP_EDIT: Target message not found in current view"
        );
    });
}

// =============================================================================
// Delete Handler
// =============================================================================

/// Handle anon group message delete (delete-for-everyone).
///
/// Marks the message as deleted in the UI with placeholder text.
pub(crate) fn handle_anon_group_delete(
    weak_window: &slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    target_sent_timestamp: u64,
) {
    let group_id_str = group_id.to_string();
    let weak = weak_window.clone();

    tracing::debug!(
        group_id = %group_id_str,
        target = target_sent_timestamp,
        "ANON_GROUP_DELETE: Received delete-for-everyone"
    );

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_group = window.get_selected_private_group_id().to_string();
        if selected_group != group_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "This message was deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                        tracing::debug!(
                            target_ts = target_sent_timestamp,
                            "ANON_GROUP_DELETE: Marked message as deleted"
                        );
                    }
                    return;
                }
            }
        }

        tracing::debug!(
            target_ts = target_sent_timestamp,
            "ANON_GROUP_DELETE: Target message not found in current view"
        );
    });
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
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
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
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
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
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
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
