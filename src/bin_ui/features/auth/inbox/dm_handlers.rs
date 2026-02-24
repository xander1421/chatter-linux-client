//! DM-specific event handlers.
//!
//! Handles direct message events: received, typing, receipts, reactions, pins, edits, deletes.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use chatter_linux::domain::ids::{AttachmentId, MessageId};
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::proto::content_v1::AttachmentPointer;
use chatter_linux::services::mailbox_manager::MailboxManager;
use chatter_linux::services::message_service::MessageService;
use chatter_linux::storage::{AttachmentRepository, ChannelRepository, Database, MessageRepository, UserRepository};

use crate::bin_ui::converters::{
    attachment_pointer_to_display_data, get_initials,
    message_to_display_data_with_attachments, AttachmentDisplayData, MessageDisplayData,
};
use crate::bin_ui::features::chat::get_dm_channel_from_peer;
use crate::{DMListItemData, MainWindow, MessageItemData};

use super::typing::{clear_typing_for_peer, clear_typing_timer, set_typing_with_timer};
use super::utils::{format_timestamp, update_reaction_summary};

// =============================================================================
// Session Recovery Handlers
// =============================================================================

/// Handle session convergence failure by resetting and re-establishing session.
///
/// When all Sesame sessions fail to decrypt a message, we need to:
/// 1. Archive all existing sessions
/// 2. Fetch fresh prekey bundle and establish new session
/// 3. Future messages should work with the new session
pub(crate) fn handle_convergence_failure(
    peer_id: chatter_linux::domain::UserId,
    message_service: Arc<MessageService>,
    grpc_pool: Arc<GrpcClientPool>,
) {
    // Use spawn_blocking because libsignal stores aren't Send
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime for convergence recovery");

        rt.block_on(async {
            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Reset session and establish fresh one (device_id = 1 for primary)
            match message_service
                .reset_and_establish_session(&mut auth_client, &peer_id, 1)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        peer_id = %peer_id,
                        "Session reset complete - ready for fresh messages"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to reset session after convergence failure"
                    );
                }
            }
        });
    });
}

// =============================================================================
// DM Bootstrap Handler
// =============================================================================

pub(crate) fn handle_dm_bootstrap_completed(
    peer_id: chatter_linux::domain::UserId,
    message_service: Arc<MessageService>,
    database: Arc<Database>,
    mailbox_manager: Arc<MailboxManager>,
    grpc_pool: Arc<GrpcClientPool>,
) {
    tracing::info!(
        peer_id = %peer_id,
        "BOOTSTRAP[4/4] handle_dm_bootstrap_completed called - spawning task to send pending"
    );

    // Use spawn_blocking because libsignal stores aren't Send
    tokio::task::spawn_blocking(move || {
        tracing::debug!(
            peer_id = %peer_id,
            "BOOTSTRAP[4/4] Inside spawn_blocking, creating runtime"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime for bootstrap completion");

        rt.block_on(async {
            tracing::debug!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] Creating auth client for pending message send"
            );

            // Create an auth client for key fetching (needed for Signal encryption)
            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            tracing::info!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] Calling send_pending_messages (initiator side)"
            );

            // Send all pending messages for this peer
            match message_service
                .send_pending_messages(&mut auth_client, &peer_id, database.clone(), &mailbox_manager)
                .await
            {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            peer_id = %peer_id,
                            count = count,
                            "BOOTSTRAP[4/4] SUCCESS - Sent {} pending message(s) after bootstrap!",
                            count
                        );
                    } else {
                        tracing::info!(
                            peer_id = %peer_id,
                            "BOOTSTRAP[4/4] No pending messages to send (initiator side)"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "BOOTSTRAP[4/4] FAILED - Could not send pending messages"
                    );
                }
            }
        });
    });
}

// =============================================================================
// DM Event Handlers
// =============================================================================

