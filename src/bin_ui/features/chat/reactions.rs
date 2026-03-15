//! Reaction callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_reaction_content, build_reaction_content_with_channel};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire reaction selected callback for DM and channel conversations.
pub fn wire_reaction_selected(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let message_service = ui_ctx.message_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    window.on_reaction_selected(move |sender_id: SharedString, timestamp_ns: SharedString, _message_id: SharedString, emoji: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let emoji_str = emoji.to_string();

        // Dedup: check if this emoji already exists on the message, skip if so
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

        if already_reacted {
            tracing::debug!(emoji = %emoji_str, "Skipping duplicate reaction");
            return;
        }

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

        // Handle anonymous DM reactions
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };
            let service = anon_dm_service.clone();
            let emoji_for_send = emoji_str.clone();
            let target_author = sender_id_str.clone();

            tokio::spawn(async move {
                if let Err(e) = service.send_reaction(
                    &anon_dm_id,
                    &emoji_for_send,
                    &target_author,
                    target_sent_timestamp,
                    false, // add, not remove
                ).await {
                    tracing::error!(error = %e, "Failed to send anon DM reaction");
                } else {
                    // Update UI optimistically
                    let emoji_for_update = emoji_for_send.clone();
                    let sender_id_for_ui = target_author.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::update_reaction_in_ui(&w, &sender_id_for_ui, &timestamp_for_ui, &emoji_for_update, false);
                        }
                    });
                }
            });
            return;
        }

        // Handle anonymous group reactions
        if !selected_private_group_id.is_empty() {
            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };
            let service = anon_group_service.clone();
            let emoji_for_send = emoji_str.clone();
            let target_author = sender_id_str.clone();
            let current_user_id = auth_state.get().current_user_id;

            tokio::spawn(async move {
                if let Err(e) = service.send_reaction(
                    &anon_group_id,
                    &emoji_for_send,
                    &target_author,
                    target_sent_timestamp,
                    false, // add, not remove
                ).await {
                    tracing::error!(error = %e, "Failed to send anon group reaction");
                } else {
                    // Persist own reaction to DB
                    if let Some(user_id) = current_user_id {
                        let service_db = service.clone();
                        let emoji_db = emoji_for_send.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Err(e) = service_db.add_reaction(&anon_group_id, target_sent_timestamp, &user_id, &emoji_db) {
                                tracing::warn!(error = %e, "Failed to persist own reaction to DB");
                            }
                        });
                    }

                    // Update UI optimistically
                    let emoji_for_update = emoji_for_send.clone();
                    let sender_id_for_ui = target_author.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::update_reaction_in_ui(&w, &sender_id_for_ui, &timestamp_for_ui, &emoji_for_update, false);
                        }
                    });
                }
            });
            return;
        }

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

                let target_sender_id: UserId = match sender_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg,
                    _ => return,
                };

                let target_author_aci = message.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = build_reaction_content(&emoji_str, false, &target_author_aci, target_sent_timestamp);

                if let Err(e) = send_dm_content(content, &peer_id, message_service, mailbox_manager).await {
                    tracing::error!(error = %e, "Failed to send DM reaction");
                } else {
                    let emoji_for_update = emoji_str.clone();
                    let sender_id_for_ui = sender_id_str.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::update_reaction_in_ui(&w, &sender_id_for_ui, &timestamp_for_ui, &emoji_for_update, false);
                        }
                    });
                }
            });
        } else {
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

                let target_sender_id: UserId = match sender_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg,
                    _ => return,
                };

                let target_author_aci = message.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = build_reaction_content_with_channel(&emoji_str, false, &target_author_aci, target_sent_timestamp, Some(&channel_id.to_string()));

                if let Err(e) = send_channel_content(content, &channel_id, message_service, group_coordinator).await {
                    tracing::error!(error = %e, "Failed to send channel reaction");
                } else {
                    let emoji_for_update = emoji_str.clone();
                    let sender_id_for_ui = sender_id_str.clone();
                    let timestamp_for_ui = timestamp_ns_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::helpers::update_reaction_in_ui(&w, &sender_id_for_ui, &timestamp_for_ui, &emoji_for_update, false);
                        }
                    });
                }
            });
        }
    });
}
