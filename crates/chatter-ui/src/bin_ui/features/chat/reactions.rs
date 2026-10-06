//! Reaction callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use crate::{MainWindow, run_ui};

/// Wire reaction selected callback for DM and channel conversations.
pub fn wire_reaction_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();
    // Complete one selection before deciding the next toggle. Rapid taps must
    // not race their own local snapshot or persist in network-completion order.
    let selection_queue = Arc::new(tokio::sync::Mutex::new(()));

    window.on_reaction_selected(move |sender_id: SharedString, timestamp_ns: SharedString, _message_id: SharedString, emoji: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let emoji_str = emoji.to_string();

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        let conversation = ui_ctx.chat_state().active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        let handle = ui_ctx.conversation(conversation.clone());
        let weak = weak_window.clone();
        let client = ui_ctx.clone();
        let selection_queue = selection_queue.clone();

        tokio::spawn(async move {
            let _selection = selection_queue.lock().await;
            let target_author: chatter_core::domain::UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!(sender_id = %sender_id_str, "invalid sender id for reaction");
                    return;
                }
            };
            // Core decides add/remove from local state — UI doesn't query.
            let _now_reacted = match handle
                .toggle_reaction(&emoji_str, &target_author, target_sent_timestamp)
                .await
            {
                Ok(state) => state,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        conversation_type = ?conversation,
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        "Failed to toggle reaction"
                    );
                    return;
                }
            };

            run_ui!(weak, |w| {
                super::helpers::refresh_reaction_in_ui(&w, &client, conversation, &target_author, target_sent_timestamp);
            });
        });
    });
}