/// Auto-download image attachments in background and update UI.
///
/// This enables images to display inline without requiring user to click "View".
/// Downloads encrypted content to database (no filesystem), then decrypts to memory for display.
async fn auto_download_image_attachments(
    pointers: Vec<AttachmentPointer>,
    grpc_pool: Arc<GrpcClientPool>,
    attachment_repo: Arc<AttachmentRepository>,
    weak_window: slint::Weak<MainWindow>,
    peer_id: String,
) {
    use chatter_linux::media::ImageProcessor;
    use chatter_linux::services::AttachmentService;

    let attachment_service = AttachmentService::new(attachment_repo.clone());

    for pointer in pointers {
        let cdn_key = pointer.cdn_key.clone();
        let att_id = AttachmentId::from_cdn_key(&cdn_key);

        tracing::info!(
            cdn_key = %cdn_key,
            filename = %pointer.file_name,
            "Auto-downloading image attachment to database"
        );

        // SECURE: Download encrypted content to database (not filesystem)
        match attachment_service
            .download_to_database(&grpc_pool, &att_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    cdn_key = %cdn_key,
                    "Image attachment downloaded to database"
                );

                // Update attachment status
                if let Ok(Some(mut att)) = attachment_repo.get(&att_id) {
                    att.status = AttachmentStatus::Ready;
                    att.content_location = ContentLocation::InDatabase;
                    if let Err(e) = attachment_repo.update(&att) {
                        tracing::error!(
                            cdn_key = %cdn_key,
                            error = %e,
                            "Failed to update attachment status"
                        );
                    }
                }

                // SECURE: Decrypt to memory for UI display
                match attachment_service.decrypt_to_memory(&att_id) {
                    Ok(secure_buffer) => {
                        // Get dimensions first (slint::Image is not Send)
                        match ImageProcessor::get_dimensions_from_memory(secure_buffer.as_slice()) {
                            Ok((width, height)) => {
                                // Copy bytes for the event loop (secure_buffer zeroizes on drop)
                                let image_bytes = secure_buffer.as_slice().to_vec();
                                let cdn_key_for_ui = cdn_key.clone();
                                let peer_id_clone = peer_id.clone();
                                let weak = weak_window.clone();

                                // Create image inside event loop (slint::Image is not Send)
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak.upgrade() {
                                        match ImageProcessor::memory_to_slint_image(&image_bytes) {
                                            Ok(image) => {
                                                update_attachment_thumbnail_from_image(
                                                    &window,
                                                    &cdn_key_for_ui,
                                                    image,
                                                    width as i32,
                                                    height as i32,
                                                    &peer_id_clone,
                                                );
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    cdn_key = %cdn_key_for_ui,
                                                    error = ?e,
                                                    "Failed to create slint image from memory"
                                                );
                                            }
                                        }
                                    }
                                });
                                // secure_buffer dropped here -> memory zeroized
                            }
                            Err(e) => {
                                tracing::error!(
                                    cdn_key = %cdn_key,
                                    error = ?e,
                                    "Failed to get image dimensions"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            cdn_key = %cdn_key,
                            error = %e,
                            "Failed to decrypt attachment to memory"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to download image attachment to database"
                );
            }
        }
    }
}

/// Update a message's attachment thumbnail from a pre-loaded image (memory-safe).
fn update_attachment_thumbnail_from_image(
    window: &MainWindow,
    cdn_key: &str,
    image: slint::Image,
    width: i32,
    height: i32,
    peer_id: &str,
) {
    let selected_dm = window.get_selected_dm_id().to_string();
    let peer_channel_id = get_dm_channel_from_peer(window, peer_id);
    let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
        || selected_dm == peer_id;

    if !is_active {
        tracing::debug!(
            cdn_key = %cdn_key,
            "Skipping thumbnail update - not active conversation"
        );
        return;
    }

    // Find and update the message with this attachment
    let messages = window.get_messages();
    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            // Check if this message has the attachment we downloaded
            let attachments = msg.attachments.clone();
            let mut found = false;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    // Match by cdn_key (stored in id field)
                    let att_id = AttachmentId::from_cdn_key(cdn_key);
                    if att.id.as_str() == att_id.to_string() || att.id.as_str() == cdn_key {
                        found = true;

                        // Update the attachment with thumbnail
                        let mut updated_att = att.clone();
                        updated_att.has_thumbnail = true;
                        updated_att.thumbnail = image.clone();
                        updated_att.width = width;
                        updated_att.height = height;

                        if let Some(model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            model.set_row_data(att_idx, updated_att);
                            tracing::debug!(
                                cdn_key = %cdn_key,
                                msg_idx,
                                att_idx,
                                "Updated attachment thumbnail in UI"
                            );
                        }
                        break;
                    }
                }
            }

            if found {
                break;
            }
        }
    }
}

