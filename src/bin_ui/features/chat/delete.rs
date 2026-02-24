//! Delete message callbacks for DM and channel conversations.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{
    build_delete_for_me_content, build_delete_for_everyone_content,
    build_delete_for_everyone_content_with_channel,
};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire delete-message callback for DM and channel conversations.
/// Delete-for-me: deletes locally and syncs to own devices, does not notify peer/channel.
pub fn wire_delete_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    window.on_delete_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        tracing::info!(message_id = %message_id_str, "UI callback: delete-for-me triggered");

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

        // DM delete-for-me via sealed sender
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
                    .expect("Failed to create runtime for delete message");

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
                            tracing::error!("Invalid message ID for delete: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for delete: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for delete: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.sent_at;
                    let conversation_id = peer_id.to_string();

                    // Build delete-for-me content (syncs to own devices)
                    let content = build_delete_for_me_content(
                        &conversation_id,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    tracing::debug!(
                        message_id = %msg_id,
                        peer = %peer_id,
                        author = %target_author_aci,
                        timestamp = target_sent_timestamp,
                        "Sending DM delete-for-me"
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM delete-for-me");
                        return;
                    }

                    tracing::info!(message_id = %msg_id, "DM delete-for-me sent successfully");

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(message_id = %msg_id, peer = %peer_id, "Deleted DM message locally");

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let before_count = current_model.row_count();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                let after_count = messages.len();
                                tracing::info!(
                                    message_id = %message_id_str,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed message from model"
                                );
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        } else {
            // Channel delete-for-me via sender key (syncs to own devices)
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel delete message");

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
                            tracing::error!("Invalid message ID for channel delete: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel delete: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel delete: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.sent_at;
                    let conversation_id = channel_id.to_string();

                    tracing::debug!(
                        message_id = %msg_id,
                        channel = %channel_id,
                        author = %target_author_aci,
                        timestamp = target_sent_timestamp,
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
                        message_service,
                        group_fanout,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send channel delete-for-me");
                        return;
                    }

                    tracing::info!(message_id = %msg_id, channel = %channel_id, "Channel delete-for-me sent successfully");

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(message_id = %msg_id, channel = %channel_id, "Deleted channel message locally");

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let before_count = current_model.row_count();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                let after_count = messages.len();
                                tracing::info!(
                                    message_id = %message_id_str,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed channel message from model"
                                );
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        }
    });
}

/// Wire delete-everyone-message callback for DM and channel conversations.
/// Sends delete notification to peer/channel so they remove the message too.
pub fn wire_delete_everyone_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    window.on_delete_everyone_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        tracing::info!(message_id = %message_id_str, "UI callback: delete-for-everyone triggered");

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

        // DM delete-for-everyone via sealed sender
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
                    .expect("Failed to create runtime for delete-everyone message");

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
                            tracing::error!("Invalid message ID for delete-everyone: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for delete-everyone: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for delete-everyone: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.sent_at;

                    tracing::debug!(
                        message_id = %msg_id,
                        peer = %peer_id,
                        timestamp = target_sent_timestamp,
                        "Sending DM delete-for-everyone"
                    );

                    // Build delete-for-everyone content
                    let content = build_delete_for_everyone_content(target_sent_timestamp);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM delete-everyone");
                        return;
                    }

                    tracing::info!(message_id = %msg_id, peer = %peer_id, "DM delete-for-everyone sent successfully");

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(message_id = %msg_id, peer = %peer_id, "Deleted DM message locally for everyone");

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let before_count = current_model.row_count();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                let after_count = messages.len();
                                tracing::info!(
                                    message_id = %message_id_str,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed message (delete-everyone)"
                                );
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        } else {
            // Channel delete-for-everyone via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel delete-everyone message");

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
                            tracing::error!("Invalid message ID for channel delete-everyone: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel delete-everyone: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel delete-everyone: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.sent_at;

                    tracing::debug!(
                        message_id = %msg_id,
                        channel = %channel_id,
                        timestamp = target_sent_timestamp,
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
                        message_service,
                        group_fanout,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send channel delete-everyone");
                        return;
                    }

                    tracing::info!(message_id = %msg_id, channel = %channel_id, "Channel delete-for-everyone sent successfully");

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!(error = %e, "Failed to delete message locally");
                    } else {
                        tracing::debug!(message_id = %msg_id, channel = %channel_id, "Deleted channel message locally for everyone");

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let before_count = current_model.row_count();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                let after_count = messages.len();
                                tracing::info!(
                                    message_id = %message_id_str,
                                    before = before_count,
                                    after = after_count,
                                    "UI updated: removed channel message (delete-everyone)"
                                );
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        }
    });
}
