//! DM action handlers.
//!
//! Handles typing, receipts, reactions, pins, edits, and deletes.

use slint::{Model, VecModel};

use crate::bin_ui::features::chat::get_dm_channel_from_peer;
use crate::{MainWindow, MessageItemData};

use super::super::typing::{clear_typing_for_peer, clear_typing_timer, set_typing_with_timer};
use super::super::utils::update_reaction_summary;

/// Handle DM typing indicator.
pub fn handle_dm_typing(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::info!(
        peer_id = %peer_id,
        action,
        action_name = if action == 1 { "STARTED" } else { "STOPPED" },
        "TYPING_HANDLER: ===> handle_dm_typing called <==="
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let result = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            tracing::warn!("TYPING_INDICATOR: Window upgrade failed!");
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);

        let cond1_match = peer_channel_id.as_deref() == Some(selected_dm.as_str());
        let cond2_match = selected_dm == peer_id_str;

        tracing::info!(
            selected_dm = %selected_dm,
            peer_id = %peer_id_str,
            peer_channel_id = ?peer_channel_id,
            cond1_match,
            cond2_match,
            "TYPING_INDICATOR: Checking active conversation"
        );

        let is_active_conversation = cond1_match || cond2_match;

        if !is_active_conversation {
            tracing::warn!(
                peer_id = %peer_id_str,
                selected_dm = %selected_dm,
                peer_channel_id = ?peer_channel_id,
                "TYPING_INDICATOR: NOT active conversation - skipping UI update!"
            );
            if action != 1 {
                clear_typing_timer(&peer_id_str);
            }
            return;
        }

        tracing::info!(
            peer_id = %peer_id_str,
            action,
            action_name = if action == 1 { "STARTED" } else { "STOPPED" },
            "TYPING_INDICATOR: IS active conversation - calling set_typing_with_timer"
        );

        if action == 1 {
            tracing::info!(peer_id = %peer_id_str, "TYPING_INDICATOR: Calling set_typing_with_timer NOW");
            set_typing_with_timer(&window, &peer_id_str);
            tracing::info!(peer_id = %peer_id_str, "TYPING_INDICATOR: set_typing_with_timer RETURNED");
        } else {
            tracing::info!(peer_id = %peer_id_str, "TYPING_INDICATOR: Calling clear_typing_for_peer");
            clear_typing_for_peer(&window, &peer_id_str);
        }
    });

    if result.is_err() {
        tracing::error!("TYPING_INDICATOR: invoke_from_event_loop FAILED to schedule!");
    }
}

/// Handle DM receipt (delivery/read).
pub fn handle_dm_receipt(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::info!(
        peer_id = %peer_id,
        receipt_type,
        count = timestamps.len(),
        timestamps = ?timestamps,
        "RECEIPT_RECEIVED: Processing delivery/read receipt from peer"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let result = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            tracing::warn!("RECEIPT_RECEIVED: Window upgrade failed!");
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);

        tracing::info!(
            selected_dm = %selected_dm,
            peer_channel_id = ?peer_channel_id,
            peer_id = %peer_id_str,
            "RECEIPT_RECEIVED: Checking if this is the active conversation"
        );

        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            tracing::debug!(
                "RECEIPT_RECEIVED: Not active conversation, skipping UI update (receipt still valid)"
            );
            return;
        }

        let messages = window.get_messages();
        let msg_count = messages.row_count();
        let mut matched_count = 0;

        tracing::info!(
            msg_count,
            receipt_timestamps = ?timestamps,
            "RECEIPT_RECEIVED: Scanning messages for timestamp match"
        );

        for i in 0..msg_count {
            if let Some(msg) = messages.row_data(i) {
                if !msg.is_own {
                    continue;
                }

                let msg_ts_ns = msg.timestamp_ns.to_string();

                // Try exact string match (both should be nanoseconds)
                let matching = timestamps.iter().any(|&ts| {
                    let ts_str = ts.to_string();
                    ts_str == msg_ts_ns
                });

                if matching {
                    matched_count += 1;
                    tracing::info!(
                        msg_ts_ns = %msg_ts_ns,
                        new_status = status,
                        "RECEIPT_RECEIVED: Updating message delivery status"
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
            "RECEIPT_RECEIVED: Finished processing receipts"
        );
    });

    if result.is_err() {
        tracing::error!("RECEIPT_RECEIVED: invoke_from_event_loop FAILED to schedule!");
    }
}

/// Handle DM reaction.
pub fn handle_dm_reaction(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received reaction"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
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
                    updated_msg.reactions = slint::ModelRc::new(VecModel::from(
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

/// Handle DM pin/unpin.
pub fn handle_dm_pin(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    pinned_state: chatter_linux::state::PinnedState,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        unpin,
        "Received pin/unpin"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
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
                    preview = super::super::utils::generate_preview_from_ui_message(&msg);

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

/// Handle DM message edit.
pub fn handle_dm_edit(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received message edit"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
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

/// Handle DM delete-for-me sync.
pub fn handle_dm_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != conversation_id {
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

/// Handle DM delete-for-everyone from peer.
pub fn handle_dm_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        "Received delete-for-everyone from peer"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
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
