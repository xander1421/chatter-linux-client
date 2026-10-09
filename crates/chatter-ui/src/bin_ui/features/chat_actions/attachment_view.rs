//! Attachment viewing handler.
//!
//! Handles viewing attachments (images, audio, video, code, PDF, etc.)
//! with secure in-memory decryption.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::attachment::{
    AttachmentStatus, ContentLocation, FileCategory, FileMetadata,
};
use chatter_core::domain::ids::AttachmentId;
use chatter_core::media::ImageProcessor;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::system::ExternalOpener;

use crate::MainWindow;
use crate::bin_ui::audio_state;
use crate::bin_ui::converters::format_file_size;
use crate::bin_ui::preview_state;
use crate::bin_ui::video_state;

use super::timers::{start_audio_progress_timer, start_video_progress_timer};
use super::transfer_status::update_attachment_transfer_status;

/// Wire the view attachment callback.
pub fn wire_view_attachment(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let tokio_handle = ui_ctx.runtime_handle().clone();
    let weak_window = window.as_weak();

    window.on_view_attachment(move |attachment_id: SharedString| {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let request = super::media_viewer::select(&w, attachment_id.as_str());
        let attachment_id_str = attachment_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let handle = tokio_handle.clone();

        tracing::debug!("View attachment clicked: {}", attachment_id_str);

        // Move database lookup off UI thread
        std::thread::spawn(move || {
            // Try to parse as UUID first, then try as cdn_key-derived ID
            let att_id: AttachmentId = match attachment_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    // Not a UUID - try as cdn_key
                    AttachmentId::from_cdn_key(&attachment_id_str)
                }
            };

            // Look up attachment from service (now in background thread)
            let mut attachment = match ui_ctx.get_attachment(&att_id) {
                Some(att) => att,
                None => {
                    tracing::warn!("Attachment {} not found in local repository", att_id);
                    return;
                }
            };

            // Check content location for secure in-memory flow
            let content_available = attachment.content_location == ContentLocation::OnFilesystem;
            let needs_download = attachment.content_location == ContentLocation::NotDownloaded
                && attachment.remote_url.is_some()
                && attachment.encryption_key.is_some();

            // Debug: Log attachment state for troubleshooting
            tracing::info!(
                attachment_id = %att_id,
                content_location = ?attachment.content_location,
                status = ?attachment.status,
                content_available = content_available,
                needs_download = needs_download,
                has_remote_url = attachment.remote_url.is_some(),
                has_encryption_key = attachment.encryption_key.is_some(),
                mime_type = %attachment.metadata.mime_type,
                "CLICK: Attachment lookup result"
            );

            // Follow an existing transfer without reopening an obsolete viewer request.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while attachment.status == AttachmentStatus::Downloading {
                if !super::media_viewer::is_current(request)
                    || std::time::Instant::now() >= deadline
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                let Some(updated) = ui_ctx.get_attachment(&att_id) else {
                    return;
                };
                attachment = updated;
            }
            let content_available = attachment.content_location == ContentLocation::OnFilesystem;
            let needs_download = attachment.content_location == ContentLocation::NotDownloaded
                && attachment.remote_url.is_some()
                && attachment.encryption_key.is_some();

            // SECURE IN-MEMORY PLAYBACK: Content is available locally, decrypt to memory
            if content_available && attachment.status == AttachmentStatus::Ready {
                handle_local_attachment(
                    weak,
                    ui_ctx.clone(),
                    att_id,
                    attachment_id,
                    &attachment.metadata.name,
                    attachment.metadata.size,
                    &attachment.metadata.mime_type,
                    request,
                );
                return;
            }

            // SECURE DOWNLOAD: Download encrypted content to database (no disk)
            if needs_download {
                handle_download_attachment(
                    handle,
                    weak,
                    ui_ctx.clone(),
                    attachment,
                    att_id,
                    request,
                );
                return;
            }

            // No valid content source - show warning
            tracing::warn!(
                "Attachment {} has no accessible content (location={:?}, status={:?})",
                att_id,
                attachment.content_location,
                attachment.status
            );
        });
    });
}

