//! Delete message callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;

use crate::{MainWindow, MessageItemData, run_ui};

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
            if let Err(e) = handle
                .delete_for_me(&target_sender_id, target_sent_timestamp)
                .await
            {
                tracing::error!(error = %e, "Failed to delete-for-me");
                return;
            }

            tracing::info!(
                sender_id = %sender_for_ui,
                timestamp = %timestamp_for_ui,
                "Delete-for-me completed"
            );

            // Our own delete raises no domain event; the row's preview has
            // to follow it here.
            crate::bin_ui::features::sidebar::history_changed(&weak, &ui_ctx_spawn);
            run_ui!(weak, |w| {
                if ui_ctx_spawn.chat_state().active_conversation() != conversation {
                    return;
                }
                super::helpers::remove_message_from_ui(
                    &w,
                    &sender_for_ui,
                    &timestamp_for_ui,
                    unregister_message_gifs,
                );
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

        let target_sender_id: UserId = match sender_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "Invalid sender ID for delete-everyone");
                return;
            }
        };
        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(e) => {
                tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for delete-everyone");
                return;
            }
        };

        tokio::spawn(async move {
            let handle = ui_ctx_spawn.conversation(conversation);
            if let Err(e) = handle
                .send_delete(&target_sender_id, target_sent_timestamp)
                .await
            {
                tracing::error!(error = %e, "Failed to send delete-for-everyone");
                return;
            }

            // Persistence is handled by ConversationHandle::send_delete()

            // Update UI — remove message from model
            // Our own delete raises no domain event; the row's preview has
            // to follow it here.
            crate::bin_ui::features::sidebar::history_changed(&weak, &ui_ctx_spawn);
            run_ui!(weak, |w| {
                if ui_ctx_spawn.chat_state().active_conversation() != conversation { return; }
                super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
            });
        });
    });
}

/// Selection deletion is one awaited operation, never a loop over UI callbacks.
/// Core owns server authorization, transport limitations, and local persistence.
pub(super) async fn delete_selected_messages(
    handle: &chatter_core::services::conversation::ConversationHandle,
    targets: &[(UserId, u64)],
    everyone: bool,
    client: &ChatterClient,
    account: UserId,
    session_revision: u64,
) -> chatter_core::services::conversation::ConversationResult {
    if everyone {
        handle
            .send_delete_batch_fenced(targets, account, session_revision)
            .await
    } else {
        for (author, timestamp) in targets {
            let auth = client.auth_state().get();
            if auth.current_user_id != Some(account) || auth.session_revision != session_revision {
                return Err(
                    chatter_core::services::conversation::ConversationError::Other(
                        "The login changed before deletion finished.".into(),
                    ),
                );
            }
            handle.delete_for_me(author, *timestamp).await?;
        }
        Ok(())
    }
}

pub(super) fn remove_selected_messages_from_ui(w: &MainWindow, targets: &[(UserId, u64)]) {
    for (author, timestamp) in targets {
        super::helpers::remove_message_from_ui(
            w,
            &author.to_string(),
            &timestamp.to_string(),
            unregister_message_gifs,
        );
    }
}
