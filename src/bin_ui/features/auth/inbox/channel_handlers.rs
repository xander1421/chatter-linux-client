//! Channel-specific event handlers.
//!
//! Handles channel message events: received, typing, reactions, pins, edits, deletes.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::domain::attachment::Attachment;
use chatter_linux::storage::{AttachmentRepository, MessageRepository, UserRepository};

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::{MainWindow, MessageItemData};

use super::utils::update_reaction_summary;

// =============================================================================
// Channel Event Handlers
// =============================================================================

pub(crate) fn handle_channel_message_received(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    attachment_repo: &Arc<AttachmentRepository>,
    message_repo: &Arc<MessageRepository>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    text: String,
) {
    tracing::info!(
        channel_id = %channel_id,
        "Channel message received: {} chars",
        text.len()
    );

    // Fetch messages from DB (already sorted by sent_at DESC)
    let display_messages: Option<Vec<MessageDisplayData>> = message_repo
        .list_for_channel(&channel_id, None, 100)
        .ok()
        .map(|messages| {
            let mut display: Vec<MessageDisplayData> = messages
                .iter()
                .map(|msg| {
                    let sender_name = if let Some(sender_id) = &msg.sender_id {
                        user_repo
                            .get(sender_id)
                            .ok()
                            .flatten()
                            .map(|u| u.username)
                            .unwrap_or_else(|| sender_id.to_string())
                    } else {
                        "Unknown".to_string()
                    };
                    // Resolve attachments from DB
                    let resolved_attachments: Vec<Attachment> = msg
                        .attachments
                        .iter()
                        .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                        .collect();
                    message_to_display_data(msg, &sender_name, &resolved_attachments)
                })
                .collect();
            // Reverse to show oldest first (DB returns newest first)
            display.reverse();
            display
        });

    let weak = weak_window.clone();
    let channel_id_str = channel_id.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();

        if selected_channel == channel_id_str {
            if let Some(display_messages) = display_messages {
                // Check if user is at bottom before updating
                let was_at_bottom = window.get_is_at_bottom();

                let slint_messages: Vec<MessageItemData> = display_messages
                    .iter()
                    .map(|m| m.to_slint())
                    .collect();
                let count = slint_messages.len();
                window.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                tracing::debug!("Updated channel view with {} messages from DB", count);

                // Auto-scroll to bottom if user was at bottom before new message
                if was_at_bottom {
                    window.set_message_scroll_position(-99999.0);
                    window.set_is_at_bottom(true);
                }
            }
        }
    });
}

pub(crate) fn handle_channel_typing(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    channel_id: chatter_linux::domain::ids::ChannelId,
    sender_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::debug!(
        channel_id = %channel_id,
        sender_id = %sender_id,
        action,
        "Received channel typing indicator"
    );

    let sender_name = user_repo
        .get(&sender_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "Someone".to_string());

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        if action == 1 {
            window.set_typing_indicator(format!("{} is typing...", sender_name).into());
        } else {
            window.set_typing_indicator("".into());
        }
    });
}

pub(crate) fn handle_channel_reaction(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received channel reaction"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_channel_pin(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        unpin,
        "Received channel pin/unpin"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

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
                    break;
                }
            }
        }

        let current_count = window.get_pinned_total_count();
        if unpin {
            let new_count = (current_count - 1).max(0);
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(new_count > 0);
            if new_count == 0 {
                window.set_pinned_message_preview("".into());
            }
        } else {
            let new_count = current_count + 1;
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(true);
            if !preview.is_empty() {
                window.set_pinned_message_preview(preview.into());
                window.set_pinned_current_index(new_count);
            }
        }
    });
}

pub(crate) fn handle_channel_edit(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received channel message edit"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_channel_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        "Received channel delete-for-everyone"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_channel_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received channel delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != conversation_id {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "Message deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}