/// Handle locally available attachment (decrypt and display).
///
/// NOTE: This function is called from a background thread (std::thread::spawn),
/// so we can do synchronous decryption here without blocking the UI.
fn handle_local_attachment(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    att_id: AttachmentId,
    attachment_id: SharedString,
    filename: &str,
    file_size: u64,
    mime_type: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    tracing::debug!(
        "Attachment {} ready, decrypting in background thread",
        att_id
    );

    let filename = filename.to_string();
    let mime_type = mime_type.to_string();

    // OPTIMIZATION: For audio, check if already playing BEFORE decrypting
    let is_audio = mime_type.starts_with("audio/");
    if is_audio {
        let currently_playing = audio_state::get_current_playing_id();
        if currently_playing.as_deref() == Some(attachment_id.as_str()) {
            // Already playing this audio - just toggle pause (no decrypt needed!)
            toggle_audio_playback(&weak);
            return;
        }
    }

    // Show preview immediately for static images (from thumbnail), then load full
    // Skip for GIFs - they handle their own display flow to avoid black screen flash
    let is_image = FileMetadata::is_image_file(&filename, &mime_type);
    let is_gif = mime_type == "image/gif" || filename.to_lowercase().ends_with(".gif");
    if is_image && !is_gif {
        show_image_preview(
            &weak,
            &ui_ctx,
            &att_id,
            attachment_id.as_str(),
            &filename,
            file_size,
            request,
        );
    }

    // OPTIMIZATION: For GIFs, try to reuse inline animation frames (avoids re-decryption)
    // INLINE_GIFS is thread-local to UI thread, so we handle GIFs entirely on UI thread
    if is_gif {
        let att_id_str = attachment_id.to_string();
        let filename_for_gif = filename.clone();
        let size_display = format_file_size(file_size);
        let weak_for_gif = weak.clone();
        let ui_ctx_for_gif = ui_ctx.clone();
        let att_id_for_gif = att_id.clone();

        // Schedule on UI thread: check inline cache, then decrypt only if needed
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_for_gif.upgrade() {
                if !super::media_viewer::is_current(request) {
                    return;
                }
                if w.get_media_viewer_attachment_id().as_str() != att_id_str {
                    return;
                }
                let weak_inner = w.as_weak();

                // First, try to reuse inline frames (instant, no decryption)
                if let Some(first_frame) =
                    crate::bin_ui::gif_state::start_viewer_from_inline(&att_id_str, &weak_inner)
                {
                    w.set_image_viewer_source(first_frame);
                    w.set_image_viewer_filename(filename_for_gif.into());
                    w.set_image_viewer_size(size_display.into());
                    w.set_image_viewer_visible(true);
                    crate::bin_ui::gif_state::resume();
                    tracing::info!("Opened GIF viewer using cached inline frames (no decrypt)");
                    return;
                }

                // No inline frames - decrypt in background and display
                tracing::debug!("No inline GIF frames cached for {}, decrypting", att_id_str);
                let weak_decrypt = weak_inner.clone();
                let att_id_str_decrypt = att_id_str.clone();
                std::thread::spawn(move || {
                    match ui_ctx_for_gif.decrypt_attachment_auto(&att_id_for_gif) {
                        Ok(buffer) => {
                            let data = buffer.as_slice().to_vec();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak_decrypt.upgrade() {
                                    if !super::media_viewer::is_current(request) {
                                        return;
                                    }
                                    if w.get_media_viewer_attachment_id().as_str()
                                        != att_id_str_decrypt
                                    {
                                        return;
                                    }
                                    let weak_anim = w.as_weak();
                                    if let Some(first_frame) =
                                        crate::bin_ui::gif_state::start_gif_animation_and_inline(
                                            &data,
                                            &weak_anim,
                                            &att_id_str_decrypt,
                                        )
                                    {
                                        w.set_image_viewer_source(first_frame);
                                        w.set_image_viewer_filename(filename_for_gif.into());
                                        w.set_image_viewer_size(size_display.into());
                                        w.set_image_viewer_visible(true);
                                        crate::bin_ui::gif_state::resume();
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to decrypt GIF: {}", e);
                        }
                    }
                });
            }
        });
        return; // GIF handled entirely above
    }

    // OPTIMIZATION: For large videos, stream decrypted ranges into the player
    // instead of decrypting the whole file to memory first. Playback starts
    // without the multi-second full-decrypt wall (and without holding the whole
    // plaintext in RAM). Small videos keep the simple full-decrypt path.
    if crate::platform::video_source::use_video_ranges(
        if FileCategory::from_mime_and_filename(&mime_type, &filename) == FileCategory::Video {
            "video/unknown"
        } else {
            &mime_type
        },
        file_size,
        cfg!(target_os = "android"),
    ) {
        match ui_ctx.open_attachment_range(&att_id) {
            Ok(dec) => {
                play_video_from_range(weak, std::sync::Arc::new(dec), &filename, request);
                return;
            }
            Err(e) => {
                tracing::warn!("Streaming video open failed ({})", e);
                #[cfg(target_os = "android")]
                {
                    // An unavailable/unauthenticated range source must not
                    // trigger a multi-GiB plaintext allocation on a phone.
                    crate::platform::android::media::show_video_error();
                    return;
                }
                // Desktop retains its existing full-decrypt fallback.
            }
        }
    }

    // Same streaming path for large audio: rodio's decoder is Read+Seek, so it
    // pulls decrypted ranges on demand instead of holding the whole track in RAM.
    const AUDIO_STREAM_THRESHOLD: u64 = 4 * 1024 * 1024; // 4MB
    if mime_type.starts_with("audio/") && file_size >= AUDIO_STREAM_THRESHOLD {
        match ui_ctx.open_attachment_range(&att_id) {
            Ok(dec) => {
                play_audio_from_range(
                    weak,
                    std::sync::Arc::new(dec),
                    &attachment_id.to_string(),
                    &mime_type,
                    request,
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    "Streaming audio open failed ({}), falling back to full decrypt",
                    e
                );
                // fall through to full-decrypt path below
            }
        }
    }

    #[cfg(target_os = "android")]
    if !is_audio
        && !is_image
        && FileCategory::from_mime_and_filename(&mime_type, &filename) != FileCategory::Video
        && !FileMetadata::is_code_file(&filename, &mime_type)
        && mime_type != "application/pdf"
        && !filename.to_lowercase().ends_with(".pdf")
    {
        let revision = ui_ctx.auth_state().get().session_revision;
        let result =
            crate::platform::android::media_export::save_attachment(&ui_ctx, &att_id, revision);
        let _ = weak.upgrade_in_event_loop(move |w| {
            if !super::media_viewer::is_current(request)
                || ui_ctx.auth_state().get().session_revision != revision
            {
                return;
            }
            let (message, variant) = match result {
                Ok(()) => ("Saved to Chatter downloads".to_string(), "success"),
                Err(error) => (error, "error"),
            };
            w.set_toast_message(message.into());
            w.set_toast_variant(variant.into());
            w.set_toast_visible(true);
        });
        return;
    }

    // Preview allocation is bounded by the authenticated reader's actual
    // plaintext length, not the peer-supplied display metadata.
    let preview_limit = if FileMetadata::is_code_file(&filename, &mime_type) {
        Some(4 * 1024 * 1024u64)
    } else if mime_type == "application/pdf" || filename.to_lowercase().ends_with(".pdf") {
        Some(32 * 1024 * 1024u64)
    } else {
        None
    };
    if let Some(limit) = preview_limit {
        let revision = ui_ctx.auth_state().get().session_revision;
        let reader = match ui_ctx.open_attachment_range(&att_id) {
            Ok(reader) => reader,
            Err(error) => {
                tracing::warn!(%error, "Authenticated preview source unavailable");
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if !super::media_viewer::is_current(request) {
                        return;
                    }
                    w.set_toast_message("Unable to open attachment".into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                });
                return;
            }
        };
        if reader.plaintext_size() > limit {
            let result = super::media_viewer::export(&ui_ctx, &att_id, false);
            let _ = weak.upgrade_in_event_loop(move |w| {
                if !super::media_viewer::is_current(request)
                    || ui_ctx.auth_state().get().session_revision != revision
                {
                    return;
                }
                let (message, variant) = match result {
                    Ok(message) => (format!("Too large to preview. {message}"), "success"),
                    Err(error) => (format!("Too large to preview. {error}"), "error"),
                };
                w.set_toast_message(message.into());
                w.set_toast_variant(variant.into());
                w.set_toast_visible(true);
            });
            return;
        }
        let buffer =
            chatter_core::prelude::SecureBuffer::new(reader.read_range(0, reader.plaintext_size()));
        if buffer.as_slice().len() as u64 != reader.plaintext_size()
            || ui_ctx.auth_state().get().session_revision != revision
            || !super::media_viewer::is_current(request)
        {
            return;
        }
        display_decrypted_content(
            ui_ctx.runtime_handle(),
            weak,
            buffer,
            attachment_id.as_str(),
            &filename,
            file_size,
            &mime_type,
            request,
        );
        return;
    }

    // Decrypt synchronously (we're already in a background thread)
    let secure_buffer = match ui_ctx.decrypt_attachment_auto(&att_id) {
        Ok(buffer) => buffer,
        Err(e) => {
            tracing::error!("Failed to decrypt attachment: {}", e);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if !super::media_viewer::is_current(request) {
                        return;
                    }
                    w.set_image_viewer_visible(false);
                }
            });
            return;
        }
    };

    display_decrypted_content(
        ui_ctx.runtime_handle(),
        weak,
        secure_buffer,
        &attachment_id.to_string(),
        &filename,
        file_size,
        &mime_type,
        request,
    );
}

