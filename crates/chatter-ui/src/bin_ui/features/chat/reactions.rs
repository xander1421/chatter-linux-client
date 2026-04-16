//! Reaction callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::services::conversation::ConversationHandle;
use chatter_core::state::ActiveConversation;

use crate::{MainWindow, run_ui};

/// Wire reaction selected callback for DM and channel conversations.
pub fn wire_reaction_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_reaction_selected(move |sender_id: SharedString, timestamp_ns: SharedString, _message_id: SharedString, emoji: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let emoji_str = emoji.to_string();

        // Check if user already reacted with this emoji — toggle it off if so
        let already_reacted = weak_window.upgrade().map(|w| {
            let current_model = w.get_messages();
            (0..current_model.row_count())
                .filter_map(|i| current_model.row_data(i))
                .any(|m| {
                    m.sender_id.as_str() == sender_id_str
                        && m.timestamp_ns.as_str() == timestamp_ns_str
                        && m.reaction_summary.to_string().contains(&emoji_str)
                })
        }).unwrap_or(false);

        let remove = already_reacted;

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        let conversation = ui_ctx.chat_state().active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        let handle = ConversationHandle::new(conversation.clone(), ui_ctx.clone());
        let weak = weak_window.clone();

        tokio::spawn(async move {
            if let Err(e) = handle.send_reaction(
                &emoji_str, &sender_id_str, target_sent_timestamp, remove,
            ).await {
                tracing::error!(
                    error = %e,
                    conversation_type = ?conversation,
                    sender_id = %sender_id_str,
                    timestamp = target_sent_timestamp,
                    remove,
                    "Failed to send reaction"
                );
                return;
            }

            // Update UI
            run_ui!(weak, |w| {
                super::helpers::update_reaction_in_ui(&w, &sender_id_str, &timestamp_ns_str, &emoji_str, remove);
            });
        });
    });
}
