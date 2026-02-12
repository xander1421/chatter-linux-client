//! Send message callback for channels and DMs.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::message_client::MessageClient;

use crate::bin_ui::converters::{get_initials, message_to_display_data};
use crate::{MainWindow, MessageItemData, PendingAttachmentData};

/// Wire the main send message callback.
pub fn wire_send(window: &MainWindow, ctx: &AppContext) {
    let message_service = ctx.message_service();
    let chat_state = ctx.chat_state.clone();
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let channel_repo = ctx.channel_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    // Get group fanout for NDSS '21 mailbox-based delivery
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        let chat_state_check = chat_state.clone();
        let has_attachments = !chat_state_check.get_pending_attachments().is_empty();

        // Must have text OR attachments to send
        if text.trim().is_empty() && !has_attachments {
            return;
        }

        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let mailbox_manager = mailbox_manager.clone();
        let database = database.clone();
        let attachment_repo = attachment_repo.clone();
        let group_fanout = group_fanout.clone();
        let weak = weak_window.clone();

        // Get the selected DM ID from UI (if any)
        let selected_dm_id = weak
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        // Check if this is a DM or channel message
        let is_dm = !selected_dm_id.is_empty();

        if is_dm {
            // Send via MessageService for DM privacy with Signal Protocol forward secrecy
            // Use spawn_blocking because libsignal stores aren't Send
            let grpc_pool = grpc_pool.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for DM send");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        tracing::warn!("Cannot send DM: not logged in");
                        return;
                    }

                    let peer_id: chatter_linux::domain::UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid DM peer ID: {}", e);
                            return;
                        }
                    };

                    // Use MessageService for two-layer encryption
                    let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                        grpc_pool.channel(),
                        grpc_pool.auth_clone(),
                    );

                    // Get current user info for display
                    let current_user_name = auth_state
                        .get()
                        .display_name
                        .unwrap_or_else(|| "Me".to_string());

                    match message_service
                        .send_dm_text(&mut auth_client, &peer_id, &text, database, &mailbox_manager, &grpc_pool)
                        .await
                    {
                        Ok(()) => {
                            tracing::debug!("DM sent to peer {} via MessageService", peer_id);

                            // Add sent message to UI immediately
                            let sent_text = text.clone();
                            let sender_name = current_user_name.clone();
                            let sender_initials = get_initials(&sender_name);
                            let timestamp = chrono::Utc::now();
                            let timestamp_str = timestamp.format("%H:%M").to_string();
                            let message_id = uuid::Uuid::new_v4().to_string();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Create message item for sent DM
                                    let msg_item = MessageItemData {
                                        message_id: message_id.into(),
                                        sender_id: SharedString::default(), // Own message
                                        sender_name: sender_name.into(),
                                        sender_initials: sender_initials.into(),
                                        sender_avatar_url: SharedString::default(),
                                        content: sent_text.into(),
                                        timestamp: timestamp_str.into(),
                                        timestamp_ms: timestamp.timestamp_millis() as i32,
                                        is_own: true,
                                        is_edited: false,
                                        is_deleted: false,
                                        is_system: false,
                                        is_ai: false,
                                        has_attachments: false,
                                        attachment_count: 0,
                                        attachments: ModelRc::new(VecModel::from(Vec::new())),
                                        has_reactions: false,
                                        reaction_summary: SharedString::default(),
                                        reply_count: 0,
                                        reply_count_text: SharedString::default(),
                                        is_thread_reply: false,
                                        show_sender: true,
                                        delivery_status: "sent".into(),
                                    };

                                    // Add to message list
                                    let current_model = w.get_messages();
                                    let mut messages: Vec<_> = (0..current_model.row_count())
                                        .filter_map(|i| current_model.row_data(i))
                                        .collect();
                                    messages.push(msg_item);
                                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                                    w.set_composer_text("".into());
                                    w.set_composer_placeholder("Type a message...".into());
                                }
                            });
                        }
                        Err(chatter_linux::services::message_service::MessageError::BootstrapInitiated) => {
                            // First DM - bootstrap was initiated, inform user
                            tracing::info!("DM bootstrap initiated for peer {}", peer_id);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Keep the text so user can retry after bootstrap completes
                                    w.set_composer_placeholder(
                                        "Setting up secure channel... Please wait and retry.".into(),
                                    );
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to send DM: {}", e);
                        }
                    }
                });
            });
        } else {
            // Send via regular channel message flow
            let channel_repo = channel_repo.clone();
            let user_repo = user_repo.clone();
            let attachment_repo = attachment_repo.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for message send");

                rt.block_on(async {
                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Cannot send message: no active channel");
                            return;
                        }
                    };

                    if !auth_state.is_logged_in() {
                        tracing::warn!("Cannot send message: not logged in");
                        return;
                    }

                    // Check if this is a SavedMessages channel
                    let is_saved_messages = match channel_repo.get(&channel_id) {
                        Ok(Some(channel)) => {
                            tracing::debug!(
                                "Channel {} type: {:?}, is_saved_messages: {}",
                                channel_id,
                                channel.channel_type,
                                channel.channel_type == ChannelType::SavedMessages
                            );
                            channel.channel_type == ChannelType::SavedMessages
                        }
                        Ok(None) => {
                            tracing::warn!("Channel {} not found in repo for SavedMessages check", channel_id);
                            false
                        }
                        Err(e) => {
                            tracing::error!("Failed to get channel {} for SavedMessages check: {}", channel_id, e);
                            false
                        }
                    };

                    let mut message_client =
                        MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                    let mut auth_client =
                        chatter_linux::network::grpc::auth_client::AuthClient::new(
                            grpc_pool.channel(),
                            grpc_pool.auth_clone(),
                        );

                    let mut channel_client =
                        ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                    // Get pending attachments from chat state
                    let pending_attachment_ids = chat_state.get_pending_attachment_ids();
                    tracing::debug!(
                        "Sending message with {} attachments",
                        pending_attachment_ids.len()
                    );

                    if is_saved_messages {
                        // Use Signal-style Note to Self handling
                        match message_service
                            .send_saved_message(
                                &mut auth_client,
                                &mut message_client,
                                &text,
                                pending_attachment_ids.clone(),
                            )
                            .await
                        {
                            Ok(message) => {
                                // Clear pending attachments after successful send
                                chat_state.clear_pending_attachments();

                                // Get sender name for display
                                let sender_name = if let Some(sender_id) = &message.sender_id {
                                    user_repo
                                        .get_profile(sender_id)
                                        .ok()
                                        .flatten()
                                        .map(|p| p.display_name)
                                        .or_else(|| {
                                            user_repo.get(sender_id).ok().flatten().map(|u| u.username)
                                        })
                                        .unwrap_or_else(|| "Me".to_string())
                                } else {
                                    "Me".to_string()
                                };

                                // Resolve attachments for display
                                let resolved_attachments: Vec<_> = message
                                    .attachments
                                    .iter()
                                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                                    .collect();

                                // Convert to display data (thread-safe)
                                let display_msg = message_to_display_data(&message, &sender_name, &resolved_attachments);

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Convert to Slint type on UI thread
                                        let slint_msg = display_msg.to_slint();

                                        // Add message to the end of the list
                                        let current_model = w.get_messages();
                                        let mut messages: Vec<_> = (0..current_model.row_count())
                                            .filter_map(|i| current_model.row_data(i))
                                            .collect();
                                        messages.push(slint_msg);
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Clear attachment tray UI
                                        w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                        w.set_composer_placeholder("Type a message...".into());
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to send saved message: {}", e);
                            }
                        }
                    } else {
                        // Regular channel message
                        match message_service
                            .send_message(
                                &mut message_client,
                                &mut auth_client,
                                &mut channel_client,
                                group_fanout.as_deref(),
                                &channel_id,
                                &text,
                                pending_attachment_ids.clone(),
                                None,
                                vec![],
                                false,
                            )
                            .await
                        {
                            Ok(message) => {
                                // Clear pending attachments after successful send
                                chat_state.clear_pending_attachments();

                                // Get sender name for display
                                let sender_name = if let Some(sender_id) = &message.sender_id {
                                    user_repo
                                        .get_profile(sender_id)
                                        .ok()
                                        .flatten()
                                        .map(|p| p.display_name)
                                        .or_else(|| {
                                            user_repo.get(sender_id).ok().flatten().map(|u| u.username)
                                        })
                                        .unwrap_or_else(|| "Me".to_string())
                                } else {
                                    "Me".to_string()
                                };

                                // Resolve attachments for display
                                let resolved_attachments: Vec<_> = message
                                    .attachments
                                    .iter()
                                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                                    .collect();

                                // Convert to display data (thread-safe)
                                let display_msg = message_to_display_data(&message, &sender_name, &resolved_attachments);

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Convert to Slint type on UI thread
                                        let slint_msg = display_msg.to_slint();

                                        // Add message to the end of the list
                                        let current_model = w.get_messages();
                                        let mut messages: Vec<_> = (0..current_model.row_count())
                                            .filter_map(|i| current_model.row_data(i))
                                            .collect();
                                        messages.push(slint_msg);
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Clear attachment tray UI
                                        w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                        w.set_composer_placeholder("Type a message...".into());
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        }
                    }
                });
            });
        }
    });
}