/// Toggle audio playback (pause/resume).
fn toggle_audio_playback(weak: &slint::Weak<MainWindow>) {
    let player_arc = audio_state::get_audio_player();
    if let Ok(mut player_guard) = player_arc.lock() {
        if let Some(ref mut player) = *player_guard {
            if player.is_paused() {
                if let Err(e) = player.play() {
                    tracing::error!("Failed to resume audio: {}", e);
                } else {
                    audio_state::set_resumed();
                    let weak_for_resume = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_resume.upgrade() {
                            w.set_is_media_playing(true);
                        }
                    });
                }
            } else {
                player.pause();
                audio_state::set_paused();
                let weak_for_pause = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_pause.upgrade() {
                        w.set_is_media_playing(false);
                    }
                });
            }
        }
    }
}

/// Show image preview (thumbnail) while loading full image.
fn show_image_preview(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    att_id: &AttachmentId,
    selection_id: &str,
    filename: &str,
    file_size: u64,
    request: u64,
) {
    let weak_loading = weak.clone();
    let expected_selection = selection_id.to_owned();
    let preview_filename = filename.to_string();
    let preview_size = format_file_size(file_size);

    // Get preview from service layer (fast, sync)
    let preview_data = ui_ctx.get_attachment_preview_image(att_id);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_loading.upgrade() {
            if !super::media_viewer::is_current(request) {
                return;
            }
            if w.get_media_viewer_attachment_id().as_str() != expected_selection {
                return;
            }
            if let Some((rgba, width, height)) = preview_data {
                // Show thumbnail preview immediately
                w.set_image_viewer_source(crate::bin_ui::slint_conversions::rgba_to_slint_image(
                    &rgba, width, height,
                ));
                w.set_image_viewer_filename(format!("{} (loading...)", preview_filename).into());
                w.set_image_viewer_size(preview_size.into());
            } else {
                // No preview available
                w.set_image_viewer_filename("Loading...".into());
                w.set_image_viewer_size("Decrypting...".into());
            }
            w.set_image_viewer_visible(true);
        }
    });
}

