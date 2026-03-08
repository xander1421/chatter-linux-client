//! Channel-specific event handlers.
//!
//! Handles channel message events: received, typing, receipts, reactions, pins, edits, deletes.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::attachment::Attachment;
use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::proto::content_v1::AttachmentPointer;

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::{ChannelListItemData, MainWindow, MessageItemData};

use super::utils::update_reaction_summary;

// =============================================================================
// Auto-Download Attachments
// =============================================================================

/// Auto-download all attachments in background for channel messages.
///
/// Downloads encrypted content to filesystem as soon as pointer arrives.
/// Attachment records are created by the service layer (AttachmentService.create_from_pointers).
/// For images, generates thumbnail. For GIFs, registers for inline animation.
async fn auto_download_channel_attachments(
    pointers: Vec<AttachmentPointer>,
    ui_ctx: Arc<UIContext>,
    weak_window: slint::Weak<MainWindow>,
    channel_id: String,
) {
    use chatter_linux::media::ImageProcessor;

    let attachment_service = ui_ctx.attachment_service.clone();
    let grpc_pool = ui_ctx.grpc_pool.clone();

    for pointer in pointers {
        let cdn_key = pointer.cdn_key.clone();
        let att_id = AttachmentId::from_cdn_key(&cdn_key);
        let is_image = pointer.content_type.starts_with("image/");
        let is_gif = pointer.content_type == "image/gif"
            || pointer.file_name.to_lowercase().ends_with(".gif");

        tracing::info!(
            cdn_key = %cdn_key,
            filename = %pointer.file_name,
            content_type = %pointer.content_type,
            channel_id = %channel_id,
            "Auto-downloading channel attachment to filesystem"
        );

        // Verify attachment record exists (created by service layer)
        match attachment_service.get_metadata(&att_id) {
            Ok(Some(_)) => {
                tracing::debug!(
                    attachment_id = %att_id,
                    "Attachment record exists, proceeding with download"
                );
            }
            Ok(None) => {
                tracing::error!(
                    attachment_id = %att_id,
                    cdn_key = %cdn_key,
                    "BUG: Attachment not found in DB - service layer should have created it"
                );
                continue;
            }
            Err(e) => {
                tracing::error!(
                    attachment_id = %att_id,
                    error = %e,
                    "Error checking attachment in DB"
                );
                continue;
            }
        }

        // Download encrypted content to filesystem
        match attachment_service
            .download_to_filesystem(&grpc_pool, &att_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    cdn_key = %cdn_key,
                    "Channel attachment downloaded to filesystem"
                );

                // For GIFs, use progressive streaming decoding (GPU when available)
                if is_gif {
                    match attachment_service.decrypt_auto(&att_id) {
                        Ok(secure_buffer) => {
                            let data = secure_buffer.as_slice().to_vec();
                            let att_id_str = att_id.to_string();
                            let weak = weak_window.clone();
                            let channel_id_clone = channel_id.clone();

                            // Progressive GIF loading with GPU acceleration
                            tokio::task::spawn_blocking(move || {
                                use chatter_linux::media::gif::{decode_first_frame_only, decode_gif_auto, is_gpu_acceleration_available};

                                let start = std::time::Instant::now();
                                let gpu_available = is_gpu_acceleration_available();

                                // Step 1: Decode first frame instantly (always fast)
                                let first_frame = match decode_first_frame_only(&data) {
                                    Ok(Some(f)) => f,
                                    Ok(None) => return,
                                    Err(e) => {
                                        tracing::error!("Channel GIF first frame error {}: {:?}", att_id_str, e);
                                        return;
                                    }
                                };

                                tracing::info!("Channel GIF first frame in {:?} (GPU: {}): {}", start.elapsed(), gpu_available, att_id_str);

                                // Step 2: Show first frame immediately
                                let first_rgba = first_frame.data.clone();
                                let first_width = first_frame.width;
                                let first_height = first_frame.height;
                                let att_id_first = att_id_str.clone();
                                let weak_first = weak.clone();
                                let channel_id_first = channel_id_clone.clone();

                                let _ = slint::invoke_from_event_loop(move || {
                                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &first_rgba,
                                        first_width,
                                        first_height,
                                    );
                                    let first_image = slint::Image::from_rgba8(buffer);

                                    if let Some(window) = weak_first.upgrade() {
                                        update_channel_attachment_thumbnail(
                                            &window,
                                            &att_id_first,
                                            first_image,
                                            &channel_id_first,
                                        );
                                        tracing::info!("Channel GIF first frame displayed: {}", att_id_first);
                                    }
                                });

                                // Step 3: Decode remaining frames with streaming (GPU accelerated)
                                let mut decoder = match decode_gif_auto(data) {
                                    Ok(d) => d,
                                    Err(e) => {
                                        tracing::warn!("Channel GIF streaming decode error {}: {:?}", att_id_str, e);
                                        return;
                                    }
                                };

                                // Progressive frame streaming
                                let mut animation_started = false;
                                const BATCH_SIZE: usize = 5;

                                loop {
                                    // Poll for newly decoded frames
                                    let new_frames = decoder.poll_frames();
                                    let frames = decoder.frames();
                                    let is_complete = decoder.is_complete();

                                    // Start animation early when we have 2+ frames
                                    if !animation_started && frames.len() >= 2 {
                                        animation_started = true;
                                        // Create Slint pixel buffers in background thread (Send)
                                        let buffers_batch: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = frames
                                            .iter()
                                            .map(|f| {
                                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                                    &f.data,
                                                    f.width,
                                                    f.height,
                                                )
                                            })
                                            .collect();
                                        let delays_batch: Vec<u64> = frames
                                            .iter()
                                            .map(|f| f.delay.as_millis() as u64)
                                            .collect();
                                        let att_id_batch = att_id_str.clone();
                                        let _ = slint::invoke_from_event_loop(move || {
                                            crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                                        });
                                    } else if animation_started && new_frames > 0 && frames.len() % BATCH_SIZE == 0 {
                                        // Update with new batch of frames
                                        let buffers_batch: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = frames
                                            .iter()
                                            .map(|f| {
                                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                                    &f.data,
                                                    f.width,
                                                    f.height,
                                                )
                                            })
                                            .collect();
                                        let delays_batch: Vec<u64> = frames
                                            .iter()
                                            .map(|f| f.delay.as_millis() as u64)
                                            .collect();
                                        let att_id_batch = att_id_str.clone();
                                        let _ = slint::invoke_from_event_loop(move || {
                                            crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                                        });
                                    }

                                    if is_complete {
                                        break;
                                    }

                                    // Small sleep to avoid busy-waiting
                                    std::thread::sleep(std::time::Duration::from_millis(10));
                                }

                                // Final update with all frames
                                let frames = decoder.frames();
                                if frames.len() > 1 {
                                    // Create ALL Slint pixel buffers in background thread (Send)
                                    let all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = frames
                                        .iter()
                                        .map(|f| {
                                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                                &f.data,
                                                f.width,
                                                f.height,
                                            )
                                        })
                                        .collect();
                                    let all_delays: Vec<u64> = frames
                                        .iter()
                                        .map(|f| f.delay.as_millis() as u64)
                                        .collect();
                                    let att_id_full = att_id_str.clone();
                                    let frame_count = all_buffers.len();
                                    let decode_time = start.elapsed();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_full, all_buffers, all_delays);
                                    });
                                    tracing::info!(
                                        "Channel GIF streaming complete: {} frames in {:?} (GPU: {})",
                                        frame_count,
                                        decode_time,
                                        gpu_available
                                    );
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!(
                                cdn_key = %cdn_key,
                                error = %e,
                                "Failed to decrypt channel GIF for animation"
                            );
                        }
                    }
                }
                // For non-GIF images without thumbnails, generate thumbnail
                else if is_image && pointer.thumbnail.is_empty() {
                    match attachment_service.decrypt_auto(&att_id) {
                        Ok(secure_buffer) => {
                            match ImageProcessor::get_dimensions_from_memory(secure_buffer.as_slice()) {
                                Ok((_width, _height)) => {
                                    let image_bytes = secure_buffer.as_slice().to_vec();
                                    let cdn_key_for_ui = cdn_key.clone();
                                    let channel_id_clone = channel_id.clone();
                                    let weak = weak_window.clone();

                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(window) = weak.upgrade() {
                                            match ImageProcessor::memory_to_slint_image(&image_bytes) {
                                                Ok(image) => {
                                                    update_channel_attachment_thumbnail(
                                                        &window,
                                                        &cdn_key_for_ui,
                                                        image,
                                                        &channel_id_clone,
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
                                "Failed to decrypt attachment for thumbnail"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to download channel attachment"
                );
            }
        }
    }
}

/// Update attachment thumbnail in channel messages.
fn update_channel_attachment_thumbnail(
    window: &MainWindow,
    attachment_id: &str,
    image: slint::Image,
    channel_id: &str,
) {
    // Check if this is the active channel
    let selected_channel = window.get_selected_channel_id().to_string();
    if selected_channel != channel_id {
        return;
    }

    let messages = window.get_messages();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = &msg.attachments;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == attachment_id {
                        let mut updated_att = att.clone();
                        updated_att.thumbnail = image;
                        updated_att.has_thumbnail = true;

                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            att_model.set_row_data(att_idx, updated_att);
                            tracing::debug!("Updated channel attachment thumbnail: {}", attachment_id);
                        }
                        return;
                    }
                }
            }
        }
    }
}

