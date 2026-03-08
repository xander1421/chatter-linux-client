//! Delete message callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;
use chatter_linux::services::{
    build_delete_for_me_content, build_delete_for_everyone_content,
    build_delete_for_everyone_content_with_channel,
};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire delete-message callback for DM and channel conversations.
/// Delete-for-me: deletes locally and syncs to own devices, does not notify peer/channel.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_delete_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let message_service = ui_ctx.message_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();

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

        // Get selected DM channel ID, peer, and private group ID
        let (selected_dm_id, peer_user_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                let dm_id = w.get_selected_dm_id().to_string();
                let peer_id = if !dm_id.is_empty() {
                    get_peer_id_from_dm_channel(&w, &dm_id)
                } else {
                    None
                };
                let group_id = w.get_selected_private_group_id().to_string();
                (dm_id, peer_id, group_id)
            })
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let message_service = message_service.clone();
        let weak = weak_window.clone();

        // Handle anonymous DM delete-for-me (local only, no sync to peer)
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            // For anon DM delete-for-me, we need to find message by sender+timestamp
            let service = anon_dm_service.clone();
            let sender_for_lookup = sender_id_str.clone();
            let timestamp_for_lookup = timestamp_ns_str.clone();
            let sender_for_ui = sender_id_str.clone();
            let timestamp_for_ui = timestamp_ns_str.clone();

            tokio::spawn(async move {
                // Lookup message by sender_id and timestamp to get the message ID
                let target_sender_id: UserId = match sender_for_lookup.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                let target_sent_timestamp: u64 = match timestamp_for_lookup.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                // Delete locally using timestamp lookup
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

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .filter(|m| {
                                !(m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                    }
                });
            });
            return;
        }

        // Handle anonymous group delete-for-me (local only)
        if !selected_private_group_id.is_empty() {
            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            let service = anon_group_service.clone();
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

                // Delete locally using timestamp lookup
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

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .filter(|m| {
                                !(m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                    }
                });
            });
            return;
        }

        // DM delete-for-me via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => return,
            };

            let mailbox_manager = mailbox_manager.clone();
            let message_service = message_service.clone();
            tokio::spawn(async move {
                if !auth_state.is_logged_in() {
                    return;
                }

                let peer_id: UserId = match peer_user_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

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
                            let current_model = w.get_messages();
                            let before_count = current_model.row_count();
                            let messages: Vec<_> = (0..current_model.row_count())
                                .filter_map(|i| current_model.row_data(i))
                                .filter(|m| {
                                    !(m.sender_id.as_str() == sender_id_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                                })
                                .collect();
                            let after_count = messages.len();
                            tracing::info!(
                                sender_id = %sender_id_for_ui,
                                timestamp = %timestamp_for_ui,
                                before = before_count,
                                after = after_count,
                                "UI updated: removed message from model"
                            );
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                        }
                    });
                }
            });
        } else {
            // Channel delete-for-me via sender key (syncs to own devices)
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_coordinator = group_coordinator.clone();
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
                            let current_model = w.get_messages();
                            let before_count = current_model.row_count();
                            let messages: Vec<_> = (0..current_model.row_count())
                                .filter_map(|i| current_model.row_data(i))
                                .filter(|m| {
                                    !(m.sender_id.as_str() == sender_id_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                                })
                                .collect();
                            let after_count = messages.len();
                            tracing::info!(
                                sender_id = %sender_id_for_ui,
                                timestamp = %timestamp_for_ui,
                                before = before_count,
                                after = after_count,
                                "UI updated: removed channel message from model"
                            );
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                        }
                    });
                }
            });
        }
    });
}