/// Display decrypted content based on file type.
fn display_decrypted_content(
    handle: &tokio::runtime::Handle,
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    attachment_id: &str,
    filename: &str,
    file_size: u64,
    mime_type: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    let is_audio = mime_type.starts_with("audio/");
    let is_image = FileMetadata::is_image_file(filename, mime_type);
    let is_video = FileCategory::from_mime_and_filename(mime_type, filename) == FileCategory::Video;
    let is_code = FileMetadata::is_code_file(filename, mime_type);
    let is_pdf = mime_type == "application/pdf" || filename.to_lowercase().ends_with(".pdf");

    if is_audio {
        play_audio_from_buffer(weak, secure_buffer, attachment_id, request);
    } else if is_image {
        display_image_from_buffer(
            weak,
            secure_buffer,
            attachment_id,
            filename,
            file_size,
            request,
        );
    } else if is_video {
        play_video_from_buffer(weak, secure_buffer, filename, request);
    } else if is_code {
        display_code_from_buffer(weak, secure_buffer, filename, request);
    } else if is_pdf {
        display_pdf_from_buffer(weak, secure_buffer, filename, request);
    } else {
        // For other files, show Save As dialog. Spawned, never blocked on:
        // this runs both on a plain thread and inside a runtime task, and
        // `block_on` in the latter panics.
        #[cfg(not(target_os = "android"))]
        handle.spawn(save_file_from_buffer(secure_buffer, filename.to_string()));
        #[cfg(target_os = "android")]
        {
            // Android generic files are exported from authenticated ranges
            // before decryption; refuse a bypass of that session-guarded path.
            let _ = (secure_buffer, filename);
            let _ = weak.upgrade_in_event_loop(move |w| {
                if !super::media_viewer::is_current(request) {
                    return;
                }
                w.set_toast_message("Unable to save attachment. Reopen it and try again.".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            });
            let _ = handle;
        }
    }
}

/// Play audio from decrypted buffer.
fn play_audio_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    attachment_id: &str,
    request: u64,
) {
    let player_arc = audio_state::get_audio_player();
    match player_arc.lock() {
        Ok(mut player_guard) => {
            if let Some(ref mut player) = *player_guard {
                let data = secure_buffer.as_slice().to_vec();
                match player.load_from_memory(data) {
                    Ok(duration) => {
                        tracing::info!("Loaded audio from memory (duration: {:?})", duration);
                        if let Err(e) = player.play() {
                            tracing::error!("Failed to play audio: {}", e);
                        } else {
                            let att_id_for_state = attachment_id.to_string();
                            audio_state::set_playing_with_duration(&att_id_for_state, duration);
                            let att_id_for_ui = attachment_id.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    if !super::media_viewer::is_current(request) {
                                        return;
                                    }
                                    w.set_current_playing_attachment_id(att_id_for_ui.into());
                                    w.set_is_media_playing(true);
                                    start_audio_progress_timer(&w);
                                }
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to load audio from memory: {}", e);
                    }
                }
            } else {
                tracing::error!("Audio player not initialized");
            }
        }
        Err(e) => {
            tracing::error!("Failed to lock audio player: {}", e);
        }
    }
}