// =============================================================================
// Channel Event Handlers
// =============================================================================

pub(crate) fn handle_channel_message_received(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    text: String,
) {
    tracing::info!(
        channel_id = %channel_id,
        text_len = text.len(),
        "UI: Channel message received, fetching from DB"
    );

    // Fetch messages from DB (already sorted by sent_at DESC)
    let display_messages: Option<Vec<MessageDisplayData>> = ui_ctx.message_service
        .get_messages_local(&channel_id, None, 100)
        .ok()
        .map(|messages| {
            let mut display: Vec<MessageDisplayData> = messages
                .iter()
                .map(|msg| {
                    let sender_name = if let Some(sender_id) = &msg.sender_id {
                        // Use profile's display_name, fall back to UUID
                        ui_ctx.user_service
                            .get_profile_local(sender_id)
                            .ok()
                            .flatten()
                            .map(|p| p.display_name)
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| sender_id.to_string())
                    } else {
                        "Unknown".to_string()
                    };
                    // Resolve attachments from DB
                    let resolved_attachments: Vec<Attachment> = msg
                        .attachments
                        .iter()
                        .filter_map(|att_id| ui_ctx.attachment_service.get_metadata(att_id).ok().flatten())
                        .collect();

                    if !msg.attachments.is_empty() {
                        tracing::info!(
                            message_id = %msg.id,
                            stored_attachment_ids = ?msg.attachments.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                            resolved_count = resolved_attachments.len(),
                            "Resolving attachments for message"
                        );
                    }

                    message_to_display_data(msg, &sender_name, &resolved_attachments)
                })
                .collect();
            // Reverse to show oldest first (DB returns newest first)
            display.reverse();
            display
        });

    let weak = weak_window.clone();
    let ui_ctx_cloned = ui_ctx.clone();
    let channel_id_str = channel_id.to_string();
    // Get current user ID for is_own calculation
    let current_user_id = ui_ctx.user_service.current_user_id().ok().map(|id| id.to_string()).unwrap_or_default();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();

        if selected_channel == channel_id_str {
            if let Some(display_messages) = display_messages {
                // Check if user is at bottom before updating
                let was_at_bottom = window.get_is_at_bottom();

                let slint_messages: Vec<MessageItemData> = display_messages
                    .iter()
                    .map(|m| m.to_slint_with_user(&current_user_id))
                    .collect();
                let count = slint_messages.len();
                window.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                tracing::debug!("Updated channel view with {} messages from DB", count);

                // Auto-register GIFs for animation
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_cloned.attachment_service.clone(),
                );

                // Auto-scroll to bottom if user was at bottom before new message
                if was_at_bottom {
                    window.set_message_scroll_position(-99999.0);
                    window.set_is_at_bottom(true);
                }
            }
        }
    });
}