pub(crate) fn handle_dm_received(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    attachment_repo: &Arc<AttachmentRepository>,
    grpc_pool: &Arc<GrpcClientPool>,
    message_repo: &Arc<MessageRepository>,
    peer_id: chatter_linux::domain::UserId,
    text: String,
    timestamp: u64,
    message_id: String,
    attachments: Vec<AttachmentPointer>,
) {
    tracing::info!("DM received from {}: {} chars, {} attachments", peer_id, text.len(), attachments.len());

    // Collect image attachments for auto-download (thumbnails)
    // Note: Audio waveforms are now included in AttachmentPointer from sender
    let mut image_pointers: Vec<AttachmentPointer> = Vec::new();

    // Store attachment metadata in database for later download
    for pointer in &attachments {
        if pointer.cdn_key.is_empty() {
            continue;
        }

        // Generate deterministic UUID from cdn_key for storage
        let att_id = AttachmentId::from_cdn_key(&pointer.cdn_key);

        let metadata = FileMetadata {
            name: pointer.file_name.clone(),
            mime_type: pointer.content_type.clone(),
            size: pointer.plaintext_size,
            sha256: vec![], // Will be computed after download
            chunk_size: 0,
            width: if pointer.width > 0 { Some(pointer.width) } else { None },
            height: if pointer.height > 0 { Some(pointer.height) } else { None },
            duration_seconds: if pointer.duration_ms > 0 { Some(pointer.duration_ms as f64 / 1000.0) } else { None },
            waveform_samples: if !pointer.waveform.is_empty() { Some(pointer.waveform.clone()) } else { None },
            thumbnail: if !pointer.thumbnail.is_empty() { Some(pointer.thumbnail.clone()) } else { None },
            thumbnail_width: if pointer.thumbnail_width > 0 { Some(pointer.thumbnail_width) } else { None },
            thumbnail_height: if pointer.thumbnail_height > 0 { Some(pointer.thumbnail_height) } else { None },
        };

        let attachment = Attachment {
            id: att_id,
            metadata,
            status: AttachmentStatus::Pending, // Pending download
            content_location: ContentLocation::NotDownloaded,
            local_path: None,
            remote_url: Some(pointer.cdn_key.clone()), // Store cdn_key here
            encryption_key: if pointer.encryption_key.is_empty() {
                None
            } else {
                Some(pointer.encryption_key.clone())
            },
            encryption_iv: if pointer.digest.is_empty() {
                None
            } else {
                Some(pointer.digest.clone()) // Store digest in iv field
            },
            created_at: chrono::Utc::now(),
        };

        if let Err(e) = attachment_repo.insert(&attachment) {
            tracing::error!(
                cdn_key = %pointer.cdn_key,
                error = %e,
                "Failed to store DM attachment metadata"
            );
        } else {
            tracing::debug!(
                id = %att_id,
                cdn_key = %pointer.cdn_key,
                filename = %pointer.file_name,
                "Stored DM attachment metadata for later download"
            );

            // Queue image attachments for auto-download ONLY if sender didn't provide thumbnail
            // Sender-provided thumbnails are instant (no download/decrypt needed)
            if pointer.content_type.starts_with("image/")
                && !pointer.encryption_key.is_empty()
                && pointer.thumbnail.is_empty()
            {
                image_pointers.push(pointer.clone());
            }
        }
    }

    // Auto-download image attachments in background
    if !image_pointers.is_empty() {
        let grpc_pool = grpc_pool.clone();
        let attachment_repo = attachment_repo.clone();
        let weak_for_download = weak_window.clone();
        let peer_id_for_download = peer_id.to_string();

        tokio::spawn(async move {
            auto_download_image_attachments(
                image_pointers,
                grpc_pool,
                attachment_repo,
                weak_for_download,
                peer_id_for_download,
            ).await;
        });
    }

    // Note: Audio waveform is now included in AttachmentPointer (sender-side generated)
    // No need to auto-download audio just for waveform display

    // Try to get display name from profile, fall back to user
    let sender_name_from_repo = user_repo
        .get_profile(&peer_id)
        .ok()
        .flatten()
        .map(|p| p.display_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            user_repo
                .get(&peer_id)
                .ok()
                .flatten()
                .map(|u| u.username.clone())
                .filter(|name| !name.is_empty())
        });

    let peer_id_str = peer_id.to_string();

    // Build lookup map from attachment pointers for fallback display
    // Key: AttachmentId derived from cdn_key, Value: display data from pointer
    let pointer_display_map: std::collections::HashMap<AttachmentId, AttachmentDisplayData> = attachments
        .iter()
        .filter(|p| !p.cdn_key.is_empty())
        .map(|p| (AttachmentId::from_cdn_key(&p.cdn_key), attachment_pointer_to_display_data(p)))
        .collect();

    // Fetch messages from DB (already sorted by sent_at)
    // Look up the message by ID to get its channel_id, then fetch all messages for that channel
    let display_messages: Option<Vec<MessageDisplayData>> = message_id
        .parse::<MessageId>()
        .ok()
        .and_then(|msg_id| message_repo.get(&msg_id).ok().flatten())
        .and_then(|msg| {
            let channel_id = msg.channel_id;
            // Fetch messages for this channel (DB returns sorted by sent_at DESC)
            message_repo.list_for_channel(&channel_id, None, 100).ok()
        })
        .map(|messages| {
            // Convert to display format
            let mut display: Vec<MessageDisplayData> = messages
                .iter()
                .map(|msg| {
                    let sender_name = if let Some(sender_id) = &msg.sender_id {
                        user_repo
                            .get_profile(sender_id)
                            .ok()
                            .flatten()
                            .map(|p| p.display_name)
                            .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                            .unwrap_or_else(|| sender_id.to_string())
                    } else {
                        "Unknown".to_string()
                    };

                    // Resolve attachments: try DB first, fall back to pointer data
                    let resolved_attachments: Vec<Attachment> = msg
                        .attachments
                        .iter()
                        .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                        .collect();

                    // Get display data, using pointer fallback for missing attachments
                    let attachment_display: Vec<AttachmentDisplayData> = msg
                        .attachments
                        .iter()
                        .map(|att_id| {
                            // First try to find in resolved DB attachments
                            if let Some(att) = resolved_attachments.iter().find(|a| &a.id == att_id) {
                                crate::bin_ui::converters::attachment_to_display_data(att)
                            } else if let Some(pointer_data) = pointer_display_map.get(att_id) {
                                // Fall back to pointer data for immediate display
                                pointer_data.clone()
                            } else {
                                // Shouldn't happen, but create placeholder
                                AttachmentDisplayData {
                                    id: att_id.to_string(),
                                    filename: "Unknown".to_string(),
                                    size_display: "".to_string(),
                                    mime_type: "application/octet-stream".to_string(),
                                    category: "file".to_string(),
                                    thumbnail_url: String::new(),
                                    download_url: String::new(),
                                    local_path: None,
                                    is_image: false,
                                    is_video: false,
                                    width: 0,
                                    height: 0,
                                    is_playing: false,
                                    icon: "📎".to_string(),
                                    icon_color: slint::Color::from_argb_u8(255, 107, 114, 128),
                                    waveform: vec![10; 32],
                                    duration_display: String::new(),
                                    is_uploading: false,
                                    is_downloading: false,
                                    progress: 0.0,
                                    thumbnail_bytes: Vec::new(),
                                }
                            }
                        })
                        .collect();

                    message_to_display_data_with_attachments(msg, &sender_name, attachment_display)
                })
                .collect();
            // Reverse to show oldest first (DB returns newest first)
            display.reverse();
            display
        });

    // Build preview and timestamp for sidebar
    let timestamp_str = format_timestamp(timestamp);
    let preview = if text.is_empty() && !attachments.is_empty() {
        format!("📎 {} attachment{}", attachments.len(), if attachments.len() == 1 { "" } else { "s" })
    } else if text.len() > 50 {
        format!("{}...", &text[..47])
    } else {
        text.clone()
    };

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Get sender name from DM conversations if not in repo
        let sender_name = sender_name_from_repo.unwrap_or_else(|| {
            let conversations = window.get_dm_conversations();
            for i in 0..conversations.row_count() {
                if let Some(item) = conversations.row_data(i) {
                    if item.user_id.as_str() == peer_id_str {
                        let name = item.display_name.to_string();
                        if !name.is_empty() {
                            return name;
                        }
                    }
                }
            }
            peer_id_str.clone()
        });

        let selected_dm = window.get_selected_dm_id().to_string();

        // Check if this is the active conversation
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;

        // Clear typing timer (Signal-style)
        clear_typing_for_peer(&window, &peer_id_str);

        if is_active {
            if let Some(display_messages) = display_messages {
                // Check if user is at bottom before updating
                let was_at_bottom = window.get_is_at_bottom();

                // Convert to Slint types and set (messages already sorted by DB)
                let slint_messages: Vec<MessageItemData> = display_messages
                    .iter()
                    .map(|m| m.to_slint())
                    .collect();
                window.set_messages(ModelRc::new(VecModel::from(slint_messages)));

                // Auto-scroll to bottom if user was at bottom before new message
                if was_at_bottom {
                    // Setting scroll position to large negative scrolls to bottom
                    // The actual value will be clamped by the Flickable
                    window.set_message_scroll_position(-99999.0);
                    window.set_is_at_bottom(true);
                }
            }
        }

        // Always update sidebar
        update_dm_sidebar(&window, &peer_id_str, &sender_name, &preview, &timestamp_str);
    });
}

