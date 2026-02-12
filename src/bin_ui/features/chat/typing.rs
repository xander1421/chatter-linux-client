//! Typing indicator callback for DM and channel conversations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_typing_content, build_typing_content_with_channel};

use super::helpers::{send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire typing indicator callback for DM and channel conversations.
pub fn wire_typing_changed(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    // Track typing state to avoid sending duplicate indicators
    let was_typing = Arc::new(AtomicBool::new(false));

    let weak_window = window.as_weak();

    window.on_typing_changed(move |is_typing: bool| {
        // Skip if state hasn't changed
        let prev_typing = was_typing.swap(is_typing, Ordering::SeqCst);
        if prev_typing == is_typing {
            return;
        }

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();

        // DM typing indicator via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for typing indicator");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Build typing indicator content
                    let content = build_typing_content(is_typing);

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
                        tracing::debug!("Failed to send DM typing indicator: {}", e);
                    }
                });
            });
        } else {
            // Channel typing indicator via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel typing indicator");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Build typing indicator content with channel context
                    let content = build_typing_content_with_channel(is_typing, Some(&channel_id.to_string()));

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::debug!("Failed to send channel typing indicator: {}", e);
                    }
                });
            });
        }
    });
}
