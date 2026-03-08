//! Message editing callbacks for channels, DMs, and anonymous conversations.
//!
//! Handles entering edit mode, sending edits, and cancelling edits.
//! All operations are properly synchronized using Signal-style identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::ids::UserId;
use chatter_linux::services::gateway::builders::{build_edit_content, build_edit_content_with_channel};
use crate::bin_ui::features::chat::helpers::{get_peer_id_from_dm_channel, send_dm_content, send_channel_content};
use crate::MainWindow;

/// Wire edit-message callback - enters edit mode by populating composer.
pub fn wire_edit_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();

    window.on_edit_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        // Get conversation context
        let (selected_dm_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())
            })
            .unwrap_or_default();

        let target_sender_id: UserId = match sender_id_str.parse() {
            Ok(id) => id,
            Err(_) => return,
        };

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        // Get message text based on conversation type
        let text = if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            // Anon DM lookup
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            match anon_dm_service.get_message_by_sender_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                Ok(Some(msg)) => msg.as_text(),
                _ => return,
            }
        } else if !selected_private_group_id.is_empty() {
            // Anon group lookup
            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                Ok(Some(msg)) => msg.as_text(),
                _ => return,
            }
        } else {
            // Regular message lookup
            match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                Ok(Some(msg)) => msg.as_text().map(|s| s.to_string()),
                _ => return,
            }
        };

        if let Some(text) = text {
            let weak = weak_window.clone();
            let composite_key = format!("{}:{}", sender_id_str, timestamp_ns_str);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_text(text.into());
                    w.set_composer_edit_message_id(composite_key.into());
                    w.set_composer_is_editing(true);
                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        "Entered edit mode (Signal-style)"
                    );
                }
            });
        }
    });
}

/// Wire send-edit callback - sends the edited message content.
///
/// Uses Signal-style (sender_id, timestamp_ns) stored in composer_edit_message_id as "sender_id:timestamp_ns".
pub fn wire_send_edit(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let message_service = ui_ctx.message_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    window.on_send_edit(move |composite_key: SharedString, new_text: SharedString| {
        let composite_key_str = composite_key.to_string();
        let new_text_str = new_text.to_string();

        // Parse composite key "sender_id:timestamp_ns"
        let parts: Vec<&str> = composite_key_str.split(':').collect();
        if parts.len() != 2 {
            tracing::error!("Invalid composite key for edit: {}", composite_key_str);
            return;
        }
        let sender_id_str = parts[0].to_string();
        let timestamp_ns_str = parts[1].to_string();

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

        // Handle anonymous DM edit
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
            let new_text_for_update = new_text_str.clone();
            let sender_for_ui = sender_id_str.clone();
            let timestamp_for_ui = timestamp_ns_str.clone();

            tokio::spawn(async move {
                // Send edit to peer
                if let Err(e) = service.send_edit(&anon_dm_id, target_sent_timestamp, &new_text_for_update).await {
                    tracing::error!(error = %e, "Failed to send anon DM edit");
                    return;
                }

                tracing::info!(
                    anon_dm = %anon_dm_id,
                    timestamp = %timestamp_for_ui,
                    "Sent anon DM edit"
                );

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        // Update message content in UI
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .map(|mut m| {
                                if m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                {
                                    m.content = new_text_for_update.clone().into();
                                    m.is_edited = true;
                                }
                                m
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));

                        // Clear edit mode
                        w.set_composer_is_editing(false);
                        w.set_composer_edit_message_id("".into());
                        w.set_composer_text("".into());
                    }
                });
            });
            return;
        }

        // Handle anonymous group edit
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
            let new_text_for_update = new_text_str.clone();
            let sender_for_ui = sender_id_str.clone();
            let timestamp_for_ui = timestamp_ns_str.clone();

            tokio::spawn(async move {
                // Send edit to group
                if let Err(e) = service.send_edit(&anon_group_id, target_sent_timestamp, &new_text_for_update).await {
                    tracing::error!(error = %e, "Failed to send anon group edit");
                    return;
                }

                tracing::info!(
                    anon_group = %anon_group_id,
                    timestamp = %timestamp_for_ui,
                    "Sent anon group edit"
                );

                // Update UI
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        // Update message content in UI
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .map(|mut m| {
                                if m.sender_id.as_str() == sender_for_ui.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                {
                                    m.content = new_text_for_update.clone().into();
                                    m.is_edited = true;
                                }
                                m
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));

                        // Clear edit mode
                        w.set_composer_is_editing(false);
                        w.set_composer_edit_message_id("".into());
                        w.set_composer_text("".into());
                    }
                });
            });
            return;
        }

        // Handle regular DM edit
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
                    Err(e) => {
                        tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for edit");
                        return;
                    }
                };

                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(e) => {
                        tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for edit");
                        return;
                    }
                };

                let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => return,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up message for edit");
                        return;
                    }
                };

                let content = build_edit_content(target_sent_timestamp, &new_text_str);

                if let Err(e) = send_dm_content(content, &peer_id, message_service.clone(), mailbox_manager).await {
                    tracing::error!(error = %e, "Failed to send DM edit");
                    return;
                }

                use chatter_linux::domain::message::MessageContent;
                let mut updated_message = message.clone();
                updated_message.content = MessageContent::Text(new_text_str.clone());
                updated_message.is_edited = true;
                let _ = message_service.update_message(&updated_message);

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_composer_is_editing(false);
                        w.set_composer_edit_message_id("".into());
                        w.set_composer_text("".into());
                    }
                });
            });
        } else {
            // Handle channel edit
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

                let content = build_edit_content_with_channel(target_sent_timestamp, &new_text_str, Some(&channel_id.to_string()));

                if let Err(e) = send_channel_content(content, &channel_id, message_service.clone(), group_coordinator).await {
                    tracing::error!(error = %e, "Failed to send channel edit");
                    return;
                }

                use chatter_linux::domain::message::MessageContent;
                let mut updated_message = message.clone();
                updated_message.content = MessageContent::Text(new_text_str.clone());
                updated_message.is_edited = true;
                let _ = message_service.update_message(&updated_message);

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_composer_is_editing(false);
                        w.set_composer_edit_message_id("".into());
                        w.set_composer_text("".into());
                    }
                });
            });
        }
    });
}

/// Wire cancel-edit callback - exits edit mode without sending.
pub fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_edit(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_composer_text("".into());
                w.set_composer_edit_message_id("".into());
                w.set_composer_is_editing(false);
                tracing::debug!("Cancelled edit mode");
            }
        });
    });
}
