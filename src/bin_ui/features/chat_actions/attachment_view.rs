//! Attachment viewing handler.
//!
//! Handles viewing attachments (images, audio, video, code, PDF, etc.)
//! with secure in-memory decryption.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::attachment::{AttachmentStatus, ContentLocation, FileMetadata};
use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::media::code_preview::CodeViewState;
use chatter_linux::media::{FilePicker, FilePickerConfig, ImageProcessor};
use chatter_linux::system::ExternalOpener;

use crate::bin_ui::audio_state;
use crate::bin_ui::converters::format_file_size;
use crate::bin_ui::preview_state;
use crate::bin_ui::video_state;
use crate::MainWindow;

use super::code_preview::set_code_view_state;
use super::timers::{start_audio_progress_timer, start_video_progress_timer};
use super::transfer_status::update_attachment_transfer_status;

/// Wire the view attachment callback.
pub fn wire_view_attachment(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let attachment_service = ui_ctx.attachment_service.clone();
    let grpc_pool = ui_ctx.grpc_pool.clone();
    let weak_window = window.as_weak();

    window.on_view_attachment(move |attachment_id: SharedString| {
        let attachment_id_str = attachment_id.to_string();
        let attachment_service = attachment_service.clone();
        let attachment_service_for_download = attachment_service.clone();
        let grpc_pool = grpc_pool.clone();
        let weak = weak_window.clone();

        tracing::debug!("View attachment clicked: {}", attachment_id_str);

        // Try to parse as UUID first, then try as cdn_key-derived ID
        let att_id: AttachmentId = match attachment_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                // Not a UUID - try as cdn_key
                AttachmentId::from_cdn_key(&attachment_id_str)
            }
        };

        // Look up attachment from service
        let attachment = match attachment_service.get(&att_id) {
            Ok(Some(att)) => att,
            Ok(None) => {
                tracing::warn!("Attachment {} not found in local repository", att_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to lookup attachment {}: {}", att_id, e);
                return;
            }
        };

        // Check content location for secure in-memory flow
        let content_available = attachment.content_location == ContentLocation::OnFilesystem;
        let needs_download = attachment.content_location == ContentLocation::NotDownloaded
            && attachment.remote_url.is_some()
            && attachment.encryption_key.is_some();

        // Skip if already downloading
        if attachment.status == AttachmentStatus::Downloading {
            tracing::debug!("Attachment {} already downloading, skipping", att_id);
            return;
        }

        // SECURE IN-MEMORY PLAYBACK: Content is available locally, decrypt to memory
        if content_available && attachment.status == AttachmentStatus::Ready {
            handle_local_attachment(
                weak,
                attachment_service,
                att_id,
                attachment_id,
                &attachment.metadata.name,
                attachment.metadata.size,
                &attachment.metadata.mime_type,
            );
            return;
        }

        // SECURE DOWNLOAD: Download encrypted content to database (no disk)
        if needs_download {
            handle_download_attachment(
                weak,
                attachment_service_for_download,
                grpc_pool,
                attachment,
                att_id,
            );
            return;
        }

        // Legacy fallback: Handle old attachments that have local_path (pre-migration)
        let has_legacy_local_path = attachment.local_path.is_some()
            && attachment.local_path.as_ref().is_some_and(|p| std::path::Path::new(p).exists());

        if has_legacy_local_path && attachment.status == AttachmentStatus::Ready {
            tracing::warn!("Using legacy filesystem path for attachment {} - consider re-downloading", att_id);
            if let Some(ref local_path) = attachment.local_path {
                let path = std::path::PathBuf::from(local_path);
                let filename = attachment.metadata.name.clone();
                let mime_type = attachment.metadata.mime_type.clone();

                // Legacy: open file with system handler
                let opener = ExternalOpener;
                if let Err(e) = opener.open_file(&path) {
                    tracing::error!("Failed to open legacy file {:?}: {}", path, e);
                } else {
                    tracing::info!("Opened legacy file: {} ({})", filename, mime_type);
                }
            }
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
}

/// Handle locally available attachment (decrypt and display).
fn handle_local_attachment(
    weak: slint::Weak<MainWindow>,
    attachment_service: Arc<chatter_linux::services::attachment::AttachmentService>,
    att_id: AttachmentId,
    attachment_id: SharedString,
    filename: &str,
    file_size: u64,
    mime_type: &str,
) {
    tracing::debug!("Attachment {} ready, spawning async decryption", att_id);

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
        show_image_preview(&weak, &attachment_service, &att_id, &filename, file_size);
    }

    // Spawn async task - don't block UI thread
    let weak_for_task = weak.clone();
    let attachment_id_for_task = attachment_id.clone();
    let filename_for_task = filename.clone();
    let mime_type_for_task = mime_type.clone();

    tokio::spawn(async move {
        // Decrypt in blocking task (CPU-bound)
        let secure_buffer = match tokio::task::spawn_blocking({
            let attachment_service = attachment_service.clone();
            move || attachment_service.decrypt_auto(&att_id)
        }).await {
            Ok(Ok(buffer)) => buffer,
            Ok(Err(e)) => {
                tracing::error!("Failed to decrypt attachment: {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_task.upgrade() {
                        w.set_image_viewer_visible(false);
                    }
                });
                return;
            }
            Err(e) => {
                tracing::error!("Decrypt task panicked: {}", e);
                return;
            }
        };

        display_decrypted_content(
            weak_for_task,
            secure_buffer,
            &attachment_id_for_task.to_string(),
            &filename_for_task,
            file_size,
            &mime_type_for_task,
        );
    });
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
    attachment_service: &Arc<chatter_linux::services::attachment::AttachmentService>,
    att_id: &AttachmentId,
    filename: &str,
    file_size: u64,
) {
    let weak_loading = weak.clone();
    let preview_filename = filename.to_string();
    let preview_size = format_file_size(file_size);

    // Get preview from service layer (fast, sync)
    let preview_data = attachment_service.get_preview_image(att_id);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_loading.upgrade() {
            if let Some((rgba, width, height)) = preview_data {
                // Show thumbnail preview immediately
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &rgba, width, height
                );
                w.set_image_viewer_source(slint::Image::from_rgba8(buffer));
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
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    attachment_id: &str,
    filename: &str,
    file_size: u64,
    mime_type: &str,
) {
    let is_audio = mime_type.starts_with("audio/");
    let is_image = FileMetadata::is_image_file(filename, mime_type);
    let is_video = mime_type.starts_with("video/");
    let is_code = FileMetadata::is_code_file(filename, mime_type);
    let is_pdf = mime_type == "application/pdf" || filename.to_lowercase().ends_with(".pdf");

    if is_audio {
        play_audio_from_buffer(weak, secure_buffer, attachment_id);
    } else if is_image {
        display_image_from_buffer(weak, secure_buffer, attachment_id, filename, file_size);
    } else if is_video {
        play_video_from_buffer(weak, secure_buffer, filename);
    } else if is_code {
        display_code_from_buffer(weak, secure_buffer, filename);
    } else if is_pdf {
        display_pdf_from_buffer(weak, secure_buffer, filename);
    } else {
        // For other files, show Save As dialog
        save_file_from_buffer(secure_buffer, filename);
    }
}

/// Play audio from decrypted buffer.
fn play_audio_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    attachment_id: &str,
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

/// Display image from decrypted buffer.
fn display_image_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    attachment_id: &str,
    filename: &str,
    file_size: u64,
) {
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_ui = filename.to_string();
    let size_display = format_file_size(file_size);
    let att_id_for_inline = attachment_id.to_string();

    // Check if it's a GIF first (quick header check)
    let is_gif = data.len() >= 6 && (&data[0..6] == b"GIF89a" || &data[0..6] == b"GIF87a");

    if is_gif {
        // GIF: Handle on UI thread
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let weak_for_gif = w.as_weak();
                if let Some(first_frame) = crate::bin_ui::gif_state::start_gif_animation_and_inline(
                    &data,
                    &weak_for_gif,
                    &att_id_for_inline,
                ) {
                    w.set_image_viewer_source(first_frame);
                    w.set_image_viewer_filename(filename_for_ui.into());
                    w.set_image_viewer_size(size_display.into());
                    w.set_image_viewer_visible(true);
                    crate::bin_ui::gif_state::resume();
                    tracing::info!("Opened animated GIF in viewer");
                } else {
                    // Not animated, decode as static image
                    match ImageProcessor::memory_to_slint_image(&data) {
                        Ok(img) => {
                            w.set_image_viewer_source(img);
                            w.set_image_viewer_filename(filename_for_ui.into());
                            w.set_image_viewer_size(size_display.into());
                            w.set_image_viewer_visible(true);
                        }
                        Err(e) => tracing::error!("Failed to load GIF as static: {:?}", e),
                    }
                }
            }
        });
    } else {
        // Static image: Decode in background thread
        tokio::task::spawn_blocking(move || {
            let decode_start = std::time::Instant::now();
            match ImageProcessor::load_from_memory(&data) {
                Ok(image) => {
                    let rgba = image.to_rgba8();
                    let width = rgba.width();
                    let height = rgba.height();
                    let rgba_data = rgba.into_raw();
                    tracing::debug!("Image decoded in {:?}: {}x{}", decode_start.elapsed(), width, height);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let img = slint::Image::from_rgba8(
                                slint::SharedPixelBuffer::clone_from_slice(&rgba_data, width, height)
                            );
                            w.set_image_viewer_source(img);
                            w.set_image_viewer_filename(filename_for_ui.into());
                            w.set_image_viewer_size(size_display.into());
                            w.set_image_viewer_visible(true);
                            tracing::info!("Opened image from memory in viewer");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to load image from memory: {:?}", e);
                }
            }
        });
    }
}

