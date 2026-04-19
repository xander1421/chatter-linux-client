//! Delete message callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;

use crate::{MessageItemData, MainWindow, run_ui};

/// Unregister any GIF animations from a message's attachments.
///
/// Call this when a message is deleted to free GIF animation memory.
fn unregister_message_gifs(message: &MessageItemData) {
    if !message.has_attachments {
        return;
    }

    let attachments = message.attachments.iter();
    for att in attachments {
        let mime = att.mime_type.to_string();
        if mime.starts_with("image/gif") || mime == "image/webp" {
            crate::bin_ui::gif_state::unregister_inline_gif(&att.id);
        }
    }
}

/// Wire delete-message callback for DM and channel conversations.
/// Delete-for-me: deletes locally and syncs to own devices, does not notify peer/channel.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_delete_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_delete_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            "UI callback: delete-for-me triggered (Signal-style lookup)"
        );

        let conversation = ui_ctx.chat_state().active_conversation();
        let ui_ctx_spawn = ui_ctx.clone();
        let weak = weak_window.clone();
        let sender_for_ui = sender_id_str.clone();
        let timestamp_for_ui = timestamp_ns_str.clone();

        tokio::spawn(async move {
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid sender ID for delete");
                    return;
                }
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid timestamp for delete");
                    return;
                }
            };

            let handle = ui_ctx_spawn.conversation(conversation);
            if let Err(e) = handle.delete_for_me(&target_sender_id, target_sent_timestamp).await {
                tracing::error!(error = %e, "Failed to delete-for-me");
                return;
            }

            tracing::info!(
                sender_id = %sender_for_ui,
                timestamp = %timestamp_for_ui,
                "Delete-for-me completed"
            );

            run_ui!(weak, |w| {
                super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
            });
        });
    });
}

/// Wire delete-everyone-message callback for DM and channel conversations.
/// Sends delete notification to peer/channel so they remove the message too.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_delete_everyone_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_delete_everyone_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            "UI callback: delete-for-everyone triggered (Signal-style lookup)"
        );

        let conversation = ui_ctx.chat_state().active_conversation();
        let ui_ctx_spawn = ui_ctx.clone();
        let weak = weak_window.clone();
        let sender_for_ui = sender_id_str.clone();
        let timestamp_for_ui = timestamp_ns_str.clone();

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(e) => {
                tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for delete-everyone");
                return;
            }
        };

        tokio::spawn(async move {
            let handle = ui_ctx_spawn.conversation(conversation);
            if let Err(e) = handle.send_delete(target_sent_timestamp).await {
                tracing::error!(error = %e, "Failed to send delete-for-everyone");
                return;
            }

            // Persistence is handled by ConversationHandle::send_delete()

            // Update UI — remove message from model
            run_ui!(weak, |w| {
                super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
            });
        });
    });
}
