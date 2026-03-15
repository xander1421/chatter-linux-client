//! Delete message callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::build_delete_for_me_content;
use chatter_linux::services::conversation::ConversationHandle;
use chatter_linux::state::ActiveConversation;

use super::helpers::{send_channel_content, send_dm_content};
use crate::MessageItemData;
use crate::MainWindow;

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
pub fn wire_delete_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_delete_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            "UI callback: delete-for-me triggered (Signal-style lookup)"
        );

        let conversation = ui_ctx.chat_state.active_conversation();
        let weak = weak_window.clone();

        match conversation {
            ActiveConversation::AnonDm(anon_dm_id) => {
                let service = ui_ctx.anon_dm_service.clone();
                let sender_for_lookup = sender_id_str.clone();
                let timestamp_for_lookup = timestamp_ns_str.clone();
                let sender_for_ui = sender_id_str.clone();
                let timestamp_for_ui = timestamp_ns_str.clone();

                tokio::spawn(async move {
                    let target_sender_id: UserId = match sender_for_lookup.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let target_sent_timestamp: u64 = match timestamp_for_lookup.parse() {
                        Ok(ts) => ts,
                        Err(_) => return,
                    };

                    if let Err(e) = service.delete_message_by_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                        tracing::error!(error = %e, "Failed to delete anon DM message locally");
                        return;
                    }

                    tracing::info!(
                        anon_dm = %anon_dm_id,
                        sender = %sender_for_lookup,
                        timestamp = %timestamp_for_lookup,
                        "Deleted anon DM message locally (delete-for-me)"
                    );

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
                        }
                    });
                });
            }

            ActiveConversation::AnonGroup(anon_group_id) => {
                let service = ui_ctx.anon_group_service.clone();
                let sender_for_lookup = sender_id_str.clone();
                let timestamp_for_lookup = timestamp_ns_str.clone();
                let sender_for_ui = sender_id_str.clone();
                let timestamp_for_ui = timestamp_ns_str.clone();

                tokio::spawn(async move {
                    let target_sender_id: UserId = match sender_for_lookup.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let target_sent_timestamp: u64 = match timestamp_for_lookup.parse() {
                        Ok(ts) => ts,
                        Err(_) => return,
                    };

                    if let Err(e) = service.delete_message_by_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                        tracing::error!(error = %e, "Failed to delete anon group message locally");
                        return;
                    }

                    tracing::info!(
                        anon_group = %anon_group_id,
                        sender = %sender_for_lookup,
                        timestamp = %timestamp_for_lookup,
                        "Deleted anon group message locally (delete-for-me)"
                    );

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
                        }
                    });
                });
            }

            ActiveConversation::Dm { peer_id, .. } => {
                let auth_state = ui_ctx.auth_state.clone();
                let mailbox_manager = ui_ctx.mailbox_manager.clone();
                let message_service = ui_ctx.message_service.clone();

                tokio::spawn(async move {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    // Parse sender_id for Signal-style lookup
                    let target_sender_id: UserId = match sender_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for delete");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for delete");
                            return;
                        }
                    };

                    // Signal-style lookup: (sender_id, sent_at)
                    let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                "Message not found for delete (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for delete");
                            return;
                        }
                    };

                    let msg_id = message.id;
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let conversation_id = peer_id.to_string();

                    // Build delete-for-me content (syncs to own devices)
                    let content = build_delete_for_me_content(
                        &conversation_id,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        peer = %peer_id,
                        author = %target_author_aci,
                        "Sending DM delete-for-me"
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        message_service.clone(),
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM delete-for-me");
                        return;
                    }

                    tracing::info!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        "DM delete-for-me sent successfully"
                    );

                    // Delete locally
                    if let Err(e) = message_service.delete_message_local(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            peer = %peer_id,
                            "Deleted DM message locally"
                        );

                        // Update UI to remove message using Signal-style lookup
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let before_count = w.get_messages().row_count();
                                super::helpers::remove_message_from_ui(&w, &sender_id_for_ui, &timestamp_for_ui, unregister_message_gifs);
                                let after_count = w.get_messages().row_count();
                                tracing::info!(
                                    sender_id = %sender_id_for_ui,
                                    timestamp = %timestamp_for_ui,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed message from model"
                                );
                            }
                        });
                    }
                });
            }

            ActiveConversation::Channel(_) => {
                let auth_state = ui_ctx.auth_state.clone();
                let chat_state = ui_ctx.chat_state.clone();
                let message_service = ui_ctx.message_service.clone();
                let group_coordinator = ui_ctx.group_coordinator.clone();

                tokio::spawn(async move {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse sender_id for Signal-style lookup
                    let target_sender_id: UserId = match sender_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for channel delete");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for channel delete");
                            return;
                        }
                    };

                    // Signal-style lookup: (sender_id, sent_at)
                    let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                "Message not found for channel delete (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for channel delete");
                            return;
                        }
                    };

                    let msg_id = message.id;
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let conversation_id = channel_id.to_string();

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        channel = %channel_id,
                        author = %target_author_aci,
                        "Sending channel delete-for-me"
                    );

                    // Build delete-for-me content (syncs to own devices)
                    let content = build_delete_for_me_content(
                        &conversation_id,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service.clone(),
                        group_coordinator,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send channel delete-for-me");
                        return;
                    }

                    tracing::info!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        channel = %channel_id,
                        "Channel delete-for-me sent successfully"
                    );

                    // Delete locally
                    if let Err(e) = message_service.delete_message_local(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            channel = %channel_id,
                            "Deleted channel message locally"
                        );

                        // Update UI to remove message using Signal-style lookup
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let before_count = w.get_messages().row_count();
                                super::helpers::remove_message_from_ui(&w, &sender_id_for_ui, &timestamp_for_ui, unregister_message_gifs);
                                let after_count = w.get_messages().row_count();
                                tracing::info!(
                                    sender_id = %sender_id_for_ui,
                                    timestamp = %timestamp_for_ui,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed channel message from model"
                                );
                            }
                        });
                    }
                });
            }

            ActiveConversation::None => return,
        }
    });
}

/// Wire delete-everyone-message callback for DM and channel conversations.
/// Sends delete notification to peer/channel so they remove the message too.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_delete_everyone_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_delete_everyone_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            "UI callback: delete-for-everyone triggered (Signal-style lookup)"
        );

        let handle = ConversationHandle::current(&ui_ctx);
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
            if let Err(e) = handle.send_delete(target_sent_timestamp).await {
                tracing::error!(error = %e, "Failed to send delete-for-everyone");
                return;
            }

            // Update UI — remove message from model
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    super::helpers::remove_message_from_ui(&w, &sender_for_ui, &timestamp_for_ui, unregister_message_gifs);
                }
            });
        });
    });
}
