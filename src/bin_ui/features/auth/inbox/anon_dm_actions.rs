//! Anonymous DM action handlers.
//!
//! Handles typing, receipts, reactions, pins, edits, and deletes for anonymous DMs.
//! These handlers use `conversation_id: ChannelId` (derived from `AnonDmId`) instead of
//! `peer_id` which is used for regular DMs.

use slint::{Model, VecModel};

use crate::bin_ui::features::auth::inbox::typing::{clear_typing_for_peer, set_typing_with_timer};
use crate::bin_ui::features::auth::inbox::utils::update_reaction_summary;
use crate::{MainWindow, MessageItemData};

/// Format conversation ID for UI matching.
///
/// The UI uses `"anon_dm:{uuid}"` format for selected_dm_id.
fn format_anon_dm_id(channel_id: &chatter_linux::domain::ChannelId) -> String {
    format!("anon_dm:{}", channel_id)
}

/// Check if the given channel_id (from AnonDmId) matches the currently selected anon DM.
fn is_active_anon_dm(window: &MainWindow, channel_id: &chatter_linux::domain::ChannelId) -> bool {
    let selected_dm = window.get_selected_dm_id().to_string();
    let expected = format_anon_dm_id(channel_id);
    let is_active = selected_dm == expected;

    tracing::debug!(
        selected_dm = %selected_dm,
        expected = %expected,
        is_active = is_active,
        "[ANON_DM_RECEIPT] Checking if active conversation"
    );

    is_active
}

/// Handle anonymous DM typing indicator.
///
/// Called when `DomainEvent::TypingStarted/Stopped` is received for an anon DM.
/// The `channel_id` is derived from `AnonDmId` via `Into<ChannelId>`.
pub fn handle_anon_dm_typing(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    user_id: chatter_linux::domain::UserId,
    is_started: bool,
) {
    let channel_id_str = format_anon_dm_id(&channel_id);

    tracing::info!(
        channel_id = %channel_id,
        user_id = %user_id,
        is_started,
        "[ANON_DM_TYPING] Received typing indicator"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            tracing::warn!("[ANON_DM_TYPING] Window upgrade failed!");
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
            tracing::debug!(
                channel_id = %channel_id,
                selected = %window.get_selected_dm_id(),
                "[ANON_DM_TYPING] Not active conversation - skipping UI update"
            );
            return;
        }

        if is_started {
            tracing::info!(
                channel_id = %channel_id_str,
                "[ANON_DM_TYPING] Setting typing indicator"
            );
            // Use channel_id as the key for typing timer (same pattern as peer_id for DMs)
            // This now handles both header and sidebar with auto-clear timer
            set_typing_with_timer(&window, &channel_id_str);
        } else {
            tracing::info!(
                channel_id = %channel_id_str,
                "[ANON_DM_TYPING] Clearing typing indicator"
            );
            // This now clears both header and sidebar
            clear_typing_for_peer(&window, &channel_id_str);
        }
    });
}

/// Handle anonymous DM receipt (delivery/read).
///
/// Called when `DomainEvent::MessagesRead/Delivered` is received.
/// Updates message delivery status in the UI for the active anon DM.
pub fn handle_anon_dm_receipt(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::info!(
        channel_id = %channel_id,
        receipt_type,
        count = timestamps.len(),
        "[ANON_DM_RECEIPT] Processing receipt"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            tracing::warn!("[ANON_DM_RECEIPT] Window upgrade failed!");
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
            tracing::debug!(
                channel_id = %channel_id,
                "[ANON_DM_RECEIPT] Not active conversation - skipping UI update"
            );
            return;
        }

        let messages = window.get_messages();
        let msg_count = messages.row_count();
        let mut matched_count = 0;

        // Debug: log all receipt timestamps
        tracing::info!(
            timestamps = ?timestamps,
            "[ANON_DM_RECEIPT] Receipt timestamps to match"
        );

        for i in 0..msg_count {
            if let Some(msg) = messages.row_data(i) {
                // Debug: log each own message's timestamp
                if msg.is_own {
                    tracing::info!(
                        msg_id = %msg.message_id,
                        msg_ts_ns = %msg.timestamp_ns,
                        is_own = msg.is_own,
                        current_status = %msg.delivery_status,
                        "[ANON_DM_RECEIPT] Own message in UI"
                    );
                }

                if !msg.is_own {
                    continue;
                }

                let msg_ts_ns = msg.timestamp_ns.to_string();

                // Try exact string match (both should be nanoseconds)
                let matching = timestamps.iter().any(|&ts| {
                    let ts_str = ts.to_string();
                    let matches = ts_str == msg_ts_ns;
                    if !matches {
                        tracing::debug!(
                            receipt_ts = %ts_str,
                            msg_ts = %msg_ts_ns,
                            "[ANON_DM_RECEIPT] Timestamp comparison (no match)"
                        );
                    }
                    matches
                });

                if matching {
                    matched_count += 1;
                    tracing::debug!(
                        msg_ts_ns = %msg_ts_ns,
                        new_status = status,
                        "[ANON_DM_RECEIPT] Updating message delivery status"
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

        tracing::info!(
            matched_count,
            receipt_count = timestamps.len(),
            "[ANON_DM_RECEIPT] Finished processing receipts"
        );
    });
}

/// Handle anonymous DM reaction.
///
/// Called when `DomainEvent::ReactionAdded/Removed` is received for an anon DM message.
pub fn handle_anon_dm_reaction(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "[ANON_DM_REACTION] Received reaction"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
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

                    tracing::debug!(
                        target = target_sent_timestamp,
                        emoji = %emoji,
                        "[ANON_DM_REACTION] Updated message reactions"
                    );
                    return;
                }
            }
        }
    });
}

