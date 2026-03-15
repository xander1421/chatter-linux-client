//! Unified UI action handlers for all conversation types.
//!
//! These handlers replace the duplicated dm/actions.rs, anon_dm_actions.rs,
//! and anon_group_handlers.rs for common message actions (typing, reaction,
//! pin, edit, delete). Each handler checks if the event is for the currently
//! active conversation using `ChatState::active_conversation()`.

use slint::{Model, VecModel};

use chatter_linux::domain::ids::{ChannelId, UserId};
use chatter_linux::state::{ActiveConversation, ChatState};

use crate::bin_ui::features::auth::inbox::typing::{clear_typing_for_peer, set_typing_with_timer};
use crate::bin_ui::features::auth::inbox::utils::update_reaction_summary;
use crate::{MainWindow, MessageItemData};

/// Flexible event identifier — events can come with different ID types.
#[derive(Clone)]
pub enum EventSource {
    /// Server-managed channel/DM identified by ChannelId.
    Channel(ChannelId),
    /// DM identified by peer UserId (legacy InboxEvent format).
    Peer(UserId),
}

/// Check if an event source matches the currently active conversation.
fn is_active(chat_state: &ChatState, source: &EventSource) -> bool {
    let active = chat_state.active_conversation();
    match source {
        EventSource::Channel(channel_id) => {
            active.channel_id().map(|id| id == *channel_id).unwrap_or(false)
        }
        EventSource::Peer(peer_id) => {
            matches!(active, ActiveConversation::Dm { peer_id: active_peer, .. } if active_peer == *peer_id)
        }
    }
}

/// Get a display key for typing indicators.
fn typing_key(chat_state: &ChatState, source: &EventSource) -> String {
    let active = chat_state.active_conversation();
    match active {
        ActiveConversation::Dm { peer_id, .. } => peer_id.to_string(),
        ActiveConversation::AnonDm(id) => format!("anon_dm:{}", ChannelId::from(id)),
        ActiveConversation::AnonGroup(id) => id.to_string(),
        ActiveConversation::Channel(id) => id.to_string(),
        ActiveConversation::None => match source {
            EventSource::Channel(id) => id.to_string(),
            EventSource::Peer(id) => id.to_string(),
        },
    }
}

// ============================================================================
// Typing
// ============================================================================

/// Handle typing indicator for any conversation type.
pub fn handle_typing(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    source: EventSource,
    is_started: bool,
) {
    let key = typing_key(chat_state, &source);
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            if !is_started {
                clear_typing_for_peer(&window, &key);
            }
            return;
        }

        if is_started {
            set_typing_with_timer(&window, &key);
        } else {
            clear_typing_for_peer(&window, &key);
        }
    });
}

// ============================================================================
// Reaction
// ============================================================================

/// Handle reaction for any conversation type.
pub fn handle_reaction(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    source: EventSource,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
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

                    if let Some(model) =
                        messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

// ============================================================================
// Receipt
// ============================================================================

/// Handle delivery/read receipt for any conversation type.
pub fn handle_receipt(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    source: EventSource,
    receipt_type: i32,
    message_timestamps: Vec<u64>,
) {
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            return;
        }

        // 1 = delivery, 2 = read
        let new_status = if receipt_type == 2 { "read" } else { "delivered" };

        let messages = window.get_messages();
        let timestamp_strs: Vec<String> =
            message_timestamps.iter().map(|ts| ts.to_string()).collect();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if timestamp_strs.iter().any(|ts| msg.timestamp_ns.as_str() == ts.as_str()) {
                    let mut updated_msg = msg.clone();
                    updated_msg.delivery_status = new_status.into();

                    if let Some(model) =
                        messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                }
            }
        }
    });
}

// ============================================================================
// Edit
// ============================================================================

/// Handle message edit for any conversation type.
pub fn handle_edit(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    source: EventSource,
    target_sent_timestamp: u64,
    new_text: String,
) {
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_text.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) =
                        messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

// ============================================================================
// Delete
// ============================================================================

/// Handle message deletion for any conversation type.
pub fn handle_delete(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    source: EventSource,
    target_sent_timestamp: u64,
) {
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        let mut new_messages = Vec::new();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    // Unregister GIF animations
                    if msg.has_attachments {
                        let attachments = msg.attachments.iter();
                        for att in attachments {
                            let mime = att.mime_type.to_string();
                            if mime.starts_with("image/gif") || mime == "image/webp" {
                                crate::bin_ui::gif_state::unregister_inline_gif(&att.id);
                            }
                        }
                    }
                } else {
                    new_messages.push(msg);
                }
            }
        }
        window.set_messages(slint::ModelRc::new(VecModel::from(new_messages)));
    });
}

// ============================================================================
// Pin
// ============================================================================

/// Handle pin/unpin for any conversation type.
pub fn handle_pin(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &ChatState,
    pinned_state: &chatter_linux::state::PinnedState,
    source: EventSource,
    target_sent_timestamp: u64,
    is_pinned: bool,
) {
    let chat_state = chat_state.clone();
    let pinned_state = pinned_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    // Generate preview from message content
                    let text = msg.content.to_string();
                    preview = if text.is_empty() {
                        "[Pinned message]".to_string()
                    } else if text.len() > 80 {
                        format!("{}...", &text[..77])
                    } else {
                        text
                    };

                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = is_pinned;

                    if let Some(model) =
                        messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned_state
                    if let Ok(msg_id) =
                        msg.message_id.to_string().parse::<chatter_linux::domain::ids::MessageId>()
                    {
                        if is_pinned {
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
        window.set_has_pinned_messages(pinned_count > 0);
        window.set_pinned_total_count(pinned_count);
        if is_pinned && !preview.is_empty() {
            window.set_pinned_message_preview(preview.into());
            window.set_pinned_current_index(pinned_count);
        } else if pinned_count == 0 {
            window.set_pinned_message_preview("".into());
        }
    });
}