fn update_dm_sidebar(
    window: &MainWindow,
    peer_id_str: &str,
    sender_name: &str,
    preview: &str,
    timestamp_str: &str,
) {
    let dms = window.get_dm_conversations();

    // Check if this peer already exists in the sidebar
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            if dm.user_id.as_str() == peer_id_str {
                // Update existing entry
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

    // Peer not found - add new DM entry to sidebar
    tracing::info!(
        peer_id = %peer_id_str,
        sender_name = %sender_name,
        "Adding new DM conversation to sidebar for incoming message"
    );

    let initials = get_initials(sender_name);

    let new_dm = DMListItemData {
        channel_id: "".into(), // Will be populated when channel is synced
        user_id: peer_id_str.into(),
        display_name: sender_name.into(),
        avatar_url: "".into(),
        initials: initials.into(),
        presence_status: "offline".into(),
        last_message: preview.into(),
        timestamp: timestamp_str.into(),
        unread_count: 1, // New message is unread
    };

    // Add to conversations list
    let mut dm_vec: Vec<DMListItemData> = Vec::new();
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            dm_vec.push(dm);
        }
    }
    dm_vec.push(new_dm);

    window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(dm_vec)));
}

pub(crate) fn handle_dm_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    peer_id: chatter_linux::domain::UserId,
    unread_count: u32,
) {
    tracing::debug!(
        "DM unread count updated: {} (peer {}) = {}",
        channel_id,
        peer_id,
        unread_count
    );

    if let Some(window) = weak_window.upgrade() {
        let peer_id_str = peer_id.to_string();
        let dms = window.get_dm_conversations();

        for i in 0..dms.row_count() {
            if let Some(dm) = dms.row_data(i) {
                if dm.user_id.as_str() == peer_id_str {
                    let mut updated_dm = dm.clone();
                    updated_dm.unread_count = unread_count as i32;

                    if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                        model.set_row_data(i, updated_dm);
                    }
                    break;
                }
            }
        }
    }
}