/// Play large audio by streaming decrypted ranges on demand (no full decrypt).
fn play_audio_from_range(
    weak: slint::Weak<MainWindow>,
    dec: std::sync::Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    attachment_id: &str,
    mime_type: &str,
    request: u64,
) {
    let player_arc = audio_state::get_audio_player();
    match player_arc.lock() {
        Ok(mut player_guard) => {
            if let Some(ref mut player) = *player_guard {
                match player.load_from_range(dec, Some(mime_type.to_string())) {
                    Ok(duration) => {
                        tracing::info!("Loaded streaming audio (duration: {:?})", duration);
                        if let Err(e) = player.play() {
                            tracing::error!("Failed to play streaming audio: {}", e);
                        } else {
                            let att_id_for_state = attachment_id.to_string();
                            audio_state::set_playing_with_duration(&att_id_for_state, duration);
                            let att_id_for_ui = attachment_id.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    if !super::media_viewer::is_current(request) {
                                        return;
                                    }
                                    w.set_current_playing_attachment_id(att_id_for_ui.into());
                                    w.set_is_media_playing(true);
                                    start_audio_progress_timer(&w);
                                }
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to load streaming audio: {}", e);
                    }
                }
            } else {
                tracing::error!("Audio player not initialized");
            }
        }
        Err(e) => {
            tracing::error!("Failed to lock audio player: {}", e);
        }
    }
}