pub(crate) fn handle_channel_typing(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    sender_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::debug!(
        channel_id = %channel_id,
        sender_id = %sender_id,
        action,
        "Received channel typing indicator"
    );

    let sender_name = ui_ctx.user_service
        .get_user_local(&sender_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "Someone".to_string());

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        if action == 1 {
            window.set_typing_indicator(format!("{} is typing...", sender_name).into());
        } else {
            window.set_typing_indicator("".into());
        }
    });
}

pub(crate) fn handle_channel_reaction(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received channel reaction"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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

pub(crate) fn handle_channel_pin(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        unpin,
        "Received channel pin/unpin"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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

pub(crate) fn handle_channel_edit(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received channel message edit"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
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

pub(crate) fn handle_channel_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        "Received channel delete-for-everyone"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
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

pub(crate) fn handle_channel_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received channel delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != conversation_id {
            return;
        }

        let messages = window.get_messages();
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

// =============================================================================
// Receipt Handler (delivery/read status for channel messages)
// =============================================================================

pub(crate) fn handle_channel_receipt(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::info!(
        channel_id = %channel_id,
        receipt_type,
        count = timestamps.len(),
        timestamps = ?timestamps,
        "CHANNEL_RECEIPT: Processing delivery/read receipt"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
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
                    tracing::info!(
                        msg_timestamp = msg_ts_ns,
                        new_status = status,
                        "CHANNEL_RECEIPT: Updating message delivery_status in UI"
                    );
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

// =============================================================================
// Unread Count Handler
// =============================================================================

pub(crate) fn handle_channel_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    unread_count: u32,
) {
    tracing::debug!(
        channel_id = %channel_id,
        unread_count,
        "Channel unread count updated"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let channels = window.get_channels();

        for i in 0..channels.row_count() {
            if let Some(channel) = channels.row_data(i) {
                if channel.channel_id.as_str() == channel_id_str {
                    let mut updated_channel = channel.clone();
                    updated_channel.unread_count = unread_count as i32;

                    if let Some(model) = channels.as_any().downcast_ref::<VecModel<ChannelListItemData>>()
                    {
                        model.set_row_data(i, updated_channel);
                    }
                    return;
                }
            }
        }
    });
}

// =============================================================================
// Sidebar Preview Update
// =============================================================================

/// Update the channel sidebar preview with a sent message.
pub fn update_channel_sidebar_preview(
    window: &MainWindow,
    channel_id_str: &str,
    preview: &str,
    timestamp_str: &str,
) {
    let channels = window.get_channels();

    // Find and update the channel entry
    for i in 0..channels.row_count() {
        if let Some(channel) = channels.row_data(i) {
            if channel.channel_id.as_str() == channel_id_str {
                let mut updated_channel = channel.clone();
                updated_channel.last_message = preview.into();
                updated_channel.timestamp = timestamp_str.into();

                if let Some(model) = channels.as_any().downcast_ref::<VecModel<ChannelListItemData>>()
                {
                    model.set_row_data(i, updated_channel);
                }
                return;
            }
        }
    }

    tracing::debug!(
        channel_id = %channel_id_str,
        "Channel sidebar entry not found for sent message preview update"
    );
}

// =============================================================================
// Auto-Download Entry Point (called from inbox event handler)
// =============================================================================

/// Trigger auto-download for channel attachments (spawns background task).
pub(crate) fn trigger_channel_attachment_download(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    attachments: Vec<AttachmentPointer>,
) {
    if attachments.is_empty() {
        return;
    }

    let weak = weak_window.clone();
    let ui_ctx = ui_ctx.clone();
    let channel_str = channel_id.to_string();

    tokio::spawn(async move {
        auto_download_channel_attachments(
            attachments,
            ui_ctx,
            weak,
            channel_str,
        ).await;
    });
}
