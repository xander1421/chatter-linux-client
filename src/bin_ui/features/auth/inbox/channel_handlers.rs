//! Channel-specific event handlers.
//!
//! Handles channel message events: received, typing, receipts, reactions, pins, edits, deletes.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::attachment::Attachment;
use chatter_linux::proto::content_v1::AttachmentPointer;

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::{ChannelListItemData, MainWindow, MessageItemData};

use super::attachment_download::download_attachments;
use super::utils::update_reaction_summary;

// =============================================================================
// Channel Event Handlers
// =============================================================================

pub(crate) fn handle_channel_message_received(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    text: String,
) {
    tracing::info!(
        channel_id = %channel_id,
        text_len = text.len(),
        "UI: Channel message received, fetching from DB"
    );

    // Fetch messages from DB (already sorted by sent_at DESC)
    let display_messages: Option<Vec<MessageDisplayData>> = ui_ctx.message_service
        .get_messages_local(&channel_id, None, 100)
        .ok()
        .map(|messages| {
            let mut display: Vec<MessageDisplayData> = messages
                .iter()
                .map(|msg| {
                    let sender_name = if let Some(sender_id) = &msg.sender_id {
                        // Use profile's display_name, fall back to UUID
                        ui_ctx.user_service
                            .get_profile_local(sender_id)
                            .ok()
                            .flatten()
                            .map(|p| p.display_name)
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| sender_id.to_string())
                    } else {
                        "Unknown".to_string()
                    };
                    // Resolve attachments from DB
                    let resolved_attachments: Vec<Attachment> = msg
                        .attachments
                        .iter()
                        .filter_map(|att_id| ui_ctx.attachment_service.get_metadata(att_id).ok().flatten())
                        .collect();

                    if !msg.attachments.is_empty() {
                        tracing::info!(
                            message_id = %msg.id,
                            stored_attachment_ids = ?msg.attachments.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                            resolved_count = resolved_attachments.len(),
                            "Resolving attachments for message"
                        );
                    }

                    message_to_display_data(msg, &sender_name, &resolved_attachments)
                })
                .collect();
            // Reverse to show oldest first (DB returns newest first)
            display.reverse();
            // Pre-decode attachment thumbnails before UI thread
            for msg in &mut display {
                msg.decode_attachment_thumbnails();
            }
            display
        });

    let weak = weak_window.clone();
    let ui_ctx_cloned = ui_ctx.clone();
    let channel_id_str = channel_id.to_string();
    // Get current user ID for is_own calculation
    let current_user_id = ui_ctx.user_service.current_user_id().ok().map(|id| id.to_string()).unwrap_or_default();

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
                    .map(|m| m.to_slint_with_user(&current_user_id))
                    .collect();
                let count = slint_messages.len();
                window.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                tracing::debug!("Updated channel view with {} messages from DB", count);

                // Auto-register GIFs for animation
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_cloned.attachment_service.clone(),
                );

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
    ui_ctx: &Arc<UIContext>,
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

    let sender_name = ui_ctx.user_service
        .get_user_local(&sender_id)
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
                    updated_msg.reaction_summary = new_summary.clone().into();
                    updated_msg.has_reactions = !new_summary.is_empty();
                    updated_msg.reactions = slint::ModelRc::new(slint::VecModel::from(
                        crate::bin_ui::converters::parse_reaction_summary_to_entries(&new_summary),
                    ));

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
    pinned_state: chatter_linux::state::PinnedState,
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
        let new_pinned = !unpin;
        let mut preview = String::new();

        // Update is_pinned on the message and generate preview
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    preview = super::utils::generate_preview_from_ui_message(&msg);

                    // Update is_pinned on the message
                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = new_pinned;
                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned_state with message ID
                    if let Ok(msg_id) = msg.message_id.to_string().parse::<chatter_linux::domain::ids::MessageId>() {
                        if new_pinned {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                    }
                    break;
                }
            }
        }

        let pinned_count = pinned_state.get().pinned_message_ids.len() as i32;
        if unpin {
            window.set_pinned_total_count(pinned_count);
            window.set_has_pinned_messages(pinned_count > 0);
            if pinned_count == 0 {
                window.set_pinned_message_preview("".into());
            }
        } else {
            window.set_pinned_total_count(pinned_count);
            window.set_has_pinned_messages(true);
            window.set_pinned_message_preview(preview.into());
            window.set_pinned_current_index(pinned_count);
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

// =============================================================================
// Receipt Handler (delivery/read status for channel messages)
// =============================================================================

pub(crate) fn handle_channel_receipt(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &std::sync::Arc<chatter_linux::app::UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::info!(
        channel_id = %channel_id,
        receipt_type,
        count = timestamps.len(),
        timestamps = ?timestamps,
        "CHANNEL_RECEIPT: Processing delivery/read receipt"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    // Persist delivery status to database for each timestamp
    let message_service = ui_ctx.message_service.clone();
    for &ts in &timestamps {
        match message_service.update_delivery_status(&channel_id, ts, status) {
            Ok(true) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    sent_at = ts,
                    status,
                    "CHANNEL_RECEIPT: Updated delivery_status in DB"
                );
            }
            Ok(false) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    sent_at = ts,
                    "CHANNEL_RECEIPT: No message found in DB for timestamp"
                );
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    sent_at = ts,
                    error = %e,
                    "CHANNEL_RECEIPT: Failed to update delivery_status in DB"
                );
            }
        }
    }

    // Update UI if the channel is currently selected
    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            // Channel is not selected, but we already updated the DB
            return;
        }

        let messages = window.get_messages();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if !msg.is_own {
                    continue;
                }

                // Compare nanosecond timestamps as strings
                let msg_ts_ns = msg.timestamp_ns.to_string();
                let matching = timestamps.iter().any(|&ts| ts.to_string() == msg_ts_ns);
                if matching {
                    tracing::info!(
                        msg_timestamp = msg_ts_ns,
                        new_status = status,
                        "CHANNEL_RECEIPT: Updating message delivery_status in UI"
                    );
                    let mut updated_msg = msg.clone();
                    updated_msg.delivery_status = status.into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                }
            }
        }
    });
}