/// Display image from decrypted buffer.
fn display_image_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    attachment_id: &str,
    filename: &str,
    file_size: u64,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_ui = filename.to_string();
    let size_display = format_file_size(file_size);
    let att_id_for_inline = attachment_id.to_string();

    // Check if it's a GIF first (quick header check)
    let is_gif = data.len() >= 6 && (&data[0..6] == b"GIF89a" || &data[0..6] == b"GIF87a");

    tracing::info!(
        attachment_id = %att_id_for_inline,
        is_gif = is_gif,
        data_len = data.len(),
        "CLICK: display_image_from_buffer called"
    );

    if is_gif {
        // GIF: Handle on UI thread
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if !super::media_viewer::is_current(request) {
                    return;
                }
                if w.get_media_viewer_attachment_id().as_str() != att_id_for_inline {
                    return;
                }
                let weak_for_gif = w.as_weak();
                tracing::info!(
                    "CLICK: Calling start_gif_animation_and_inline for {}",
                    att_id_for_inline
                );
                if let Some(first_frame) = crate::bin_ui::gif_state::start_gif_animation_and_inline(
                    &data,
                    &weak_for_gif,
                    &att_id_for_inline,
                ) {
                    tracing::info!("CLICK: Got first frame, setting viewer source");
                    w.set_image_viewer_source(first_frame);
                    w.set_image_viewer_filename(filename_for_ui.into());
                    w.set_image_viewer_size(size_display.into());
                    w.set_image_viewer_visible(true);
                    crate::bin_ui::gif_state::resume();
                    tracing::info!("Opened animated GIF in viewer");
                } else {
                    tracing::warn!(
                        "CLICK: start_gif_animation_and_inline returned None, trying static"
                    );
                    // Not animated, decode as static image
                    match crate::bin_ui::slint_conversions::memory_to_slint_image(&data) {
                        Ok(img) => {
                            w.set_image_viewer_source(img);
                            w.set_image_viewer_filename(filename_for_ui.into());
                            w.set_image_viewer_size(size_display.into());
                            w.set_image_viewer_visible(true);
                            tracing::info!("CLICK: Loaded GIF as static image");
                        }
                        Err(e) => tracing::error!("Failed to load GIF as static: {:?}", e),
                    }
                }
            }
        });
    } else {
        // Static image: decode synchronously (already on a background thread).
        let t0 = std::time::Instant::now();
        match ImageProcessor::load_from_memory(&data) {
            Ok(image) => {
                let rgba = image.to_rgba8();
                let width = rgba.width();
                let height = rgba.height();
                let rgba_data = rgba.into_raw();
                let mb = rgba_data.len() as f64 / (1024.0 * 1024.0);
                let decode_ms = t0.elapsed();
                tracing::info!(
                    "IMG TIMING decode={:?} {}x{} rgba={:.1}MB",
                    decode_ms,
                    width,
                    height,
                    mb
                );

                let queued = std::time::Instant::now();
                let _ = slint::invoke_from_event_loop(move || {
                    let invoke_latency = queued.elapsed();
                    if let Some(w) = weak.upgrade() {
                        if !super::media_viewer::is_current(request) {
                            return;
                        }
                        if w.get_media_viewer_attachment_id().as_str() != att_id_for_inline {
                            return;
                        }
                        let clone_start = std::time::Instant::now();
                        let img = crate::bin_ui::slint_conversions::rgba_to_slint_image(
                            &rgba_data, width, height,
                        );
                        let clone_ms = clone_start.elapsed();
                        w.set_image_viewer_source(img);
                        w.set_image_viewer_filename(filename_for_ui.into());
                        w.set_image_viewer_size(size_display.into());
                        w.set_image_viewer_visible(true);
                        tracing::info!(
                            "IMG TIMING invoke_latency={:?} clone+setsource={:?} total={:?} — Opened image in viewer",
                            invoke_latency,
                            clone_ms,
                            t0.elapsed()
                        );
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to load image from memory: {:?}", e);
            }
        }
    }
}

#[cfg(target_os = "android")]
fn play_video_from_buffer(
    _weak: slint::Weak<MainWindow>,
    buffer: chatter_core::prelude::SecureBuffer,
    filename: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    if crate::platform::android::media::play_video_guarded(
        crate::platform::video_source::VideoSource::Memory(buffer),
        filename,
        request,
    )
    .is_err()
    {
        tracing::warn!("Native video launch failed");
        crate::platform::android::media::show_video_error();
    }
}

#[cfg(target_os = "android")]
fn play_video_from_range(
    _weak: slint::Weak<MainWindow>,
    dec: std::sync::Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    if crate::platform::android::media::play_video_guarded(
        crate::platform::video_source::VideoSource::Range(dec),
        filename,
        request,
    )
    .is_err()
    {
        tracing::warn!("Native video launch failed");
        crate::platform::android::media::show_video_error();
    }
}

/// Play video from decrypted buffer.
#[cfg(not(target_os = "android"))]
fn play_video_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    filename: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_video = filename.to_string();

    video_state::start_video_from_memory_guarded_async(
        data,
        filename_for_video.clone(),
        move || super::media_viewer::is_current(request),
        move |result| match result {
            Ok((_, duration_str)) => {
                let filename_for_ui = filename_for_video.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        if !super::media_viewer::is_current(request) {
                            return;
                        }
                        w.set_video_player_filename(filename_for_ui.into());
                        w.set_video_player_duration(duration_str.into());
                        w.set_video_player_elapsed("0:00".into());
                        w.set_video_player_progress(0.0);
                        w.set_video_player_is_playing(true);
                        w.set_video_player_volume(1.0);
                        w.set_video_player_visible(true);
                        start_video_progress_timer(&w);
                        tracing::info!("Started video from memory: {}", filename_for_video);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to start video from memory: {}", e);
            }
        },
    );
}

/// Play a large video by streaming decrypted ranges on demand.
///
/// No full decrypt-to-memory: the GL pipeline pulls plaintext ranges from
/// `dec` as it needs them (see `video_state::start_video_from_range_async`).
#[cfg(not(target_os = "android"))]
fn play_video_from_range(
    weak: slint::Weak<MainWindow>,
    dec: std::sync::Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: &str,
    request: u64,
) {
    if !super::media_viewer::is_current(request) {
        return;
    }
    let filename_for_video = filename.to_string();

    video_state::start_video_from_range_guarded_async(
        dec,
        filename_for_video.clone(),
        move || super::media_viewer::is_current(request),
        move |result| match result {
            Ok((_, duration_str)) => {
                let filename_for_ui = filename_for_video.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        if !super::media_viewer::is_current(request) {
                            return;
                        }
                        w.set_video_player_filename(filename_for_ui.into());
                        w.set_video_player_duration(duration_str.into());
                        w.set_video_player_elapsed("0:00".into());
                        w.set_video_player_progress(0.0);
                        w.set_video_player_is_playing(true);
                        w.set_video_player_volume(1.0);
                        w.set_video_player_visible(true);
                        start_video_progress_timer(&w);
                        tracing::info!("Started streaming video: {}", filename_for_video);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to start streaming video: {}", e);
            }
        },
    );
}

/// Display code file from decrypted buffer as plain text.
///
/// Syntax-highlighted rendering (previously via `slint-code-view`) has been
/// removed; the preview panel shows the file name, line count and plain text.
fn display_code_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    filename: &str,
    request: u64,
) {
    let data = secure_buffer.as_slice().to_vec();
    let text = String::from_utf8_lossy(&data).to_string();
    let filename_for_ui = filename.to_string();
    let filename_for_state = filename.to_string();

    // Install content only after validating the viewer request on the UI thread.
    let line_count = text.lines().count() as i32;

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if !super::media_viewer::is_current(request) {
                return;
            }
            preview_state::clear_code_preview();
            preview_state::set_code_preview_content(filename_for_state, data);
            w.set_code_preview_filename(filename_for_ui.into());
            w.set_code_preview_language("".into());
            w.set_code_preview_line_count(line_count);
            w.set_code_preview_plain_text(text.into());
            w.set_code_preview_visible(true);
        }
    });
}