pub(crate) fn handle_dm_typing(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::info!(
        peer_id = %peer_id,
        action,
        action_name = if action == 1 { "STARTED" } else { "STOPPED" },
        "TYPING_INDICATOR: DM typing event received"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    tracing::info!(
        peer_id = %peer_id_str,
        action,
        "TYPING_INDICATOR: Scheduling invoke_from_event_loop"
    );

    let result = slint::invoke_from_event_loop(move || {
        tracing::info!(
            peer_id = %peer_id_str,
            action,
            "TYPING_INDICATOR: invoke_from_event_loop callback RUNNING"
        );

        let Some(window) = weak.upgrade() else {
            tracing::warn!("TYPING_INDICATOR: Window upgrade failed!");
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);

        // Check condition 1: peer's channel_id matches selected_dm_id
        let cond1_match = peer_channel_id.as_deref() == Some(selected_dm.as_str());
        // Check condition 2: peer's user_id matches selected_dm_id directly
        let cond2_match = selected_dm == peer_id_str;

        tracing::info!(
            peer_id = %peer_id_str,
            selected_dm = %selected_dm,
            peer_channel_id = ?peer_channel_id,
            cond1_channel_match = cond1_match,
            cond2_userid_match = cond2_match,
            "TYPING_INDICATOR: Channel lookup and comparison"
        );

        // Check if this is the active conversation
        // Note: selected_dm_id can be either channel_id (from sidebar) or user_id (from friends list)
        // So we check both: peer's channel matches selected, OR peer_id matches selected directly
        let is_active_conversation = cond1_match || cond2_match;

        tracing::info!(
            peer_id = %peer_id_str,
            is_active_conversation,
            "TYPING_INDICATOR: Active conversation check"
        );

        if !is_active_conversation {
            tracing::warn!(
                peer_id = %peer_id_str,
                selected_dm = %selected_dm,
                peer_channel_id = ?peer_channel_id,
                "TYPING_INDICATOR: NOT active conversation - skipping UI update"
            );
            // Not the active conversation, but still manage timer for when user switches
            if action != 1 {
                clear_typing_timer(&peer_id_str);
            }
            return;
        }

        if action == 1 {
            // STARTED: Show indicator and start 15s auto-clear timer (Signal-style)
            // For DMs, just show "typing..." since the header already shows who we're chatting with
            tracing::info!(
                peer_id = %peer_id_str,
                "TYPING_INDICATOR: Calling set_typing_with_timer"
            );
            set_typing_with_timer(&window, &peer_id_str);
        } else {
            // STOPPED: Clear indicator and cancel timer
            tracing::info!(
                peer_id = %peer_id_str,
                "TYPING_INDICATOR: Clearing typing indicator (STOPPED)"
            );
            clear_typing_for_peer(&window, &peer_id_str);
        }
    });

    if result.is_err() {
        tracing::error!("TYPING_INDICATOR: invoke_from_event_loop FAILED to schedule!");
    }
}

pub(crate) fn handle_dm_receipt(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::debug!(
        peer_id = %peer_id,
        receipt_type,
        count = timestamps.len(),
        "Received read/delivery receipt"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            return;
        }

        let messages = window.get_messages();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if !msg.is_own {
                    continue;
                }

                // Compare nanosecond timestamps as strings
                let msg_ts_ns = msg.timestamp_ns.to_string();
                let matching = timestamps.iter().any(|&ts| ts.to_string() == msg_ts_ns);
                if matching {
                    let mut updated_msg = msg.clone();
                    updated_msg.delivery_status = status.into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                }
            }
        }
    });
}

