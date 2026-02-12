//! Pin message callback for DM and channel conversations.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_pin_content, build_pin_content_with_channel};

use super::helpers::{send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire pin-message callback for DM and channel conversations.
pub fn wire_pin_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_pin_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();

        // DM pin via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for pin message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for pin: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for pin: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for pin: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    // Build pin content
                    let content = build_pin_content(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        grpc_pool,
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM pin: {}", e);
                        return;
                    }

                    // Update locally
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = !unpin;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update pin status locally: {}", e);
                    } else {
                        tracing::debug!("{} DM message {} for peer {}", action, msg_id, peer_id);
                    }
                });
            });
        } else {
            // Channel pin via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel pin message");

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
                            tracing::error!("Invalid message ID for channel pin: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel pin: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel pin: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    // Build pin content with channel context
                    let channel_id_str = channel_id.to_string();
                    let content = build_pin_content_with_channel(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                        Some(&channel_id_str),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel pin: {}", e);
                        return;
                    }

                    // Update locally
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = !unpin;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update pin status locally: {}", e);
                    } else {
                        tracing::debug!("{} channel message {} in channel {}", action, msg_id, channel_id);
                    }
                });
            });
        }
    });
}
