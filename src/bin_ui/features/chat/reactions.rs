//! Reaction callbacks for DM and channel conversations.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_reaction_content, build_reaction_content_with_channel};

use super::helpers::{send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire reaction selected callback for DM and channel conversations.
pub fn wire_reaction_selected(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_reaction_selected(move |message_id: SharedString, emoji: SharedString| {
        let message_id_str = message_id.to_string();
        let emoji_str = emoji.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();

        // DM reaction via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for reaction send");

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
                            tracing::error!("Invalid message ID for reaction: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get sender and timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for reaction: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for reaction: {}", e);
                            return;
                        }
                    };

                    // Get the message author (target_author_aci)
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    // Get the message timestamp (milliseconds since epoch)
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build reaction content
                    // remove=false means adding a reaction
                    let content = build_reaction_content(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database,
                        grpc_pool,
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM reaction: {}", e);
                    } else {
                        tracing::debug!(
                            "Sent DM reaction {} to message {} (author: {}, ts: {}) for peer {}",
                            emoji_str, msg_id, target_author_aci, target_sent_timestamp, peer_id
                        );
                    }
                });
            });
        } else {
            // Channel reaction via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel reaction send");

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
                            tracing::error!("Invalid message ID for channel reaction: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get sender and timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel reaction: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel reaction: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build reaction content with channel context
                    let content = build_reaction_content_with_channel(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
                        Some(&channel_id.to_string()),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel reaction: {}", e);
                    } else {
                        tracing::debug!(
                            "Sent channel reaction {} to message {} in channel {}",
                            emoji_str, msg_id, channel_id
                        );
                    }
                });
            });
        }
    });
}