// =============================================================================
// Unread Count Handler
// =============================================================================

pub(crate) fn handle_channel_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    unread_count: u32,
) {
    tracing::debug!(
        channel_id = %channel_id,
        unread_count,
        "Channel unread count updated"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let channels = window.get_channels();

        for i in 0..channels.row_count() {
            if let Some(channel) = channels.row_data(i) {
                if channel.channel_id.as_str() == channel_id_str {
                    let mut updated_channel = channel.clone();
                    updated_channel.unread_count = unread_count as i32;

                    if let Some(model) = channels.as_any().downcast_ref::<VecModel<ChannelListItemData>>()
                    {
                        model.set_row_data(i, updated_channel);
                    }
                    return;
                }
            }
        }
    });
}

// =============================================================================
// Sidebar Preview Update
// =============================================================================

/// Update the channel sidebar preview with a sent message.
pub fn update_channel_sidebar_preview(
    window: &MainWindow,
    channel_id_str: &str,
    preview: &str,
    timestamp_str: &str,
) {
    let channels = window.get_channels();

    // Find and update the channel entry
    for i in 0..channels.row_count() {
        if let Some(channel) = channels.row_data(i) {
            if channel.channel_id.as_str() == channel_id_str {
                let mut updated_channel = channel.clone();
                updated_channel.last_message = preview.into();
                updated_channel.timestamp = timestamp_str.into();

                if let Some(model) = channels.as_any().downcast_ref::<VecModel<ChannelListItemData>>()
                {
                    model.set_row_data(i, updated_channel);
                }
                return;
            }
        }
    }

    tracing::debug!(
        channel_id = %channel_id_str,
        "Channel sidebar entry not found for sent message preview update"
    );
}

// =============================================================================
// Auto-Download Entry Point (called from inbox event handler)
// =============================================================================

/// Trigger auto-download for channel attachments (spawns background task).
pub(crate) fn trigger_channel_attachment_download(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    attachments: Vec<AttachmentPointer>,
) {
    if attachments.is_empty() {
        return;
    }

    let weak = weak_window.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let grpc_pool = ui_ctx.grpc_pool.clone();
    let channel_str = channel_id.to_string();

    tokio::spawn(async move {
        download_attachments(
            attachments,
            attachment_service,
            grpc_pool,
            weak,
            channel_str,
        ).await;
    });
}