pub(crate) fn handle_dm_reaction(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received reaction"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    let new_summary =
                        update_reaction_summary(&msg.reaction_summary.to_string(), &emoji, remove);
                    updated_msg.reaction_summary = new_summary.into();
                    updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_dm_pin(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        unpin,
        "Received pin/unpin"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            return;
        }

        let messages = window.get_messages();
        let target_ts_ns = target_sent_timestamp.to_string();
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    preview = msg.content.to_string();
                    if preview.len() > 100 {
                        preview = format!("{}...", &preview[..97]);
                    }
                    break;
                }
            }
        }

        let current_count = window.get_pinned_total_count();
        if unpin {
            let new_count = (current_count - 1).max(0);
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(new_count > 0);
            if new_count == 0 {
                window.set_pinned_message_preview("".into());
            }
        } else {
            let new_count = current_count + 1;
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(true);
            if !preview.is_empty() {
                window.set_pinned_message_preview(preview.into());
                window.set_pinned_current_index(new_count);
            }
        }
    });
}

pub(crate) fn handle_dm_edit(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received message edit"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            return;
        }

        let messages = window.get_messages();
        // Convert nanoseconds to milliseconds for matching
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_body.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_dm_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != conversation_id {
            return;
        }

        let messages = window.get_messages();
        // Convert nanoseconds to milliseconds for matching
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "Message deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

