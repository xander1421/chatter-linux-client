//! Shared attachment download handling for all channel types.
//!
//! Downloads encrypted content and handles GIF/image processing.
//! Used by DMs, anon DMs, channels, and anon groups.

use std::sync::Arc;

use slint::{Model, VecModel};

use chatter_core::domain::ids::AttachmentId;
use chatter_core::proto::content_v1::AttachmentPointer;
use chatter_core::services::attachment::AttachmentService;

use crate::MainWindow;

/// Download attachments and handle GIF/image processing.
///
/// This is the single entry point for all channel types.
/// Handles:
/// - Downloading encrypted content to filesystem
/// - GIF: decrypts once, decodes progressively, registers animation
/// - Images: generates thumbnails if missing
pub async fn download_attachments(
    pointers: Vec<AttachmentPointer>,
    attachment_service: Arc<AttachmentService>,
    weak_window: slint::Weak<MainWindow>,
    conversation_id: String,
) {
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
            conversation_id = %conversation_id,
            "Auto-downloading attachment"
        );

        // Verify attachment record exists
        match attachment_service.get_metadata(&att_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::error!(
                    attachment_id = %att_id,
                    cdn_key = %cdn_key,
                    "BUG: Attachment not found in DB"
                );
                continue;
            }
            Err(e) => {
                tracing::error!(
                    attachment_id = %att_id,
                    error = %e,
                    "Failed to check attachment record"
                );
                continue;
            }
        }

        // Download encrypted content to filesystem
        match attachment_service
            .download_to_filesystem(&att_id)
            .await
        {
            Ok(()) => {
                tracing::info!(cdn_key = %cdn_key, "Attachment downloaded");

                if is_gif {
                    handle_gif(
                        &attachment_service,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &conversation_id,
                    );
                } else if is_image && pointer.thumbnail.is_empty() {
                    handle_image(
                        &attachment_service,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &conversation_id,
                    );
                }
            }
            Err(e) => {
                tracing::error!(cdn_key = %cdn_key, error = %e, "Download failed");
            }
        }
    }
}

/// Handle GIF: decrypt once, decode progressively, register animation.
///
/// Uses service layer for decryption (single decrypt), then:
/// 1. Shows first frame immediately
/// 2. Decodes remaining frames in background
/// 3. Registers for inline animation (reuses decoded frames)
fn handle_gif(
    attachment_service: &Arc<AttachmentService>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    // Service layer: decrypt and decode first frame (SINGLE DECRYPT)
    let decode_result = match attachment_service.decrypt_and_decode_gif_first_frame(att_id) {
        Ok(Some(result)) => result,
        Ok(None) => {
            tracing::debug!(cdn_key = %cdn_key, "Not a GIF");
            return;
        }
        Err(e) => {
            tracing::error!(cdn_key = %cdn_key, error = %e, "Failed to decrypt/decode GIF");
            return;
        }
    };

    let att_id_str = att_id.to_string();
    let weak = weak_window.clone();
    let conv_id = conversation_id.to_string();

    // Show first frame immediately
    let first_frame = decode_result.first_frame;
    let first_rgba = first_frame.data.clone();
    let first_width = first_frame.width;
    let first_height = first_frame.height;
    let att_id_first = att_id_str.clone();
    let weak_first = weak.clone();
    let conv_id_first = conv_id.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &first_rgba,
            first_width,
            first_height,
        );
        let first_image = slint::Image::from_rgba8(buffer);

        if let Some(window) = weak_first.upgrade() {
            update_attachment_thumbnail(&window, &att_id_first, first_image, &conv_id_first);
            tracing::info!("GIF first frame displayed: {}", att_id_first);
        }
    });

    // Background: decode remaining frames (REUSES ALREADY DECRYPTED BYTES)
    let gif_bytes = decode_result.gif_bytes;
    tokio::task::spawn_blocking(move || {
        use chatter_core::services::attachment::AttachmentService;

        let start = std::time::Instant::now();

        // Decode all frames from already-decrypted bytes
        let full_result = match AttachmentService::decode_gif_all_frames(&gif_bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to decode GIF frames: {}", e);
                return;
            }
        };

        // Convert to Slint buffers
        let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = Vec::new();
        let mut all_delays: Vec<u64> = Vec::new();

        for frame in &full_result.frames {
            let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &frame.data,
                frame.width,
                frame.height,
            );
            all_buffers.push(pixel_buffer);
            all_delays.push(frame.delay.as_millis() as u64);
        }

        tracing::info!(
            "GIF decoded in {:?}: {} frames, {}",
            start.elapsed(),
            all_buffers.len(),
            att_id_str
        );

        // Register animation (frames already decoded, no re-decrypt needed)
        if all_buffers.len() > 1 {
            let att_id_full = att_id_str.clone();
            let _ = slint::invoke_from_event_loop(move || {
                crate::bin_ui::gif_state::register_inline_gif_from_buffers(
                    &att_id_full,
                    all_buffers,
                    all_delays,
                );
                tracing::info!("GIF animation registered: {}", att_id_full);
            });
        }
    });
}