/// Play video from decrypted buffer.
fn play_video_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    filename: &str,
) {
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_video = filename.to_string();

    video_state::start_video_from_memory_async(
        data,
        filename_for_video.clone(),
        move |result| {
            match result {
                Ok((_, duration_str)) => {
                    let filename_for_ui = filename_for_video.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
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
            }
        },
    );
}

/// Display code with syntax highlighting from decrypted buffer.
fn display_code_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    filename: &str,
) {
    let data = secure_buffer.as_slice().to_vec();
    let text = String::from_utf8_lossy(&data).to_string();
    let filename_for_ui = filename.to_string();
    let filename_for_state = filename.to_string();
    let filename_for_code = filename.to_string();

    // Store content for "open external" feature
    preview_state::set_code_preview_content(filename_for_state, data);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Create CodeViewState and render
            let mut state = CodeViewState::new();
            state.set_code_from_file(&text, &filename_for_code);
            let image = state.render();
            let line_count = state.line_count() as i32;
            let language = state.language().to_string();
            let plain_text = state.plain_text().to_string();

            // Store state for selection handling
            set_code_view_state(state);

            w.set_code_preview_filename(filename_for_ui.into());
            w.set_code_preview_language(language.into());
            w.set_code_preview_line_count(line_count);
            w.set_code_preview_image(image);
            w.set_code_preview_plain_text(plain_text.into());
            w.set_code_preview_has_selection(false);
            w.set_code_preview_selected_text("".into());
            w.set_code_preview_visible(true);
        }
    });
}

