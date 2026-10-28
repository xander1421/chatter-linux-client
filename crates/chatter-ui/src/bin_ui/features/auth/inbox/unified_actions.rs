//! Unified UI action handlers for all conversation types.
//!
//! These handlers replace the duplicated dm/actions.rs and anon_dm_actions.rs
//! for common message actions (typing, receipts,
//! pin). Edits and deletes go through `domain_handler`, keyed by
//! (author, sent timestamp) in the conversation on screen. Each handler checks if the event is for the currently
//! active conversation using `ChatState::active_conversation()`.

use slint::ComponentHandle;
use slint::{Model, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use std::sync::Arc;

use crate::bin_ui::features::auth::inbox::typing::{clear_typing_for_peer, set_typing_with_timer};
use crate::{MainWindow, MessageItemData};

/// Event source identifier for typing/reaction routing.
#[derive(Clone)]
pub enum EventSource {
    /// Channel or DM identified by ChannelId.
    Channel(ChannelId),
}

/// Check if an event source matches the currently active conversation.
fn is_active(chat_state: &ChatterClient, source: &EventSource) -> bool {
    let active = chat_state.active_conversation();
    match source {
        EventSource::Channel(channel_id) => active.is_for_channel(*channel_id),
    }
}

/// The typing-state key for an event: its own conversation, as the sidebar
/// keys it. Never the ACTIVE conversation's — that keyed a STOPPED arriving
/// for another conversation to the open one, leaving the real sender's row
/// and timer uncleared. `ChannelId::from(UserId)` keeps the UUID, so a server
/// DM's key is the peer id the DM row is keyed by.
fn typing_key(source: &EventSource) -> String {
    match source {
        EventSource::Channel(id) => id.to_string(),
    }
}

// ============================================================================
// Typing
// ============================================================================

/// Handle typing indicator for any conversation type.
///
/// `user_id` / `timestamp` are the sender and the sender's clock: a STARTED
/// no newer than that sender's last STOPPED (or message) in this conversation
/// arrived out of order and is dropped (`ChatState::accept_typing`).
pub fn handle_typing(
    weak_window: &slint::Weak<MainWindow>,
    client: &std::sync::Arc<chatter_core::client::ChatterClient>,
    source: EventSource,
    user_id: UserId,
    timestamp: u64,
    is_started: bool,
) {
    let EventSource::Channel(channel_id) = &source;
    let channel_id = *channel_id;
    if !client.update_typing_indicator(channel_id, user_id, timestamp, is_started) {
        tracing::debug!(%channel_id, %user_id, timestamp, "TYPING_UI: dropping STARTED older than its STOPPED");
        return;
    }
    let key = typing_key(&source);
    let client = client.clone();
    let chat_state = client.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };
        let active = is_active(&chat_state, &source);

        let text = client.typing_indicator_text(channel_id);
        if !text.is_empty() {
            set_typing_with_timer(&window, &key, &chat_state, channel_id, &text);
        } else {
            clear_typing_for_peer(&window, &key, active);
        }
    });
}

// ============================================================================
// Receipt
// ============================================================================

/// Handle delivery/read receipt for any conversation type.
pub fn handle_receipt(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &Arc<ChatterClient>,
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
        let new_status = if receipt_type == 2 {
            "read"
        } else {
            "delivered"
        };

        let pane = window.global::<crate::ChatPane>();
        let timestamp_strs: Vec<String> =
            message_timestamps.iter().map(|ts| ts.to_string()).collect();

        // The open thread panel keeps its own model; ticks there must move too.
        for messages in [pane.get_messages(), pane.get_thread_replies()] {
            for i in 0..messages.row_count() {
                if let Some(msg) = messages.row_data(i) {
                    if timestamp_strs
                        .iter()
                        .any(|ts| msg.timestamp_ns.as_str() == ts.as_str())
                    {
                        let mut updated_msg = msg.clone();
                        updated_msg.delivery_status = new_status.into();

                        if let Some(model) = messages
                            .as_any()
                            .downcast_ref::<VecModel<MessageItemData>>()
                        {
                            model.set_row_data(i, updated_msg);
                        }
                    }
                }
            }
        }
    });
}

// ============================================================================
// Pin
// ============================================================================

/// Handle pin/unpin for any conversation type.
pub fn handle_pin(
    weak_window: &slint::Weak<MainWindow>,
    chat_state: &Arc<ChatterClient>,
    source: EventSource,
    target_sent_timestamp: u64,
    is_pinned: bool,
) {
    let chat_state = chat_state.clone();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };

        if !is_active(&chat_state, &source) {
            return;
        }

        let messages = window.global::<crate::ChatPane>().get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    // Generate preview from message content
                    let text = msg.content.to_string();
                    preview = if text.is_empty() {
                        "[Pinned message]".to_string()
                    } else {
                        chatter_core::services::gateway::shared::truncate_preview(&text)
                    };

                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = is_pinned;

                    if let Some(model) = messages
                        .as_any()
                        .downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned-bar substate
                    if let Ok(msg_id) = msg
                        .message_id
                        .to_string()
                        .parse::<chatter_core::domain::ids::MessageId>()
                    {
                        if is_pinned {
                            chat_state.add_pinned_message(msg_id);
                        } else {
                            chat_state.remove_pinned_message(&msg_id);
                        }
                    }
                    break;
                }
            }
        }

        let pinned_count = chat_state.pinned_total_count() as i32;
        window
            .global::<crate::ChatPane>()
            .set_has_pinned_messages(pinned_count > 0);
        window
            .global::<crate::ChatPane>()
            .set_pinned_total_count(pinned_count);
        if is_pinned && !preview.is_empty() {
            window
                .global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            window
                .global::<crate::ChatPane>()
                .set_pinned_current_index(1);
        } else if pinned_count == 0 {
            window
                .global::<crate::ChatPane>()
                .set_pinned_message_preview("".into());
        }
    });
}
