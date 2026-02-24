//! Edit message callbacks for DM and channel conversations.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_edit_content, build_edit_content_with_channel};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire edit-message callback - enters edit mode with original message text.
pub fn wire_edit_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    window.on_edit_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // Parse message ID and look up message
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for edit: {}", e);
                return;
            }
        };

        // Look up the message to get original text
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for edit: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for edit: {}", e);
                return;
            }
        };

        // Extract original text
        use chatter_linux::domain::message::MessageContent;
        let original_text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
            _ => {
                tracing::debug!("Cannot edit non-text message content");
                return;
            }
        };

        // Enter edit mode
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_composer_text(original_text.into());
                w.set_composer_edit_message_id(message_id_str.into());
                w.set_composer_is_editing(true);
                tracing::debug!("Entered edit mode for message: {}", msg_id);
            }
        });
    });
}

/// Wire send-edit callback - sends EditMessage via sealed sender (DM) or sender key (channel).
pub fn wire_send_edit(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    window.on_send_edit(move |message_id: SharedString, new_text: SharedString| {
        let message_id_str = message_id.to_string();
        let new_text_str = new_text.to_string();

        // Get selected DM channel ID and look up peer
        let (selected_dm_id, peer_user_id) = weak_window
            .upgrade()
            .map(|w| {
                let dm_id = w.get_selected_dm_id().to_string();
                let peer_id = if !dm_id.is_empty() {
                    get_peer_id_from_dm_channel(&w, &dm_id)
                } else {
                    None
                };
                (dm_id, peer_id)
            })
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let _grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // DM edit via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => return,
            };

            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for edit message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match peer_user_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for edit: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get original timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for edit: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for edit: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.sent_at;

                    // Build edit content
                    let content = build_edit_content(target_sent_timestamp, &new_text_str);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM edit: {}", e);
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update message locally: {}", e);
                    } else {
                        tracing::debug!("Edited DM message {} for peer {}", msg_id, peer_id);
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        } else {
            // Channel edit via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel edit message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel edit: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get original timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel edit: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel edit: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.sent_at;

                    // Build edit content with channel context
                    let content = build_edit_content_with_channel(
                        target_sent_timestamp,
                        &new_text_str,
                        Some(&channel_id.to_string()),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        group_fanout,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel edit: {}", e);
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update message locally: {}", e);
                    } else {
                        tracing::debug!("Edited channel message {} in channel {}", msg_id, channel_id);
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        }
    });
}

/// Wire cancel-edit callback - exits edit mode without sending.
pub fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_edit(move || {
        let _ = slint::invoke_from_event_loop({
            let weak = weak_window.clone();
            move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    w.set_composer_text("".into());
                    tracing::debug!("Cancelled edit mode");
                }
            }
        });
    });
}
