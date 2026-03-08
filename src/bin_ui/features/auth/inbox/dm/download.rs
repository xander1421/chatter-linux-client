//! Attachment auto-download for DMs.
//!
//! Downloads encrypted content to filesystem and generates thumbnails.

use std::sync::Arc;

use slint::{Model, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::media::ImageProcessor;
use chatter_linux::proto::content_v1::AttachmentPointer;

use crate::bin_ui::features::chat::get_dm_channel_from_peer;
use crate::MainWindow;

/// Auto-download all attachments in background.
///
/// Downloads encrypted content to filesystem (no temporary files) as soon as pointer arrives.
/// For images, generates thumbnail. For GIFs, registers for inline animation.
pub async fn auto_download_attachments(
    pointers: Vec<AttachmentPointer>,
    ui_ctx: Arc<UIContext>,
    weak_window: slint::Weak<MainWindow>,
    peer_id: String,
) {
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
            "Auto-downloading attachment to filesystem"
        );

        // Verify attachment record exists (created by service layer in poll_dm_mailboxes)
        match attachment_service.get_metadata(&att_id) {
            Ok(Some(_)) => {
                tracing::debug!(
                    attachment_id = %att_id,
                    cdn_key = %cdn_key,
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
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to check attachment record"
                );
                continue;
            }
        }

        // SECURE: Download encrypted content to filesystem
        // Content remains encrypted until playback (decrypt_to_memory)
        match attachment_service
            .download_to_filesystem(&grpc_pool, &att_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    cdn_key = %cdn_key,
                    "Attachment downloaded to filesystem"
                );

                // For GIFs, use progressive decoding - show first frame immediately
                if is_gif {
                    handle_gif_download(
                        &attachment_service,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &peer_id,
                    );
                }
                // For non-GIF images without thumbnails, generate thumbnail
                else if is_image && pointer.thumbnail.is_empty() {
                    handle_image_thumbnail(
                        &attachment_service,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &peer_id,
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to download attachment to database"
                );
            }
        }
    }
}

fn handle_gif_download(
    attachment_service: &Arc<chatter_linux::services::attachment::AttachmentService>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    peer_id: &str,
) {
    match attachment_service.decrypt_auto(att_id) {
        Ok(secure_buffer) => {
            let data = secure_buffer.as_slice().to_vec();
            let att_id_str = att_id.to_string();
            let weak = weak_window.clone();
            let peer_id_clone = peer_id.to_string();

            // Progressive GIF loading:
            // 1. Decode first frame instantly (~10-20ms)
            // 2. Show it immediately in message list
            // 3. Decode remaining frames in background
            tokio::task::spawn_blocking(move || {
                use chatter_linux::media::gif::decode_first_frame_only;
                use image::codecs::gif::GifDecoder;
                use image::AnimationDecoder;
                use std::io::Cursor;

                let start = std::time::Instant::now();

                // Step 1: Decode first frame only (instant)
                let first_frame = match decode_first_frame_only(&data) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        tracing::debug!("Not a GIF: {}", att_id_str);
                        return;
                    }
                    Err(e) => {
                        tracing::error!("Failed to decode GIF first frame {}: {:?}", att_id_str, e);
                        return;
                    }
                };

                tracing::info!(
                    "GIF first frame decoded in {:?}: {}",
                    start.elapsed(),
                    att_id_str
                );

                // Step 2: Show first frame immediately
                let first_rgba = first_frame.data.clone();
                let first_width = first_frame.width;
                let first_height = first_frame.height;
                let att_id_for_first = att_id_str.clone();
                let weak_first = weak.clone();
                let peer_id_first = peer_id_clone.clone();

                let _ = slint::invoke_from_event_loop(move || {
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &first_rgba,
                        first_width,
                        first_height,
                    );
                    let first_image = slint::Image::from_rgba8(buffer);

                    if let Some(window) = weak_first.upgrade() {
                        update_attachment_thumbnail_in_messages(
                            &window,
                            &att_id_for_first,
                            first_image,
                            &peer_id_first,
                        );
                        tracing::info!("GIF first frame displayed: {}", att_id_for_first);
                    }
                });

                // Step 3: Decode remaining frames progressively
                let cursor = Cursor::new(&data);
                let decoder = match GifDecoder::new(cursor) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("Failed to create GIF decoder: {}", e);
                        return;
                    }
                };

                let frames_iter = decoder.into_frames();
                let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = vec![];
                let mut all_delays: Vec<u64> = vec![];
                let mut animation_started = false;
                const BATCH_SIZE: usize = 5;

                for (idx, frame_result) in frames_iter.enumerate() {
                    let frame = match frame_result {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!("GIF frame {} error: {}", idx, e);
                            continue;
                        }
                    };

                    let delay = frame.delay();
                    let (numer, denom) = delay.numer_denom_ms();
                    let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };
                    let delay_ms = delay_ms.max(20);

                    let buffer = frame.into_buffer();
                    let (width, height) = buffer.dimensions();
                    let raw_data = buffer.into_raw();

                    // Convert to Slint pixel buffer IN THE BACKGROUND THREAD (Send)
                    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &raw_data,
                        width,
                        height,
                    );

                    all_buffers.push(pixel_buffer);
                    all_delays.push(delay_ms);

                    // Start animation as soon as we have enough frames
                    if !animation_started && all_buffers.len() >= 2 {
                        animation_started = true;
                        let buffers_batch = all_buffers.clone();
                        let delays_batch = all_delays.clone();
                        let att_id_batch = att_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch).is_some() {
                                tracing::info!("GIF animation started early: {}", att_id_batch);
                            }
                        });
                    }
                    // Update animation periodically with new frames
                    else if animation_started && all_buffers.len() % BATCH_SIZE == 0 {
                        let buffers_batch = all_buffers.clone();
                        let delays_batch = all_delays.clone();
                        let att_id_batch = att_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                        });
                    }
                }

                tracing::info!(
                    "GIF full decode complete in {:?}: {} frames, {}",
                    start.elapsed(),
                    all_buffers.len(),
                    att_id_str
                );

                // Final update with all frames
                if all_buffers.len() > 1 {
                    let att_id_full = att_id_str.clone();
                    let buffers_full = all_buffers.clone();
                    let delays_full = all_delays.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::bin_ui::gif_state::register_inline_gif_from_buffers(&att_id_full, buffers_full, delays_full);
                        tracing::info!("GIF animation complete: {}", att_id_full);
                    });
                }
            });
        }
        Err(e) => {
            tracing::error!(
                cdn_key = %cdn_key,
                error = %e,
                "Failed to decrypt GIF for animation"
            );
        }
    }
}