/// Display PDF from decrypted buffer.
fn display_pdf_from_buffer(
    weak: slint::Weak<MainWindow>,
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    filename: &str,
) {
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_ui = filename.to_string();
    let filename_for_state = filename.to_string();

    // Store content for "open external" feature
    preview_state::set_pdf_preview_content(filename_for_state, data);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_pdf_preview_filename(filename_for_ui.into());
            w.set_pdf_preview_page_count(0);
            w.set_pdf_preview_error("".into());
            w.set_pdf_preview_visible(true);
            tracing::info!("Opened PDF preview from memory");
        }
    });
}

/// Save file to user-selected location.
fn save_file_from_buffer(
    secure_buffer: chatter_linux::crypto::SecureBuffer,
    filename: &str,
) {
    let data = secure_buffer.as_slice().to_vec();
    let filename_for_save = filename.to_string();

    tokio::spawn(async move {
        let config = FilePickerConfig::new()
            .with_title("Save Attachment As")
            .with_default_path(
                dirs::download_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join(&filename_for_save),
            );

        if let Some(destination) = FilePicker::save_file(config).await {
            match std::fs::write(&destination, &data) {
                Ok(()) => {
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
    });
}

/// Handle attachment that needs to be downloaded first.
fn handle_download_attachment(
    weak: slint::Weak<MainWindow>,
    attachment_service: Arc<chatter_linux::services::attachment::AttachmentService>,
    grpc_pool: Arc<chatter_linux::network::grpc::client_pool::GrpcClientPool>,
    attachment: chatter_linux::domain::attachment::Attachment,
    att_id: AttachmentId,
) {
    // Mark as downloading immediately to prevent duplicate downloads
    let mut att_to_update = attachment.clone();
    att_to_update.status = AttachmentStatus::Downloading;
    if let Err(e) = attachment_service.update(&att_to_update) {
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

    // Spawn async download task
    let weak_for_completion = weak.clone();
    let att_id_str_for_task = att_id_str.clone();
    let attachment_service_for_task = attachment_service.clone();

    tokio::spawn(async move {
        match attachment_service_for_task.download_to_filesystem(&grpc_pool, &att_id).await {
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
                        update_attachment_transfer_status(&window, &att_id_for_done, false, false, 1.0);
                    }
                });

                // Now decrypt to memory and display
                let secure_buffer = match attachment_service_for_task.decrypt_auto(&att_id) {
                    Ok(buffer) => buffer,
                    Err(e) => {
                        tracing::error!("Failed to decrypt downloaded attachment: {}", e);
                        return;
                    }
                };

                display_decrypted_content(
                    weak,
                    secure_buffer,
                    &att_id_str,
                    &file_name,
                    file_size,
                    &mime_type,
                );
            }
            Err(e) => {
                tracing::error!(
                    cdn_key = %cdn_key,
                    error = %e,
                    "Failed to download attachment to database"
                );

                // Mark as failed so it can be retried
                if let Ok(Some(mut att)) = attachment_service_for_task.get(&att_id) {
                    att.status = AttachmentStatus::Failed;
                    let _ = attachment_service_for_task.update(&att);
                }

                // Update UI to clear downloading state
                let att_id_for_ui = att_id_str_for_task.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak_for_completion.upgrade() {
                        update_attachment_transfer_status(&window, &att_id_for_ui, false, false, 0.0);
                    }
                });
            }
        }
    });
}