/// Display PDF from decrypted buffer.
fn display_pdf_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_core::prelude::SecureBuffer,
    filename: &str,
    request: u64,
) {
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_ui = filename.to_string();
    let filename_for_state = filename.to_string();
    #[cfg(feature = "desktop")]
    let data_for_render = data.clone();

    // Install content only after validating the viewer request on the UI thread.
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if !super::media_viewer::is_current(request) {
                return;
            }
            preview_state::clear_pdf_preview();
            preview_state::set_pdf_preview_content(filename_for_state, data);
            w.set_pdf_preview_filename(filename_for_ui.into());
            w.set_pdf_preview_page_count(0);
            w.set_pdf_preview_error("".into());
            w.set_pdf_preview_visible(true);

            // Render first page inline (desktop only — uses pdfium-render)
            #[cfg(feature = "desktop")]
            super::pdf_preview::render_pdf_preview(&w, &data_for_render);
            #[cfg(not(feature = "desktop"))]
            super::pdf_preview::render_pdf_preview(&w, &[]);

            tracing::info!("Opened PDF preview");
        }
    });
}

/// Save file to user-selected location.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn save_file_from_buffer(
    secure_buffer: chatter_core::prelude::SecureBuffer,
    filename_for_save: String,
) {
    let data = secure_buffer.as_slice().to_vec();

    let config = FilePickerConfig::new()
        .with_title("Save Attachment As")
        .with_default_path(
            dirs::download_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(super::media_viewer::export_basename(&filename_for_save)),
        );

    let destination = FilePicker::save_file(config).await;

    if let Some(destination) = destination {
        match super::media_viewer::save_export(&destination, &data) {
            Ok(destination) => {
                tracing::info!("Exported attachment to {}", destination.display());
                let opener = ExternalOpener;
                if let Err(e) = opener.open_file(&destination) {
                    tracing::warn!("Could not open exported file: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("Failed to save attachment: {}", e);
            }
        }
    } else {
        tracing::debug!("Save As dialog cancelled");
    }
}

#[cfg(target_os = "ios")]
async fn save_file_from_buffer(
    _secure_buffer: chatter_core::prelude::SecureBuffer,
    _filename: String,
) {
    tracing::warn!("Save file not supported on mobile");
}

/// Handle attachment that needs to be downloaded first.
fn handle_download_attachment(
    handle: tokio::runtime::Handle,
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    attachment: chatter_core::domain::attachment::Attachment,
    att_id: AttachmentId,
    request: u64,
) {
    // Mark as downloading immediately to prevent duplicate downloads
    let mut att_to_update = attachment.clone();
    att_to_update.status = AttachmentStatus::Downloading;
    if let Err(e) = ui_ctx.update_attachment(&att_to_update) {
        tracing::warn!("Failed to mark attachment as downloading: {}", e);
    }

    let cdn_key = attachment.remote_url.clone().unwrap();
    let file_name = attachment.metadata.name.clone();
    let mime_type = attachment.metadata.mime_type.clone();
    let file_size = attachment.metadata.size;
    let att_id_str = att_id.to_string();

    tracing::info!(
        cdn_key = %cdn_key,
        filename = %file_name,
        "Downloading encrypted attachment to database (secure)"
    );

    // Update UI to show downloading state
    let weak_for_ui = weak.clone();
    let att_id_for_ui = att_id_str.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak_for_ui.upgrade() {
            update_attachment_transfer_status(&window, &att_id_for_ui, true, false, 0.0);
        }
    });

    // Spawn async download task using provided runtime handle
    let weak_for_completion = weak.clone();
    let att_id_str_for_task = att_id_str.clone();
    let ui_ctx_for_task = ui_ctx.clone();

    handle.spawn(async move {
        match ui_ctx_for_task
            .download_attachment_to_filesystem(&att_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    cdn_key = %cdn_key,
                    attachment_id = %att_id,
                    "Encrypted content stored on filesystem (secure)"
                );

                // Update UI to clear downloading state
                let weak_for_done = weak_for_completion.clone();
                let att_id_for_done = att_id_str_for_task.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak_for_done.upgrade() {
                        update_attachment_transfer_status(
                            &window,
                            &att_id_for_done,
                            false,
                            false,
                            1.0,
                        );
                    }
                });

                // Reuse the authenticated range path for downloaded videos;
                // do not allocate a second whole-file plaintext buffer.
                if crate::platform::video_source::use_video_ranges(
                    if FileCategory::from_mime_and_filename(&mime_type, &file_name)
                        == FileCategory::Video
                    {
                        "video/unknown"
                    } else {
                        &mime_type
                    },
                    file_size,
                    cfg!(target_os = "android"),
                ) {
                    handle_local_attachment(
                        weak,
                        ui_ctx_for_task.clone(),
                        att_id,
                        att_id_str.into(),
                        &file_name,
                        file_size,
                        &mime_type,
                        request,
                    );
                    return;
                }

                #[cfg(target_os = "android")]
                if !mime_type.starts_with("audio/")
                    && !FileMetadata::is_image_file(&file_name, &mime_type)
                    && FileCategory::from_mime_and_filename(&mime_type, &file_name)
                        != FileCategory::Video
                    && !FileMetadata::is_code_file(&file_name, &mime_type)
                    && mime_type != "application/pdf"
                    && !file_name.to_lowercase().ends_with(".pdf")
                {
                    handle_local_attachment(
                        weak,
                        ui_ctx_for_task.clone(),
                        att_id,
                        att_id_str.into(),
                        &file_name,
                        file_size,
                        &mime_type,
                        request,
                    );
                    return;
                }

                // Reuse local classification so downloaded PDF/code paths
                // enforce the same authenticated preview bound.
                handle_local_attachment(
                    weak,
                    ui_ctx_for_task.clone(),
                    att_id,
                    att_id_str.into(),
                    &file_name,
                    file_size,
                    &mime_type,
                    request,
                );
            }
            Err(e) => {
                tracing::error!(
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to download attachment to database"
                );

                // Mark as failed so it can be retried
                if let Some(mut att) = ui_ctx_for_task.get_attachment(&att_id) {
                    att.status = AttachmentStatus::Failed;
                    let _ = ui_ctx_for_task.update_attachment(&att);
                }

                // Update UI to clear downloading state
                let att_id_for_ui = att_id_str_for_task.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak_for_completion.upgrade() {
                        update_attachment_transfer_status(
                            &window,
                            &att_id_for_ui,
                            false,
                            false,
                            0.0,
                        );
                    }
                });
            }
        }
    });
}