pub(crate) fn handle_dm_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        "Received delete-for-everyone from peer"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;
        if !is_active {
            return;
        }

        let messages = window.get_messages();
        // Convert nanoseconds to milliseconds for matching
        let target_ts_ns = target_sent_timestamp.to_string();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_ns {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "This message was deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

// =============================================================================
// Friend Event Handlers
// =============================================================================

/// Handle FriendAccepted event by adding the DM channel to the sidebar.
/// This is called on the SENDER's side when their friend request is accepted.
///
/// DM bootstrap happens on-demand when the user sends their first message,
/// avoiding race conditions where both sides try to initiate simultaneously.
pub(crate) async fn handle_friend_accepted(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    channel_repo: &Arc<ChannelRepository>,
    grpc_pool: &Arc<GrpcClientPool>,
    friend_user_id: String,
    channel_id: String,
) {
    use chatter_linux::network::grpc::channel_client::ChannelClient;
    use chatter_linux::network::grpc::UserClient;

    // Validate the channel ID format
    if let Err(e) = channel_id.parse::<chatter_linux::domain::ids::ChannelId>() {
        tracing::error!(error = %e, channel_id = %channel_id, "Invalid channel_id in FriendAccepted event");
        return;
    }

    // Parse the friend user ID
    let friend_uuid = match friend_user_id.parse::<chatter_linux::domain::UserId>() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, friend_user_id = %friend_user_id, "Invalid friend_user_id in FriendAccepted event");
            return;
        }
    };

    // Fetch channel from server
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    match channel_client.get_channel(&channel_id).await {
        Ok(proto_channel) => {
            // Convert proto to domain
            let channel = match chatter_linux::services::channel::converters::proto_to_domain_channel(&proto_channel) {
                Ok(ch) => ch,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to convert proto channel");
                    return;
                }
            };

            // Save to local repo
            if let Err(e) = channel_repo.insert(&channel) {
                tracing::error!(error = %e, "Failed to save DM channel to local repo");
                // Continue anyway to update UI
            }

            // Sync memberships from the proto response
            for proto_member in &proto_channel.members {
                match chatter_linux::services::channel::converters::proto_to_domain_membership(&channel.id, proto_member) {
                    Ok(membership) => {
                        if let Err(e) = channel_repo.insert_or_ignore_membership(&membership) {
                            tracing::warn!(
                                channel_id = %channel.id,
                                user_id = %membership.user_id,
                                error = %e,
                                "Failed to sync DM membership"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel.id,
                            error = %e,
                            "Failed to convert proto DM membership"
                        );
                    }
                }
            }

            // Get friend's display name - try local repo first, then fetch from server
            let friend_name = match user_repo.get_profile(&friend_uuid).ok().flatten() {
                Some(profile) => {
                    if profile.display_name.is_empty() {
                        friend_user_id.clone()
                    } else {
                        profile.display_name.clone()
                    }
                }
                None => {
                    // User not in local repo, fetch from server and save
                    let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
                    match user_client.get_user_profile(&friend_user_id).await {
                        Ok(server_profile) => {
                            let display_name = if !server_profile.display_name.is_empty() {
                                server_profile.display_name.clone()
                            } else {
                                friend_user_id.clone()
                            };

                            // First create user record (required for profile foreign key)
                            // Username not in profile proto, use display_name or user_id
                            let username = if !server_profile.display_name.is_empty() {
                                server_profile.display_name.clone()
                            } else {
                                friend_user_id.clone()
                            };
                            let user = chatter_linux::domain::user::User::new(
                                friend_uuid,
                                username,
                                vec![], // Public key not available from profile endpoint
                            );
                            if let Err(e) = user_repo.insert(&user) {
                                // Ignore duplicate key errors (user may already exist)
                                if !e.to_string().contains("UNIQUE constraint") {
                                    tracing::warn!(error = %e, "Failed to save friend user to local repo");
                                }
                            }

                            // Now save profile to local repo for future lookups
                            let local_profile = chatter_linux::domain::user::Profile {
                                user_id: friend_uuid,
                                display_name: display_name.clone(),
                                avatar_url: Some(server_profile.avatar_url),
                                status_message: None,
                                presence: chatter_linux::domain::user::PresenceStatus::Offline,
                            };
                            if let Err(e) = user_repo.upsert_profile(&local_profile) {
                                tracing::warn!(error = %e, "Failed to save friend profile to local repo");
                            } else {
                                tracing::debug!(
                                    user_id = %friend_user_id,
                                    display_name = %display_name,
                                    "Saved friend profile to local repo"
                                );
                            }

                            display_name
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to fetch friend profile, using ID");
                            friend_user_id.clone()
                        }
                    }
                }
            };

            let initials = get_initials(&friend_name);

            // Add DM to sidebar UI
            let weak = weak_window.clone();
            let channel_id_clone = channel_id.clone();
            let friend_user_id_clone = friend_user_id.clone();
            let dm_item = DMListItemData {
                channel_id: channel_id.clone().into(),
                user_id: friend_user_id.clone().into(),
                display_name: friend_name.into(),
                initials: initials.into(),
                avatar_url: "".into(),
                presence_status: "offline".into(),
                last_message: "".into(),
                timestamp: "".into(),
                unread_count: 0,
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let current_dms = window.get_dm_conversations();
                    let mut dms: Vec<DMListItemData> = (0..current_dms.row_count())
                        .filter_map(|i| current_dms.row_data(i))
                        .collect();

                    // Check if DM already exists
                    let exists = dms.iter().any(|dm| dm.channel_id.as_str() == channel_id_clone);
                    if !exists {
                        dms.push(dm_item);
                        window.set_dm_conversations(ModelRc::new(VecModel::from(dms)));
                        tracing::info!(
                            channel_id = %channel_id_clone,
                            friend_user_id = %friend_user_id_clone,
                            "Added DM to sidebar for accepted friend"
                        );
                    }
                }
            });
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                channel_id = %channel_id,
                "Failed to fetch DM channel from server"
            );

            // Even if we can't fetch the channel, try to add a basic DM entry
            let friend_name = user_repo
                .get(&friend_uuid)
                .ok()
                .flatten()
                .map(|u| u.username.clone())
                .unwrap_or_else(|| friend_user_id.clone());

            let initials = get_initials(&friend_name);

            let weak = weak_window.clone();
            let channel_id_clone = channel_id.clone();
            let friend_user_id_clone = friend_user_id.clone();
            let dm_item = DMListItemData {
                channel_id: channel_id.clone().into(),
                user_id: friend_user_id.clone().into(),
                display_name: friend_name.into(),
                initials: initials.into(),
                avatar_url: "".into(),
                presence_status: "offline".into(),
                last_message: "".into(),
                timestamp: "".into(),
                unread_count: 0,
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let current_dms = window.get_dm_conversations();
                    let mut dms: Vec<DMListItemData> = (0..current_dms.row_count())
                        .filter_map(|i| current_dms.row_data(i))
                        .collect();

                    let exists = dms.iter().any(|dm| dm.channel_id.as_str() == channel_id_clone);
                    if !exists {
                        dms.push(dm_item);
                        window.set_dm_conversations(ModelRc::new(VecModel::from(dms)));
                        tracing::info!(
                            channel_id = %channel_id_clone,
                            friend_user_id = %friend_user_id_clone,
                            "Added basic DM entry to sidebar (channel fetch failed)"
                        );
                    }
                }
            });
        }
    }
}
