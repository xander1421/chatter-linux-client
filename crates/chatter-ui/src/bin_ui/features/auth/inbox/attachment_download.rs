//! Shared attachment download handling for all channel types.
//!
//! Downloads encrypted content and handles GIF/image processing.
//! Used by DMs, anon DMs, channels, and anon groups.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::{Model, VecModel};

use chatter_core::domain::Attachment;
use chatter_core::domain::ids::AttachmentId;

use crate::MainWindow;

/// Fetch media previews, then auto-download small images and process them.
///
/// This is the single entry point for all channel types.
/// Takes resolved `Attachment` domain objects (not proto types).
pub async fn download_attachments(
    attachments: &[Attachment],
    ui_ctx: Arc<chatter_core::client::ChatterClient>,
    weak_window: slint::Weak<MainWindow>,
    conversation_id: String,
    own_sent: bool,
) {
    for att in attachments {
        let cdn_key = match &att.remote_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => continue,
        };
        let att_id = att.id;
        let mime = &att.metadata.mime_type;
        let filename = &att.metadata.name;
        // Media-kind classification is domain behavior on the attachment metadata.
        let is_image = att.metadata.is_image();
        let is_video = att.metadata.is_video();
        let is_gif = att.metadata.is_gif();
        let mut has_thumbnail = att
            .metadata
            .thumbnail
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false);

        // Verify attachment record exists
        if ui_ctx.get_attachment_metadata(&att_id).is_none() {
            tracing::error!(
                attachment_id = %att_id,
                cdn_key = %cdn_key,
                "BUG: Attachment not found in DB"
            );
            continue;
        }

        // Instant preview from the thumbnail sidecar — a small, fast fetch that
        // shows a preview BEFORE the (potentially huge) full blob downloads.
        // GIFs included: `ensure_attachment_thumbnail` PERSISTS the decrypted
        // sidecar into the attachment record, so the bubble renders a static
        // preview durably on reopen/restart (the DB record's `thumbnail` field
        // is otherwise None on receive). Without this a GIF showed a bare
        // placeholder until clicked, because the live-only first-frame push in
        // `handle_gif` is lost on any re-render. The animated frames still come
        // from the full download below.
        if (is_image || is_video) && !has_thumbnail {
            if let Some(thumb_bytes) = ui_ctx.ensure_attachment_thumbnail(&att_id).await {
                has_thumbnail = true;
                let weak = weak_window.clone();
                let att_id_p = att_id.to_string();
                let conv = conversation_id.clone();
                let ui_p = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        match crate::bin_ui::slint_conversions::memory_to_slint_image(&thumb_bytes)
                        {
                            Ok(image) => {
                                update_attachment_thumbnail(&window, &ui_p, &att_id_p, image, &conv)
                            }
                            Err(e) => {
                                tracing::debug!(error = ?e, "Failed to decode sidecar thumbnail for preview")
                            }
                        }
                    }
                });
            }
        }

        // The originating device already has the video, but its linked device
        // may have received no sidecar (older sends or a failed sidecar upload).
        // Recover that device's own preview from the authenticated full blob.
        if !att.should_auto_download() && !(own_sent && is_video && !has_thumbnail) {
            continue;
        }

        tracing::info!(
            cdn_key = %cdn_key,
            filename = %filename,
            content_type = %mime,
            conversation_id = %conversation_id,
            "Auto-downloading media attachment"
        );

        // Download encrypted content to filesystem
        match ui_ctx.download_attachment_to_filesystem(&att_id).await {
            Ok(()) => {
                tracing::info!(cdn_key = %cdn_key, "Attachment downloaded");

                if is_gif {
                    handle_gif(&ui_ctx, &att_id, &cdn_key, &weak_window, &conversation_id);
                } else if is_image && !has_thumbnail {
                    handle_image(&ui_ctx, &att_id, &cdn_key, &weak_window, &conversation_id);
                } else if is_video && !has_thumbnail {
                    handle_video(&ui_ctx, &att_id, &cdn_key, &weak_window, &conversation_id);
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
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    // Service layer: decrypt and decode first frame (SINGLE DECRYPT)
    let decode_result = match ui_ctx.decrypt_and_decode_gif_first_frame(att_id) {
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
    let ui_ctx_gif = ui_ctx.clone();

    // Show first frame immediately
    let first_frame = decode_result.first_frame;
    let first_rgba = first_frame.data.clone();
    let first_width = first_frame.width;
    let first_height = first_frame.height;
    let att_id_first = att_id_str.clone();
    let weak_first = weak.clone();
    let conv_id_first = conv_id.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let first_image = crate::bin_ui::slint_conversions::rgba_to_slint_image(
            &first_rgba,
            first_width,
            first_height,
        );

        if let Some(window) = weak_first.upgrade() {
            update_attachment_thumbnail(
                &window,
                &ui_ctx_gif,
                &att_id_first,
                first_image,
                &conv_id_first,
            );
            tracing::info!("GIF first frame displayed: {}", att_id_first);
        }
    });

    // Background: decode remaining frames (REUSES ALREADY DECRYPTED BYTES)
    let gif_bytes = decode_result.gif_bytes;
    let ui_ctx_decode = ui_ctx.clone();
    tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();

        // Decode all frames from already-decrypted bytes via the client façade.
        let full_result = match ui_ctx_decode.decode_gif_all_frames(&gif_bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to decode GIF frames: {}", e);
                return;
            }
        };

        // Thin the loop to the inline budget across its whole duration
        // (same selector as the other GIF paths), then build Slint buffers.
        let mut selected = chatter_core::media::gif::DecimatedGif::default();
        for frame in &full_result.frames {
            let delay_ms = frame.delay.as_millis() as u64;
            let (data, width, height) = crate::bin_ui::gif_state::fit_frame(
                frame.data.clone(), frame.width, frame.height,
                crate::bin_ui::gif_state::INLINE_FRAME_PX,
                crate::bin_ui::gif_state::INLINE_FRAME_PX,
            );
            if data.is_empty() { continue; }
            let bytes = data.len();
            selected = chatter_core::media::gif::decimate_gif_frame(
                selected, (data, width, height), delay_ms, bytes,
                crate::bin_ui::gif_state::MAX_VIEWER_FRAMES,
                crate::bin_ui::gif_state::INLINE_ANIMATION_BYTES,
            );
        }
        let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = Vec::new();
        let mut all_delays: Vec<u64> = Vec::new();
        for sample in selected.frames {
            let (data, width, height) = sample.frame;
            all_buffers.push(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &data, width, height,
            ));
            all_delays.push(sample.delay_ms);
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
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    let image_result = match ui_ctx.decrypt_and_get_image_info(att_id) {
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
    let ui_ctx_img = ui_ctx.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            match crate::bin_ui::slint_conversions::memory_to_slint_image(&image_bytes) {
                Ok(image) => {
                    update_attachment_thumbnail_with_dims(
                        &window,
                        &ui_ctx_img,
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

/// Handle video: extract a thumbnail frame if missing.
///
/// Server-managed DMs strip the inline thumbnail before sealing (sealed
/// envelope is padded to small buckets for metadata privacy), so the receiver
/// derives one from the downloaded video. Platform decode is blocking and runs
/// on a background thread; Android persists the frame for pin/profile reuse.
fn handle_video(
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    let ui_ctx_video = ui_ctx.clone();
    let att_id = *att_id;
    let cdn_key = cdn_key.to_string();
    let weak = weak_window.clone();
    let conv_id = conversation_id.to_string();
    let revision = ui_ctx.auth_snapshot().session_revision;

    tokio::task::spawn_blocking(move || {
        let thumb = match ui_ctx_video.decrypt_and_extract_video_thumbnail(&att_id) {
            Ok(result) => result,
            Err(e) => {
                tracing::error!(cdn_key = %cdn_key, error = %e, "Failed to extract video thumbnail");
                return;
            }
        };

        if ui_ctx_video.auth_snapshot().session_revision != revision {
            return;
        }
        let width = thumb.width;
        let height = thumb.height;
        let thumb_bytes = thumb.image_bytes;

        let _ = slint::invoke_from_event_loop(move || {
            if ui_ctx_video.auth_snapshot().session_revision != revision {
                return;
            }
            if let Some(window) = weak.upgrade() {
                match crate::bin_ui::slint_conversions::memory_to_slint_image(&thumb_bytes) {
                    Ok(image) => {
                        update_attachment_thumbnail_with_dims(
                            &window,
                            &ui_ctx_video,
                            &cdn_key,
                            image,
                            width as i32,
                            height as i32,
                            &conv_id,
                        );
                        crate::bin_ui::features::header::refresh_pinned_ui(&window, &ui_ctx_video);
                    }
                    Err(e) => {
                        tracing::error!(cdn_key = %cdn_key, error = ?e, "Failed to create video thumbnail image");
                    }
                }
            }
        });
    });
}

/// Update attachment thumbnail in message list.
pub fn update_attachment_thumbnail(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    attachment_id: &str,
    image: slint::Image,
    conversation_id: &str,
) {
    if !is_conversation_active(ui_ctx, conversation_id) && !conversation_id.is_empty() {
        return;
    }

    let messages = window.global::<crate::ChatPane>().get_messages();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = &msg.attachments;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == attachment_id {
                        let mut updated_att = att.clone();
                        let size = image.size();
                        if updated_att.width <= 0 || updated_att.height <= 0 {
                            updated_att.width = size.width as i32;
                            updated_att.height = size.height as i32;
                        }
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
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    cdn_key: &str,
    image: slint::Image,
    width: i32,
    height: i32,
    conversation_id: &str,
) {
    if !is_conversation_active(ui_ctx, conversation_id) {
        return;
    }

    let messages = window.global::<crate::ChatPane>().get_messages();
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

/// Check if the given (stringly-typed) conversation identifier refers to the
/// currently-viewed conversation. Delegates to the typed `matches_*`
/// predicates on `ActiveConversation` in `chatter-core`.
fn is_conversation_active(
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    conversation_id: &str,
) -> bool {
    let active = ui_ctx.active_conversation();
    // A conversation_id string from the UI may be any of: ChannelId uuid,
    // AnonDmId uuid, AnonGroupId uuid, or peer UserId uuid. Try each shape
    // against the matching predicate — exactly one (or none) will hit.
    let channel_match = conversation_id
        .parse()
        .map(|id| active.matches_channel(&id))
        .unwrap_or(false);
    let dm_channel_match = conversation_id
        .parse()
        .map(|id| match &active {
            chatter_core::state::chat_state::ActiveConversation::ServerDm { peer_id } => {
                chatter_core::domain::ids::ChannelId::from(*peer_id) == id
            }
            _ => false,
        })
        .unwrap_or(false);
    let anon_dm_match = conversation_id
        .parse()
        .map(|id| active.matches_anon_dm(&id))
        .unwrap_or(false);
    let anon_group_match = conversation_id
        .parse()
        .map(|id| active.matches_anon_group(&id))
        .unwrap_or(false);
    let peer_match = conversation_id
        .parse()
        .map(|id| active.matches_dm_peer(&id))
        .unwrap_or(false);
    channel_match || dm_channel_match || anon_dm_match || anon_group_match || peer_match
}