/// Handle anonymous DM pin/unpin.
///
/// Called when `DomainEvent::MessagePinned/Unpinned` is received for an anon DM.
pub fn handle_anon_dm_pin(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    pinned_state: chatter_linux::state::PinnedState,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        unpin,
        "[ANON_DM_PIN] Received pin/unpin"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
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
            tracing::debug!(
                target = target_sent_timestamp,
                pinned_count,
                "[ANON_DM_PIN] Message unpinned"
            );
        } else {
            window.set_pinned_total_count(pinned_count);
            window.set_has_pinned_messages(true);
            window.set_pinned_message_preview(preview.into());
            window.set_pinned_current_index(pinned_count);
            tracing::debug!(
                target = target_sent_timestamp,
                pinned_count,
                "[ANON_DM_PIN] Message pinned"
            );
        }
    });
}

/// Handle anonymous DM message edit.
///
/// Called when a peer edits their message in an anon DM conversation.
pub fn handle_anon_dm_edit(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "[ANON_DM_EDIT] Received message edit"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
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

                    tracing::info!(
                        target = target_sent_timestamp,
                        "[ANON_DM_EDIT] Updated message content"
                    );
                    return;
                }
            }
        }
    });
}

/// Handle anonymous DM delete-for-everyone from peer.
///
/// Called when `DomainEvent::MessageDeleted` is received for an anon DM.
pub fn handle_anon_dm_delete(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ChannelId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        "[ANON_DM_DELETE] Received delete-for-everyone"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
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

                    tracing::info!(
                        target = target_sent_timestamp,
                        "[ANON_DM_DELETE] Marked message as deleted"
                    );
                    return;
                }
            }
        }
    });
}

/// Handle anonymous DM message deleted event.
///
/// Called when `DomainEvent::AnonDmMessageDeleted` is received.
/// This is the specific anon DM delete event (not the generic MessageDeleted).
pub fn handle_anon_dm_message_deleted(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: chatter_linux::domain::anon_dm::AnonDmId,
    message_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        message_timestamp,
        "[ANON_DM_DELETE] Received AnonDmMessageDeleted event"
    );

    let channel_id: chatter_linux::domain::ChannelId = conversation_id.into();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = message_timestamp.to_string();

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

                    tracing::info!(
                        message_timestamp,
                        "[ANON_DM_DELETE] Marked message as deleted via AnonDmMessageDeleted"
                    );
                    return;
                }
            }
        }
    });
}

/// Handle anonymous DM message edited event.
///
/// Called when `DomainEvent::AnonDmMessageEdited` is received.
pub fn handle_anon_dm_message_edited(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: chatter_linux::domain::anon_dm::AnonDmId,
    message_timestamp: u64,
    new_text: String,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        message_timestamp,
        "[ANON_DM_EDIT] Received AnonDmMessageEdited event"
    );

    let channel_id: chatter_linux::domain::ChannelId = conversation_id.into();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Check if this is the active conversation
        if !is_active_anon_dm(&window, &channel_id) {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = message_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_text.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }

                    tracing::info!(
                        message_timestamp,
                        "[ANON_DM_EDIT] Updated message content via AnonDmMessageEdited"
                    );
                    return;
                }
            }
        }
    });
}