/// Wire delete-everyone-message callback for DM and channel conversations.
/// Sends delete notification to peer/channel so they remove the message too.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_delete_everyone_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let message_service = ui_ctx.message_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();

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

        // Get selected DM channel ID, peer, and private group ID
        let (selected_dm_id, peer_user_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                let dm_id = w.get_selected_dm_id().to_string();
                let peer_id = if !dm_id.is_empty() {
                    get_peer_id_from_dm_channel(&w, &dm_id)
                } else {
                    None
                };
                let group_id = w.get_selected_private_group_id().to_string();
                (dm_id, peer_id, group_id)
            })
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let message_service = message_service.clone();
        let weak = weak_window.clone();

        // Handle anonymous DM delete-for-everyone
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            let service = anon_dm_service.clone();
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

                // Send delete-for-everyone to peer
                if let Err(e) = service.send_delete_for_everyone(&anon_dm_id, target_sent_timestamp).await {
                    tracing::error!(error = %e, "Failed to send anon DM delete-for-everyone");
                    return;
                }

                // Delete locally
                if let Err(e) = service.delete_message_by_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                    tracing::warn!(error = %e, "Failed to delete anon DM message locally after delete-everyone");
                }

                tracing::info!(
                    anon_dm = %anon_dm_id,
                    timestamp = %timestamp_for_lookup,
                    "Sent anon DM delete-for-everyone"
                );

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .filter(|m| {
                                !(m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                    }
                });
            });
            return;
        }

        // Handle anonymous group delete-for-everyone
        if !selected_private_group_id.is_empty() {
            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            let service = anon_group_service.clone();
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

                // Send delete-for-everyone to group
                if let Err(e) = service.send_delete_for_everyone(&anon_group_id, target_sent_timestamp).await {
                    tracing::error!(error = %e, "Failed to send anon group delete-for-everyone");
                    return;
                }

                // Delete locally
                if let Err(e) = service.delete_message_by_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    tracing::warn!(error = %e, "Failed to delete anon group message locally after delete-everyone");
                }

                tracing::info!(
                    anon_group = %anon_group_id,
                    timestamp = %timestamp_for_lookup,
                    "Sent anon group delete-for-everyone"
                );

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .filter(|m| {
                                !(m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                    }
                });
            });
            return;
        }

        // DM delete-for-everyone via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => return,
            };

            let mailbox_manager = mailbox_manager.clone();
            let message_service = message_service.clone();
            tokio::spawn(async move {
                if !auth_state.is_logged_in() {
                    return;
                }

                let peer_id: UserId = match peer_user_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

                // Parse sender_id for Signal-style lookup
                let target_sender_id: UserId = match sender_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for delete-everyone");
                        return;
                    }
                };

                // Parse timestamp for Signal-style lookup
                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(e) => {
                        tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for delete-everyone");
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
                            "Message not found for delete-everyone (Signal-style lookup)"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up message for delete-everyone");
                        return;
                    }
                };

                let msg_id = message.id;

                tracing::debug!(
                    sender_id = %sender_id_str,
                    timestamp = target_sent_timestamp,
                    peer = %peer_id,
                    "Sending DM delete-for-everyone"
                );

                // Build delete-for-everyone content
                let content = build_delete_for_everyone_content(target_sent_timestamp);

                if let Err(e) = send_dm_content(
                    content,
                    &peer_id,
                    message_service.clone(),
                    mailbox_manager,
                )
                .await
                {
                    tracing::error!(error = %e, "Failed to send DM delete-everyone");
                    return;
                }

                tracing::info!(
                    sender_id = %sender_id_str,
                    timestamp = %timestamp_ns_str,
                    peer = %peer_id,
                    "DM delete-for-everyone sent successfully"
                );

                // Delete locally
                if let Err(e) = message_service.delete_message_local(&msg_id) {
                    tracing::error!(error = %e, "Failed to delete message locally");
                } else {
                    tracing::debug!(
                        sender_id = %sender_id_str,
                        peer = %peer_id,
                        "Deleted DM message locally for everyone"
                    );

                    // Update UI to remove message using Signal-style lookup
                    let sender_id_for_ui = sender_id_str.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let current_model = w.get_messages();
                            let before_count = current_model.row_count();
                            let messages: Vec<_> = (0..current_model.row_count())
                                .filter_map(|i| current_model.row_data(i))
                                .filter(|m| {
                                    !(m.sender_id.as_str() == sender_id_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                                })
                                .collect();
                            let after_count = messages.len();
                            tracing::info!(
                                sender_id = %sender_id_for_ui,
                                timestamp = %timestamp_for_ui,
                                before = before_count,
                                after = after_count,
                                "UI updated: removed message (delete-everyone)"
                            );
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                        }
                    });
                }
            });
        } else {
            // Channel delete-everyone via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_coordinator = group_coordinator.clone();
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
                        tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for channel delete-everyone");
                        return;
                    }
                };

                // Parse timestamp for Signal-style lookup
                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(e) => {
                        tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for channel delete-everyone");
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
                            "Message not found for channel delete-everyone (Signal-style lookup)"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up message for channel delete-everyone");
                        return;
                    }
                };

                let msg_id = message.id;

                tracing::debug!(
                    sender_id = %sender_id_str,
                    timestamp = target_sent_timestamp,
                    channel = %channel_id,
                    "Sending channel delete-for-everyone"
                );

                // Build delete-for-everyone content with channel context
                let channel_id_str = channel_id.to_string();
                let content = build_delete_for_everyone_content_with_channel(
                    target_sent_timestamp,
                    Some(&channel_id_str),
                );

                if let Err(e) = send_channel_content(
                    content,
                    &channel_id,
                    message_service.clone(),
                    group_coordinator,
                )
                .await
                {
                    tracing::error!(error = %e, "Failed to send channel delete-everyone");
                    return;
                }

                tracing::info!(
                    sender_id = %sender_id_str,
                    timestamp = %timestamp_ns_str,
                    channel = %channel_id,
                    "Channel delete-for-everyone sent successfully"
                );

                // Delete locally
                if let Err(e) = message_service.delete_message_local(&msg_id) {
                    tracing::error!(error = %e, "Failed to delete message locally");
                } else {
                    tracing::debug!(
                        sender_id = %sender_id_str,
                        channel = %channel_id,
                        "Deleted channel message locally for everyone"
                    );

                    // Update UI to remove message using Signal-style lookup
                    let sender_id_for_ui = sender_id_str.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let current_model = w.get_messages();
                            let before_count = current_model.row_count();
                            let messages: Vec<_> = (0..current_model.row_count())
                                .filter_map(|i| current_model.row_data(i))
                                .filter(|m| {
                                    !(m.sender_id.as_str() == sender_id_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str())
                                })
                                .collect();
                            let after_count = messages.len();
                            tracing::info!(
                                sender_id = %sender_id_for_ui,
                                timestamp = %timestamp_for_ui,
                                before = before_count,
                                after = after_count,
                                "UI updated: removed channel message (delete-everyone)"
                            );
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                        }
                    });
                }
            });
        }
    });
}