fn handle_image_thumbnail(
    attachment_service: &Arc<chatter_linux::services::attachment::AttachmentService>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    peer_id: &str,
) {
    match attachment_service.decrypt_auto(att_id) {
        Ok(secure_buffer) => {
            match ImageProcessor::get_dimensions_from_memory(secure_buffer.as_slice()) {
                Ok((width, height)) => {
                    let image_bytes = secure_buffer.as_slice().to_vec();
                    let cdn_key_for_ui = cdn_key.to_string();
                    let peer_id_clone = peer_id.to_string();
                    let weak = weak_window.clone();

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

/// Check if the given conversation_id matches the currently active conversation.
///
/// Handles regular DMs, anon DMs (with "anon_dm:" prefix), and anon groups.
fn is_conversation_active(window: &MainWindow, conversation_id: &str) -> bool {
    // Check anon groups first
    let selected_group = window.get_selected_private_group_id().to_string();
    if !selected_group.is_empty() && selected_group == conversation_id {
        return true;
    }

    // Check DMs (regular and anon)
    let selected_dm = window.get_selected_dm_id().to_string();

    // Check for anon DM with prefix
    if let Some(anon_id) = selected_dm.strip_prefix("anon_dm:") {
        if anon_id == conversation_id {
            return true;
        }
    }

    // Check regular DM by peer channel
    let peer_channel_id = get_dm_channel_from_peer(window, conversation_id);
    if peer_channel_id.as_deref() == Some(selected_dm.as_str()) {
        return true;
    }

    // Direct comparison for regular DMs
    selected_dm == conversation_id
}

/// Update attachment thumbnail in messages (simpler version without dimensions).
pub fn update_attachment_thumbnail_in_messages(
    window: &MainWindow,
    attachment_id: &str,
    image: slint::Image,
    conversation_id: &str,
) {
    // Check if this is the active conversation
    let is_active = is_conversation_active(window, conversation_id);

    if !is_active && !conversation_id.is_empty() {
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
                        // Clear downloading state - download is complete
                        updated_att.is_downloading = false;
                        updated_att.progress = 1.0;

                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            att_model.set_row_data(att_idx, updated_att);
                            tracing::debug!("Updated GIF thumbnail in message list (download complete): {}", attachment_id);
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Update a message's attachment thumbnail from a pre-loaded image (memory-safe).
pub fn update_attachment_thumbnail_from_image(
    window: &MainWindow,
    cdn_key: &str,
    image: slint::Image,
    width: i32,
    height: i32,
    conversation_id: &str,
) {
    let is_active = is_conversation_active(window, conversation_id);

    if !is_active {
        tracing::debug!(
            cdn_key = %cdn_key,
            conversation_id = %conversation_id,
            "Skipping thumbnail update - not active conversation"
        );
        return;
    }

    // Find and update the message with this attachment
    let messages = window.get_messages();
    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = msg.attachments.clone();
            let mut found = false;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    let att_id = AttachmentId::from_cdn_key(cdn_key);
                    if att.id.as_str() == att_id.to_string() || att.id.as_str() == cdn_key {
                        found = true;

                        let mut updated_att = att.clone();
                        updated_att.has_thumbnail = true;
                        updated_att.thumbnail = image.clone();
                        updated_att.width = width;
                        updated_att.height = height;
                        // Clear downloading state - download is complete
                        updated_att.is_downloading = false;
                        updated_att.progress = 1.0;

                        if let Some(model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            model.set_row_data(att_idx, updated_att);
                            tracing::debug!(
                                cdn_key = %cdn_key,
                                msg_idx,
                                att_idx,
                                "Updated attachment thumbnail in UI (download complete)"
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