/// Handle image: generate thumbnail if missing.
fn handle_image(
    attachment_service: &Arc<AttachmentService>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    let image_result = match attachment_service.decrypt_and_get_image_info(att_id) {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(cdn_key = %cdn_key, error = %e, "Failed to process image");
            return;
        }
    };

    let cdn_key_for_ui = cdn_key.to_string();
    let conv_id = conversation_id.to_string();
    let weak = weak_window.clone();
    let width = image_result.width;
    let height = image_result.height;
    let image_bytes = image_result.image_bytes;

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            match crate::bin_ui::slint_conversions::memory_to_slint_image(&image_bytes) {
                Ok(image) => {
                    update_attachment_thumbnail_with_dims(
                        &window,
                        &cdn_key_for_ui,
                        image,
                        width as i32,
                        height as i32,
                        &conv_id,
                    );
                }
                Err(e) => {
                    tracing::error!(cdn_key = %cdn_key_for_ui, error = ?e, "Failed to create image");
                }
            }
        }
    });
}

/// Update attachment thumbnail in message list.
pub fn update_attachment_thumbnail(
    window: &MainWindow,
    attachment_id: &str,
    image: slint::Image,
    conversation_id: &str,
) {
    if !is_conversation_active(window, conversation_id) && !conversation_id.is_empty() {
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
                        updated_att.is_downloading = false;
                        updated_att.progress = 1.0;

                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            att_model.set_row_data(att_idx, updated_att);
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Update attachment thumbnail with dimensions.
pub fn update_attachment_thumbnail_with_dims(
    window: &MainWindow,
    cdn_key: &str,
    image: slint::Image,
    width: i32,
    height: i32,
    conversation_id: &str,
) {
    if !is_conversation_active(window, conversation_id) {
        return;
    }

    let messages = window.get_messages();
    let att_id = AttachmentId::from_cdn_key(cdn_key);
    let att_id_str = att_id.to_string();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = msg.attachments.clone();

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == att_id_str || att.id.as_str() == cdn_key {
                        let mut updated_att = att.clone();
                        updated_att.has_thumbnail = true;
                        updated_att.thumbnail = image.clone();
                        updated_att.width = width;
                        updated_att.height = height;
                        updated_att.is_downloading = false;
                        updated_att.progress = 1.0;

                        if let Some(model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            model.set_row_data(att_idx, updated_att);
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Check if conversation is currently active.
fn is_conversation_active(window: &MainWindow, conversation_id: &str) -> bool {
    // Check anon groups
    let selected_group = window.get_selected_private_group_id().to_string();
    if !selected_group.is_empty() && selected_group == conversation_id {
        return true;
    }

    // Check channels
    let selected_channel = window.get_selected_channel_id().to_string();
    if !selected_channel.is_empty() && selected_channel == conversation_id {
        return true;
    }

    // Check DMs (regular and anon)
    let selected_dm = window.get_selected_dm_id().to_string();

    // Anon DM with prefix
    if let Some(anon_id) = selected_dm.strip_prefix("anon_dm:") {
        if anon_id == conversation_id {
            return true;
        }
    }

    // Direct comparison
    selected_dm == conversation_id
}
