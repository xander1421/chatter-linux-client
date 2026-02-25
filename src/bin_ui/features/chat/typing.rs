//! Typing indicator callback for DM and channel conversations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_typing_content, build_typing_content_with_channel};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire typing indicator callback for DM and channel conversations.
pub fn wire_typing_changed(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    // Track typing state to avoid sending duplicate indicators
    let was_typing = Arc::new(AtomicBool::new(false));

    let weak_window = window.as_weak();

    window.on_typing_changed(move |is_typing: bool| {
        tracing::debug!(
            is_typing,
            "TYPING_SEND: on_typing_changed callback triggered"
        );

        // Skip if state hasn't changed
        let prev_typing = was_typing.swap(is_typing, Ordering::SeqCst);
        if prev_typing == is_typing {
            tracing::debug!(
                is_typing,
                prev_typing,
                "TYPING_SEND: State unchanged, skipping"
            );
            return;
        }

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
                tracing::debug!(
                    dm_id = %dm_id,
                    peer_id = ?peer_id,
                    "TYPING_SEND: Looked up peer from DM channel"
                );
                (dm_id, peer_id)
            })
            .unwrap_or_default();

        let auth_state = auth_state.clone();

        // DM typing indicator via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => {
                    tracing::warn!(
                        selected_dm_id = %selected_dm_id,
                        "TYPING_SEND: No peer_user_id found for DM channel - cannot send"
                    );
                    return;
                }
            };
            tracing::info!(
                selected_dm_id = %selected_dm_id,
                peer_user_id = %peer_user_id,
                is_typing,
                "TYPING_SEND: Sending DM typing indicator"
            );

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

                    let peer_id: UserId = match peer_user_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Build typing indicator content
                    let content = build_typing_content(is_typing);

                    match send_dm_content(
                        content,
                        &peer_id,
                        database,
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::info!(
                                peer_id = %peer_id,
                                is_typing,
                                "TYPING_SEND: DM typing indicator sent successfully"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer_id = %peer_id,
                                error = %e,
                                "TYPING_SEND: Failed to send DM typing indicator"
                            );
                        }
                    }
                });
            });
        } else {
            // Channel typing indicator via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

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

                    // Skip typing indicators for SavedMessages (channel_id == user_id)
                    if let Some(user_id) = auth_state.get().current_user_id {
                        if channel_id.to_string() == user_id.to_string() {
                            return;
                        }
                    }

                    // Build typing indicator content with channel context
                    let content = build_typing_content_with_channel(is_typing, Some(&channel_id.to_string()));

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        group_fanout,
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
