//! Send message callback for channels and DMs.
//!
//! Uses MessageService for all message operations - no direct network calls.

use std::path::PathBuf;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::services::{AttachmentService, SendDmMsgParams};

use crate::bin_ui::converters::{get_initials, message_to_display_data, attachment_to_display_data};
use crate::bin_ui::features::chat::helpers::get_peer_id_from_dm_channel;
use crate::{DMListItemData, MainWindow, MessageAttachmentData, MessageItemData, PendingAttachmentData};

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

        // Take snapshot of pending attachments BEFORE clearing
        let pending_attachments_snapshot = chat_state_check.get_pending_attachments();
        let has_attachments = !pending_attachments_snapshot.is_empty();

        // Must have text OR attachments to send
        if text.trim().is_empty() && !has_attachments {
            return;
        }

        // Clear pending attachments IMMEDIATELY to prevent double-send
        // This must happen before spawning async task
        if has_attachments {
            chat_state_check.clear_pending_attachments();
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

        tracing::info!(
            is_dm,
            selected_dm_id = %selected_dm_id,
            text_len = text.len(),
            attachment_count = pending_attachments_snapshot.len(),
            "UI: send button clicked"
        );

        // Clear attachment UI immediately to prevent double-clicks
        if has_attachments {
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        if is_dm {
            // Send via MessageService for DM privacy with Signal Protocol forward secrecy
            // Use spawn_blocking because libsignal stores aren't Send
            let grpc_pool = grpc_pool.clone();
            let attachment_repo = attachment_repo.clone();
            // Use the snapshot of pending attachments (already cleared from state)
            let pending_attachments = pending_attachments_snapshot.clone();

            // selected_dm_id is the channel_id - we need to find the peer's user_id from the DM list
            let peer_user_id = weak
                .upgrade()
                .and_then(|w| get_peer_id_from_dm_channel(&w, &selected_dm_id));

            tracing::debug!(
                selected_dm_id = %selected_dm_id,
                peer_user_id = ?peer_user_id,
                "UI: looked up peer_user_id from channel_id"
            );

            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => {
                    tracing::error!("Could not find peer user_id for DM channel {}", selected_dm_id);
                    return;
                }
            };

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

                    let peer_id: chatter_linux::domain::UserId = match peer_user_id.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid DM peer ID: {}", e);
                            return;
                        }
                    };

                    // Create attachment service for encrypted CDN uploads
                    let attachment_service = AttachmentService::new(attachment_repo.clone());

                    // Use the snapshot of pending attachments (already cleared from state before spawn)
                    let mut attachment_pointers: Vec<chatter_linux::proto::content_v1::AttachmentPointer> = Vec::new();
                    let mut uploaded_attachment_ids: Vec<chatter_linux::domain::ids::AttachmentId> = Vec::new();

                    for att in &pending_attachments {
                        let file_path = PathBuf::from(&att.path);

                        tracing::info!(
                            filename = %att.filename,
                            path = %att.path.display(),
                            size = att.size,
                            mime_type = %att.mime_type,
                            "Encrypting and uploading DM attachment to CDN"
                        );

                        // Upload with encryption to CDN
                        match attachment_service.upload_to_cdn_encrypted(&grpc_pool, &file_path).await {
                            Ok(result) => {
                                tracing::info!(
                                    cdn_key = %result.cdn_key,
                                    attachment_id = %result.attachment_id,
                                    filename = %result.file_name,
                                    "DM attachment uploaded to CDN and stored locally"
                                );
                                uploaded_attachment_ids.push(result.attachment_id);
                                attachment_pointers.push(result.to_attachment_pointer());
                            }
                            Err(e) => {
                                tracing::error!(
                                    filename = %att.filename,
                                    error = %e,
                                    "Failed to upload DM attachment to CDN"
                                );
                                // Continue with remaining attachments
                            }
                        }
                    }

                    // Generate timestamp ONCE - used for proto, storage, AND UI
                    // This ensures the message has the same timestamp everywhere
                    let sent_at = chatter_linux::util::time::TimestampNanos::now().as_u64();

                    tracing::info!(
                        peer_id = %peer_id,
                        text_len = text.len(),
                        attachment_count = attachment_pointers.len(),
                        sent_at = sent_at,
                        "UI: calling message_service.send_dm_msg()"
                    );

                    // Get current user info for display
                    let current_user_name = auth_state
                        .get()
                        .display_name
                        .unwrap_or_else(|| "Me".to_string());

                    // Store attachment count for UI update
                    let sent_attachment_count = attachment_pointers.len();

                    // Get the DM channel ID for storage
                    let dm_channel_id: chatter_linux::domain::ids::ChannelId = selected_dm_id.parse().unwrap_or_else(|_| {
                        // Fallback: use peer_id as channel_id (for new DMs)
                        peer_id.into()
                    });

                    // Clone attachment IDs for use in error case (BootstrapInitiated)
                    let uploaded_ids_for_display = uploaded_attachment_ids.clone();

                    // Use MessageService for two-layer encryption + local storage
                    match message_service
                        .send_dm_msg(SendDmMsgParams {
                            peer_id: &peer_id,
                            text: &text,
                            attachment_pointers,
                            attachment_ids: uploaded_attachment_ids,
                            dm_channel_id: &dm_channel_id,
                            database,
                            mailbox_manager: &mailbox_manager,
                            sent_at,
                        })
                        .await
                    {
                        Ok(message) => {
                            tracing::debug!(
                                peer_id = %peer_id,
                                message_id = %message.id,
                                attachment_count = message.attachments.len(),
                                "DM sent and stored locally via MessageService"
                            );

                            // Look up the stored CDN attachments from repo (they have correct IDs and waveforms)
                            // These were stored with cdn_key-based IDs during upload_to_cdn_encrypted
                            let attachment_display_data: Vec<_> = message.attachments
                                .iter()
                                .filter_map(|att_id| {
                                    attachment_repo.get(att_id).ok().flatten()
                                })
                                .map(|att| attachment_to_display_data(&att))
                                .collect();

                            // Add sent message to UI immediately
                            let sent_text = text.clone();
                            let sender_name = current_user_name.clone();
                            let sender_initials = get_initials(&sender_name);
                            // Convert UTC to local time for display
                            let timestamp = message.created_at.with_timezone(&chrono::Local);
                            let timestamp_str = timestamp.format("%H:%M").to_string();
                            let message_id_str = message.id.to_string();
                            let message_sent_at = message.sent_at; // For timestamp_ns matching
                            let has_attachments = sent_attachment_count > 0;

                            // Clone data needed for sidebar update
                            let sidebar_peer_id = peer_id.to_string();
                            let sidebar_preview = if sent_text.len() > 50 {
                                format!("You: {}...", &sent_text[..47])
                            } else {
                                format!("You: {}", sent_text)
                            };
                            let sidebar_timestamp = timestamp_str.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Convert attachment display data to Slint types (must be on UI thread)
                                    let slint_attachments: Vec<MessageAttachmentData> = attachment_display_data
                                        .iter()
                                        .map(|a| a.to_slint())
                                        .collect();

                                    // Create message item for sent DM
                                    // IMPORTANT: Use the message's actual sent_at for timestamp_ns
                                    // so reactions can match by timestamp
                                    let msg_item = MessageItemData {
                                        message_id: message_id_str.into(),
                                        sender_id: SharedString::default(), // Own message
                                        sender_name: sender_name.into(),
                                        sender_initials: sender_initials.into(),
                                        sender_avatar_url: SharedString::default(),
                                        content: sent_text.into(),
                                        timestamp: timestamp_str.clone().into(),
                                        timestamp_ns: message_sent_at.to_string().into(),
                                        is_own: true,
                                        is_edited: false,
                                        is_deleted: false,
                                        is_system: false,
                                        is_ai: false,
                                        has_attachments,
                                        attachment_count: sent_attachment_count as i32,
                                        attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                        has_reactions: false,
                                        reaction_summary: SharedString::default(),
                                        reply_count: 0,
                                        reply_count_text: SharedString::default(),
                                        is_thread_reply: false,
                                        show_sender: true,
                                        delivery_status: "sent".into(),
                                        is_pinned: false,
                                    };

                                    // Add to message list
                                    let current_model = w.get_messages();
                                    let mut messages: Vec<_> = (0..current_model.row_count())
                                        .filter_map(|i| current_model.row_data(i))
                                        .collect();
                                    messages.push(msg_item);
                                    // Sort by timestamp to ensure correct order
                                    messages.sort_by_key(|m| m.timestamp_ns.parse::<u64>().unwrap_or(0));
                                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                                    w.set_composer_text("".into());
                                    // Auto-scroll to bottom after sending own message
                                    w.set_message_scroll_position(-99999.0);
                                    w.set_is_at_bottom(true);
                                    // Reset typing state so subsequent typing triggers indicator
                                    w.invoke_typing_changed(false);
                                    // Clear attachment tray UI
                                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                    w.set_composer_placeholder("Type a message...".into());

                                    // Update DM sidebar with sent message preview
                                    update_dm_sidebar_preview(&w, &sidebar_peer_id, &sidebar_preview, &sidebar_timestamp);
                                }
                            });
                        }
                        Err(chatter_linux::services::message_service::MessageError::BootstrapInitiated) => {
                            // First DM - bootstrap was initiated, message was queued as pending
                            // No retry needed - it will be sent automatically when bootstrap completes
                            tracing::info!(
                                peer_id = %peer_id,
                                attachment_count = sent_attachment_count,
                                "DM bootstrap initiated - message queued"
                            );

                            // Look up the stored CDN attachments from repo (they have correct IDs and waveforms)
                            let attachment_display_data: Vec<_> = uploaded_ids_for_display
                                .iter()
                                .filter_map(|att_id| {
                                    attachment_repo.get(att_id).ok().flatten()
                                })
                                .map(|att| attachment_to_display_data(&att))
                                .collect();

                            // Add sent message to UI immediately with "queuing" status
                            // Use the SAME sent_at timestamp that was used for the proto
                            // This ensures UI matches what will be stored in DB after bootstrap
                            let sent_text = text.clone();
                            let sender_name = current_user_name.clone();
                            let sender_initials = get_initials(&sender_name);
                            let timestamp = chatter_linux::util::time::TimestampNanos::from_nanos(sent_at)
                                .to_datetime()
                                .unwrap_or_else(chrono::Utc::now);
                            let timestamp_str = timestamp.format("%H:%M").to_string();
                            let message_id = uuid::Uuid::new_v4().to_string();
                            let has_attachments = sent_attachment_count > 0;

                            // Clone data needed for sidebar update
                            let sidebar_peer_id = peer_id.to_string();
                            let sidebar_preview = if sent_text.len() > 50 {
                                format!("You: {}...", &sent_text[..47])
                            } else {
                                format!("You: {}", sent_text)
                            };
                            let sidebar_timestamp = timestamp_str.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Convert attachment display data to Slint types (must be on UI thread)
                                    let slint_attachments: Vec<MessageAttachmentData> = attachment_display_data
                                        .iter()
                                        .map(|a| a.to_slint())
                                        .collect();

                                    // Create message item for queued DM
                                    let msg_item = MessageItemData {
                                        message_id: message_id.into(),
                                        sender_id: SharedString::default(), // Own message
                                        sender_name: sender_name.into(),
                                        sender_initials: sender_initials.into(),
                                        sender_avatar_url: SharedString::default(),
                                        content: sent_text.into(),
                                        timestamp: timestamp_str.into(),
                                        timestamp_ns: sent_at.to_string().into(), // Use same sent_at as proto
                                        is_own: true,
                                        is_edited: false,
                                        is_deleted: false,
                                        is_system: false,
                                        is_ai: false,
                                        has_attachments,
                                        attachment_count: sent_attachment_count as i32,
                                        attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                        has_reactions: false,
                                        reaction_summary: SharedString::default(),
                                        reply_count: 0,
                                        reply_count_text: SharedString::default(),
                                        is_thread_reply: false,
                                        show_sender: true,
                                        delivery_status: "sending".into(), // Will update when bootstrap completes
                                        is_pinned: false,
                                    };

                                    // Add to message list
                                    let current_model = w.get_messages();
                                    let mut messages: Vec<_> = (0..current_model.row_count())
                                        .filter_map(|i| current_model.row_data(i))
                                        .collect();
                                    messages.push(msg_item);
                                    messages.sort_by_key(|m| m.timestamp_ns.parse::<u64>().unwrap_or(0));
                                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                                    w.set_composer_text("".into());
                                    // Auto-scroll to bottom after sending own message
                                    w.set_message_scroll_position(-99999.0);
                                    w.set_is_at_bottom(true);
                                    // Reset typing state so subsequent typing triggers indicator
                                    w.invoke_typing_changed(false);
                                    // Clear attachment tray UI
                                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                    w.set_composer_placeholder(
                                        "Setting up secure channel...".into(),
                                    );

                                    // Update DM sidebar with sent message preview
                                    update_dm_sidebar_preview(&w, &sidebar_peer_id, &sidebar_preview, &sidebar_timestamp);
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
            // Use full pending attachments (we need paths for upload)
            let pending_attachments = pending_attachments_snapshot.clone();

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

                    // Upload attachments with encryption at send time (secure flow)
                    let attachment_service = AttachmentService::new(attachment_repo.clone());
                    let mut attachment_pointers: Vec<chatter_linux::proto::content_v1::AttachmentPointer> = Vec::new();
                    let mut uploaded_attachment_ids: Vec<chatter_linux::domain::ids::AttachmentId> = Vec::new();

                    for att in &pending_attachments {
                        let file_path = PathBuf::from(&att.path);

                        tracing::info!(
                            filename = %att.filename,
                            path = %att.path.display(),
                            size = att.size,
                            mime_type = %att.mime_type,
                            "Encrypting and uploading channel attachment to CDN"
                        );

                        // Upload with encryption to CDN (same as DMs)
                        match attachment_service.upload_to_cdn_encrypted(&grpc_pool, &file_path).await {
                            Ok(result) => {
                                tracing::info!(
                                    cdn_key = %result.cdn_key,
                                    attachment_id = %result.attachment_id,
                                    filename = %result.file_name,
                                    "Channel attachment uploaded to CDN and stored locally"
                                );
                                uploaded_attachment_ids.push(result.attachment_id);
                                attachment_pointers.push(result.to_attachment_pointer());
                            }
                            Err(e) => {
                                tracing::error!(
                                    filename = %att.filename,
                                    error = %e,
                                    "Failed to upload channel attachment to CDN"
                                );
                                // Continue with remaining attachments
                            }
                        }
                    }

                    tracing::debug!(
                        "Sending channel message with {} encrypted attachments",
                        attachment_pointers.len()
                    );

                    if is_saved_messages {
                        // Use Signal-style Note to Self handling (simplified API)
                        match message_service
                            .save_message(&text, uploaded_attachment_ids.clone())
                            .await
                        {
                            Ok(message) => {
                                // Attachments already cleared from state before spawn to prevent double-send

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
                                        // Sort by timestamp to ensure correct order
                                        messages.sort_by_key(|m| m.timestamp_ns.parse::<u64>().unwrap_or(0));
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Auto-scroll to bottom after sending own message
                                        w.set_message_scroll_position(-99999.0);
                                        w.set_is_at_bottom(true);
                                        // Reset typing state so subsequent typing triggers indicator
                                        w.invoke_typing_changed(false);
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
                        // Regular channel message (simplified API)
                        match message_service
                            .send(
                                group_fanout.as_deref(),
                                &channel_id,
                                &text,
                                uploaded_attachment_ids.clone(),
                                None,
                                vec![],
                                false,
                            )
                            .await
                        {
                            Ok(message) => {
                                // Attachments already cleared from state before spawn to prevent double-send

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
                                        // Sort by timestamp to ensure correct order
                                        messages.sort_by_key(|m| m.timestamp_ns.parse::<u64>().unwrap_or(0));
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Auto-scroll to bottom after sending own message
                                        w.set_message_scroll_position(-99999.0);
                                        w.set_is_at_bottom(true);
                                        // Reset typing state so subsequent typing triggers indicator
                                        w.invoke_typing_changed(false);
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

/// Update the DM sidebar preview with a sent message.
fn update_dm_sidebar_preview(
    window: &MainWindow,
    peer_id_str: &str,
    preview: &str,
    timestamp_str: &str,
) {
    let dms = window.get_dm_conversations();

    // Find and update the DM entry for this peer
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            if dm.user_id.as_str() == peer_id_str {
                // Update existing entry with sent message preview
                let mut updated_dm = dm.clone();
                updated_dm.last_message = preview.into();
                updated_dm.timestamp = timestamp_str.into();

                if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                    model.set_row_data(i, updated_dm);
                }
                return;
            }
        }
    }

    // If not found by user_id, the DM might be new - no action needed
    // The sidebar entry should already exist from friend accept/initial load
    tracing::debug!(
        peer_id = %peer_id_str,
        "DM sidebar entry not found for sent message preview update"
    );
}
