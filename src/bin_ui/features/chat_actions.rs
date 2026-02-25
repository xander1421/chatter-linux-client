//! Chat action callbacks.
//!
//! Handles message interactions (reply, react, thread, more, view attachment).

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::attachment::{AttachmentStatus, ContentLocation, FileMetadata};
use chatter_linux::domain::ids::{AttachmentId, MessageId};
use chatter_linux::media::{FilePicker, FilePickerConfig, ImageProcessor};
use chatter_linux::services::AttachmentService;
use chatter_linux::system::ExternalOpener;
use zeroize::Zeroize;

use crate::bin_ui::audio_state;
use crate::bin_ui::converters::format_file_size;
use crate::bin_ui::preview_state;
use crate::bin_ui::video_state;
use crate::{CodeLine, MainWindow};

thread_local! {
    /// Thread-local audio progress timer (must be on UI thread).
    static AUDIO_PROGRESS_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Thread-local video progress timer (must be on UI thread).
    static VIDEO_PROGRESS_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Start the audio progress timer.
/// Must be called from the UI thread.
pub fn start_audio_progress_timer(window: &MainWindow) {
    let weak_window = window.as_weak();

    AUDIO_PROGRESS_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(w) = weak_window.upgrade() {
                    // Get current progress from audio state
                    let progress = audio_state::get_progress();
                    let position = audio_state::get_position();
                    let elapsed = audio_state::format_duration(position);

                    w.set_audio_progress(progress);
                    w.set_audio_elapsed(elapsed.into());

                    // Check if playback finished (progress >= 1.0 or sink empty)
                    if progress >= 0.999 {
                        // Stop timer and clear state
                        stop_audio_progress_timer();
                        audio_state::clear_state();
                        w.set_current_playing_attachment_id("".into());
                        w.set_is_media_playing(false);
                        w.set_audio_progress(0.0);
                        w.set_audio_elapsed("0:00".into());
                    }
                }
            },
        );

        *timer_cell.borrow_mut() = Some(timer);
    });
}

/// Stop the audio progress timer.
/// Must be called from the UI thread.
pub fn stop_audio_progress_timer() {
    AUDIO_PROGRESS_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}

/// Start the video progress timer.
/// Must be called from the UI thread.
/// This timer handles both progress updates AND frame rendering.
pub fn start_video_progress_timer(window: &MainWindow) {
    let weak_window = window.as_weak();

    VIDEO_PROGRESS_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer - run at ~1ms for high refresh rate video (up to 540Hz)
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            Duration::from_micros(1850), // ~540Hz max
            move || {
                if let Some(w) = weak_window.upgrade() {
                    // Take latest frame from buffer and render it
                    if let Some(frame) = video_state::take_frame() {
                        let image = chatter_linux::media::rgb_to_slint_image(
                            frame.width,
                            frame.height,
                            &frame.rgb_data,
                        );
                        w.set_video_player_frame(image);
                    }

                    // Get current state from video player
                    if let Some((position, duration, is_playing)) = video_state::get_playback_state() {
                        // Clamp position to duration (some videos report timestamps beyond duration)
                        let clamped_position = position.min(duration);
                        let progress = if duration > 0.0 { (clamped_position / duration).clamp(0.0, 1.0) } else { 0.0 };
                        let elapsed = chatter_linux::media::format_duration(clamped_position);

                        w.set_video_player_progress(progress as f32);
                        w.set_video_player_elapsed(elapsed.into());
                        w.set_video_player_is_playing(is_playing);

                        // Check if playback finished (only when not playing and at end)
                        // Don't stop on progress alone - wait for actual EOS from decoder
                        if !is_playing && progress >= 0.99 {
                            // Video ended - stop timer but keep player visible
                            stop_video_progress_timer();
                            w.set_video_player_is_playing(false);
                        }
                    } else {
                        // Player was stopped/closed
                        stop_video_progress_timer();
                        w.set_video_player_visible(false);
                    }
                }
            },
        );

        *timer_cell.borrow_mut() = Some(timer);
    });
}

/// Stop the video progress timer.
/// Must be called from the UI thread.
pub fn stop_video_progress_timer() {
    VIDEO_PROGRESS_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}

/// Update attachment transfer status in the UI.
/// Must be called from the UI thread via invoke_from_event_loop.
fn update_attachment_transfer_status(
    window: &MainWindow,
    attachment_id: &str,
    is_downloading: bool,
    is_uploading: bool,
    progress: f32,
) {
    let messages = window.get_messages();
    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = msg.attachments.clone();
            let mut found = false;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == attachment_id {
                        found = true;

                        // Update the attachment status
                        let mut updated_att = att.clone();
                        updated_att.is_downloading = is_downloading;
                        updated_att.is_uploading = is_uploading;
                        updated_att.progress = progress;

                        // Update in the attachments model
                        if let Some(att_model) = attachments.as_any().downcast_ref::<VecModel<crate::MessageAttachmentData>>() {
                            att_model.set_row_data(att_idx, updated_att);
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

/// Wire message action callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_message_clicked(window);
    wire_reply_clicked(window, ctx);
    wire_react_clicked(window);
    wire_thread_clicked(window, ctx);
    // wire_more_clicked moved to chat.rs (shows context menu)
    wire_view_attachment(window, ctx);
    wire_audio_seek(window, ctx);
    wire_close_image_viewer(window);
    wire_code_preview_callbacks(window, ctx);
    wire_pdf_preview_callbacks(window, ctx);
    wire_video_player_callbacks(window);
}

fn wire_close_image_viewer(window: &MainWindow) {
    window.on_close_image_viewer(|| {
        // Stop any GIF animation when closing the viewer
        crate::bin_ui::gif_state::stop_gif_animation();
        tracing::debug!("Image viewer closed");
    });
}

fn wire_code_preview_callbacks(window: &MainWindow, _ctx: &AppContext) {
    use crate::bin_ui::preview_state;

    // Close code preview
    window.on_close_code_preview(|| {
        preview_state::clear_code_preview();
        tracing::debug!("Code preview closed");
    });

    // Copy code to clipboard
    let weak_window = window.as_weak();
    window.on_copy_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let lines = w.get_code_preview_lines();
            let mut content = String::new();
            for i in 0..lines.row_count() {
                if let Some(line) = lines.row_data(i) {
                    content.push_str(&line.content);
                    content.push('\n');
                }
            }
            // Copy to clipboard using arboard
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                if let Err(e) = clipboard.set_text(&content) {
                    tracing::error!("Failed to copy to clipboard: {}", e);
                } else {
                    tracing::info!("Copied {} lines to clipboard", lines.row_count());
                }
            }
        }
    });

    // Open code in external editor - uses Save As for secure export
    window.on_open_code_external(|| {
        if let Some(content) = preview_state::get_code_preview_content() {
            // Show Save As dialog then open with system handler
            let filename = content.filename.clone();
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save Code File")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match std::fs::write(&destination, &data) {
                        Ok(()) => {
                            tracing::info!("Exported code to {}", destination.display());
                            let opener = ExternalOpener;
                            if let Err(e) = opener.open_file(&destination) {
                                tracing::warn!("Could not open exported file: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to save code file: {}", e),
                    }
                }
            });
        } else {
            tracing::warn!("No code content available for external open");
        }
    });
}

fn wire_pdf_preview_callbacks(window: &MainWindow, _ctx: &AppContext) {
    use crate::bin_ui::preview_state;

    // Close PDF preview
    window.on_close_pdf_preview(|| {
        preview_state::clear_pdf_preview();
        tracing::debug!("PDF preview closed");
    });

    // Open PDF in external viewer - uses Save As for secure export
    window.on_open_pdf_external(|| {
        if let Some(content) = preview_state::get_pdf_preview_content() {
            // Show Save As dialog then open with system handler
            let filename = content.filename.clone();
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save PDF File")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match std::fs::write(&destination, &data) {
                        Ok(()) => {
                            tracing::info!("Exported PDF to {}", destination.display());
                            let opener = ExternalOpener;
                            if let Err(e) = opener.open_file(&destination) {
                                tracing::warn!("Could not open exported file: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to save PDF file: {}", e),
                    }
                }
            });
        } else {
            tracing::warn!("No PDF content available for external open");
        }
    });
}

fn wire_video_player_callbacks(window: &MainWindow) {
    // Close video player
    let weak_window = window.as_weak();
    window.on_close_video_player(move || {
        video_state::stop_video();
        stop_video_progress_timer();
        if let Some(w) = weak_window.upgrade() {
            w.set_video_player_visible(false);
        }
        tracing::debug!("Video player closed");
    });

    // Toggle play/pause
    let weak_window = window.as_weak();
    window.on_video_player_toggle_play_pause(move || {
        video_state::toggle_play_pause();
        if let Some((_, _, is_playing)) = video_state::get_playback_state() {
            if let Some(w) = weak_window.upgrade() {
                w.set_video_player_is_playing(is_playing);
            }
        }
    });

    // Seek
    window.on_video_player_seek(|position| {
        video_state::seek(position);
    });

    // Set volume
    window.on_video_player_set_volume(|volume| {
        video_state::set_volume(volume);
    });
}

fn wire_message_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_message_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("Message clicked: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::trace!("Message selected: {}", message_id_str);
            }
        });
    });
}

fn wire_reply_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_reply_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

        tokio::spawn(async move {
            // Parse sender_id and timestamp for Signal-style lookup
            let target_sender: chatter_linux::domain::ids::UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid sender ID for reply");
                    return;
                }
            };
            let target_timestamp: u64 = match timestamp_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid timestamp for reply");
                    return;
                }
            };

            let reply_preview = match message_repo.get_by_sender_timestamp(&target_sender, target_timestamp) {
                Ok(Some(msg)) => {
                    let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                    let content = msg.as_text().unwrap_or("").to_string();
                    let preview = if content.chars().count() > 50 {
                        let truncated: String = content.chars().take(47).collect();
                        format!("{}...", truncated)
                    } else {
                        content
                    };
                    format!("Reply to {}: {}", sender, preview)
                }
                _ => "Reply to message".to_string(),
            };

            let placeholder = reply_preview.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_placeholder(placeholder.into());
                    tracing::info!(sender = %sender_id_str, timestamp = %timestamp_str, "Reply mode activated");
                }
            });
        });
    });
}

fn wire_react_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_react_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        tracing::debug!(sender = %sender_id_str, timestamp = %timestamp_str, "React clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!(sender = %sender_id_str, timestamp = %timestamp_str, "Emoji picker requested");
            }
        });
    });
}

fn wire_thread_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();
        let channel_repo = channel_repo.clone();

        let msg_id: MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for thread: {}", e);
                return;
            }
        };

        let channel_id = match chat_state.get().active_channel_id {
            Some(id) => id,
            None => {
                tracing::warn!("Cannot open thread: no active channel");
                return;
            }
        };

        // Update state
        chat_state.open_thread(channel_id, msg_id);

        // Load thread data and update UI
        crate::bin_ui::features::thread::load_thread_data(
            weak,
            chat_state,
            message_repo,
            user_repo,
            attachment_repo,
            channel_repo,
            channel_id,
            msg_id,
        );
    });
}

// wire_more_clicked moved to chat.rs (shows context menu with message info)

fn wire_view_attachment(window: &MainWindow, ctx: &AppContext) {
    let attachment_repo = ctx.attachment_repo.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let weak_window = window.as_weak();

    window.on_view_attachment(move |attachment_id: SharedString| {
        let attachment_id_str = attachment_id.to_string();
        let attachment_repo = attachment_repo.clone();
        let attachment_repo_for_download = attachment_repo.clone();
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

        // Look up attachment from repository
        let attachment = match attachment_repo.get(&att_id) {
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
        let content_in_database = attachment.content_location == ContentLocation::InDatabase;
        let needs_download = attachment.content_location == ContentLocation::NotDownloaded
            && attachment.remote_url.is_some()
            && attachment.encryption_key.is_some();

        // Skip if already downloading
        if attachment.status == AttachmentStatus::Downloading {
            tracing::debug!("Attachment {} already downloading, skipping", att_id);
            return;
        }

        // SECURE IN-MEMORY PLAYBACK: Content is in database, decrypt to memory
        if content_in_database && attachment.status == AttachmentStatus::Ready {
            tracing::debug!("Attachment {} ready in DB, spawning async decryption", att_id);

            let filename = attachment.metadata.name.clone();
            let file_size = attachment.metadata.size;
            let mime_type = attachment.metadata.mime_type.clone();
            let attachment_repo_for_decrypt = attachment_repo.clone();
            let att_id_for_decrypt = att_id;

            // OPTIMIZATION: For audio, check if already playing BEFORE decrypting
            // This avoids expensive decrypt for simple pause/resume toggles
            let is_audio = mime_type.starts_with("audio/");
            if is_audio {
                let currently_playing = audio_state::get_current_playing_id();
                if currently_playing.as_deref() == Some(attachment_id.as_str()) {
                    // Already playing this audio - just toggle pause (no decrypt needed!)
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
                    return; // Done - no need to decrypt
                }
            }

            // Show loading state immediately for images
            let is_image_check = FileMetadata::is_image_file(&filename, &mime_type);
            if is_image_check {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_image_viewer_filename("Loading...".into());
                        w.set_image_viewer_size("Decrypting...".into());
                        w.set_image_viewer_visible(true);
                    }
                });
            }

            // Spawn async task - don't block UI thread
            let weak_for_task = weak.clone();
            let attachment_id_for_task = attachment_id.clone();
            tokio::spawn(async move {
                let attachment_service = AttachmentService::new(attachment_repo_for_decrypt);

                // Decrypt in blocking task (CPU-bound)
                let secure_buffer = match tokio::task::spawn_blocking(move || {
                    attachment_service.decrypt_to_memory(&att_id_for_decrypt)
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

                let weak = weak_for_task;
                let attachment_id = attachment_id_for_task;
                let is_audio = mime_type.starts_with("audio/");
                let is_image = FileMetadata::is_image_file(&filename, &mime_type);
                let is_video = mime_type.starts_with("video/");

                if is_audio {
                    // Load and play new audio from memory
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
                                            let weak_for_timer = weak.clone();
                                            let att_id_for_ui = attachment_id.to_string();
                                            let _ = slint::invoke_from_event_loop(move || {
                                                if let Some(w) = weak_for_timer.upgrade() {
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
                } else if is_image {
                    // Load image from memory (secure: no filesystem)
                let mut data = secure_buffer.as_slice().to_vec();
                let filename_for_ui = filename.clone();
                let size_display = format_file_size(file_size);
                let att_id_for_inline = att_id.to_string();

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        // Check if it's an animated GIF - use combined function for
                        // viewer animation AND inline animation (shared decode)
                        let weak_for_gif = w.as_weak();
                        if let Some(first_frame) = crate::bin_ui::gif_state::start_gif_animation_and_inline(
                            &data,
                            &weak_for_gif,
                            &att_id_for_inline,
                        ) {
                            // Animated GIF - use the first frame and start animation
                            w.set_image_viewer_source(first_frame);
                            w.set_image_viewer_filename(filename_for_ui.into());
                            w.set_image_viewer_size(size_display.into());
                            w.set_image_viewer_visible(true);
                            crate::bin_ui::gif_state::resume();
                            tracing::info!("Opened animated GIF in viewer");
                        } else {
                            // Static image - use standard image loading
                            match ImageProcessor::memory_to_slint_image(&data) {
                                Ok(img) => {
                                    w.set_image_viewer_source(img);
                                    w.set_image_viewer_filename(filename_for_ui.into());
                                    w.set_image_viewer_size(size_display.into());
                                    w.set_image_viewer_visible(true);
                                    tracing::info!("Opened image from memory in viewer");
                                }
                                Err(e) => {
                                    tracing::error!("Failed to load image from memory: {:?}", e);
                                }
                            }
                        }
                    }
                    // SECURITY: Zeroize decrypted data after use
                    data.zeroize();
                    });
                } else if is_video {
                // Play video from memory using memfd (secure: no filesystem)
                let data = secure_buffer.as_slice().to_vec();
                let filename_for_video = filename.clone();

                match video_state::start_video_from_memory(data, &filename_for_video) {
                    Ok((_, duration_str)) => {
                        let weak_for_video = weak.clone();
                        let filename_for_ui = filename_for_video.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_video.upgrade() {
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
                } else {
                    // For code/text files, decode and display from memory
                if FileMetadata::is_code_file(&filename, &mime_type) {
                    let data = secure_buffer.as_slice().to_vec();
                    let text = String::from_utf8_lossy(&data).to_string();
                    let filename_for_ui = filename.clone();
                    let filename_for_state = filename.clone();

                    // Store content for "open external" feature
                    preview_state::set_code_preview_content(filename_for_state, data);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Simple line-based display for in-memory text
                            let lines: Vec<CodeLine> = text
                                .lines()
                                .enumerate()
                                .map(|(i, line)| CodeLine {
                                    line_number: (i + 1) as i32,
                                    content: line.into(),
                                })
                                .collect();

                            let line_count = lines.len() as i32;
                            w.set_code_preview_filename(filename_for_ui.into());
                            w.set_code_preview_language("text".into());
                            w.set_code_preview_line_count(line_count);
                            w.set_code_preview_lines(ModelRc::new(VecModel::from(lines)));
                            w.set_code_preview_visible(true);
                        }
                    });
                } else if mime_type == "application/pdf" || filename.to_lowercase().ends_with(".pdf") {
                    // Show PDF preview from memory
                    let data = secure_buffer.as_slice().to_vec();
                    let filename_for_ui = filename.clone();
                    let filename_for_state = filename.clone();

                    // Store content for "open external" feature
                    preview_state::set_pdf_preview_content(filename_for_state, data);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_pdf_preview_filename(filename_for_ui.into());
                            w.set_pdf_preview_page_count(0); // Page count not available without parsing
                            w.set_pdf_preview_error("".into());
                            w.set_pdf_preview_visible(true);
                            tracing::info!("Opened PDF preview from memory");
                        }
                    });
                } else {
                    // For other files, show Save As dialog
                    let data = secure_buffer.as_slice().to_vec();
                    let filename_for_save = filename.clone();
                    let att_id_for_save = att_id;

                    tokio::spawn(async move {
                        // Show Save As dialog
                        let config = FilePickerConfig::new()
                            .with_title("Save Attachment As")
                            .with_default_path(
                                dirs::download_dir()
                                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                                    .join(&filename_for_save),
                            );

                        if let Some(destination) = FilePicker::save_file(config).await {
                            // Write decrypted data to chosen location
                            match std::fs::write(&destination, &data) {
                                Ok(()) => {
                                    tracing::info!(
                                        "Exported attachment {} to {}",
                                        att_id_for_save,
                                        destination.display()
                                    );

                                    // Open the saved file with system handler
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
                }
                // secure_buffer is dropped here -> memory zeroized
            });
            // Async decryption spawned - return early
            return;
        }

        // SECURE DOWNLOAD: Download encrypted content to database (no disk)
        if needs_download {
            // Mark as downloading immediately to prevent duplicate downloads
            let mut att_to_update = attachment.clone();
            att_to_update.status = AttachmentStatus::Downloading;
            if let Err(e) = attachment_repo.update(&att_to_update) {
                tracing::warn!("Failed to mark attachment as downloading: {}", e);
            }

            let cdn_key = attachment.remote_url.clone().unwrap();
            let file_name = attachment.metadata.name.clone();
            let mime_type = attachment.metadata.mime_type.clone();
            let file_size = attachment.metadata.size;
            let att_id_for_download = att_id;
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

            // Spawn async download task - downloads encrypted content to DB, NOT disk
            let weak_for_completion = weak.clone();
            let att_id_str_for_task = att_id_str.clone();
            tokio::spawn(async move {
                let attachment_service = AttachmentService::new(attachment_repo_for_download.clone());

                // Download encrypted ciphertext to database (secure: no decryption yet)
                match attachment_service.download_to_database(&grpc_pool, &att_id_for_download).await {
                    Ok(()) => {
                        tracing::info!(
                            cdn_key = %cdn_key,
                            attachment_id = %att_id_for_download,
                            "Encrypted content stored in database (secure)"
                        );

                        // Update UI to clear downloading state
                        let weak_for_done = weak_for_completion.clone();
                        let att_id_for_done = att_id_str_for_task.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = weak_for_done.upgrade() {
                                update_attachment_transfer_status(&window, &att_id_for_done, false, false, 1.0);
                            }
                        });

                        // Now decrypt to memory and play
                        let secure_buffer = match attachment_service.decrypt_to_memory(&att_id_for_download) {
                            Ok(buffer) => buffer,
                            Err(e) => {
                                tracing::error!("Failed to decrypt downloaded attachment: {}", e);
                                return;
                            }
                        };

                        let is_audio = mime_type.starts_with("audio/");
                        let is_image = FileMetadata::is_image_file(&file_name, &mime_type);

                        if is_audio {
                            // Play audio from memory
                            let player_arc = audio_state::get_audio_player();
                            if let Ok(mut player_guard) = player_arc.lock() {
                                if let Some(ref mut player) = *player_guard {
                                    let data = secure_buffer.as_slice().to_vec();
                                    match player.load_from_memory(data) {
                                        Ok(duration) => {
                                            tracing::info!("Loaded audio from memory (duration: {:?})", duration);
                                            if let Err(e) = player.play() {
                                                tracing::error!("Failed to play audio: {}", e);
                                            } else {
                                                audio_state::set_playing_with_duration(&att_id_str, duration);
                                                let att_id_for_ui = att_id_str_for_task.clone();
                                                let weak_for_audio = weak.clone();
                                                let _ = slint::invoke_from_event_loop(move || {
                                                    if let Some(w) = weak_for_audio.upgrade() {
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
                                }
                            }
                        } else if is_image {
                            // Show image from memory
                            let data = secure_buffer.as_slice().to_vec();
                            let filename_for_ui = file_name.clone();
                            let size_display = format_file_size(file_size);
                            let att_id_for_inline = att_id_str_for_task.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Check if it's an animated GIF
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
                                        tracing::info!("Opened animated GIF from download");
                                    } else {
                                        match ImageProcessor::memory_to_slint_image(&data) {
                                            Ok(img) => {
                                                w.set_image_viewer_source(img);
                                                w.set_image_viewer_filename(filename_for_ui.into());
                                                w.set_image_viewer_size(size_display.into());
                                                w.set_image_viewer_visible(true);
                                                tracing::info!("Opened image from memory");
                                            }
                                            Err(e) => {
                                                tracing::error!("Failed to load image from memory: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            });
                        } else if FileMetadata::is_code_file(&file_name, &mime_type) {
                            // Show text/code from memory
                            let data = secure_buffer.as_slice().to_vec();
                            let text = String::from_utf8_lossy(&data).to_string();
                            let filename_for_ui = file_name.clone();
                            let filename_for_state = file_name.clone();

                            // Store content for "open external" feature
                            preview_state::set_code_preview_content(filename_for_state, data);

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    let lines: Vec<CodeLine> = text
                                        .lines()
                                        .enumerate()
                                        .map(|(i, line)| CodeLine {
                                            line_number: (i + 1) as i32,
                                            content: line.into(),
                                        })
                                        .collect();

                                    let line_count = lines.len() as i32;
                                    w.set_code_preview_filename(filename_for_ui.into());
                                    w.set_code_preview_language("text".into());
                                    w.set_code_preview_line_count(line_count);
                                    w.set_code_preview_lines(ModelRc::new(VecModel::from(lines)));
                                    w.set_code_preview_visible(true);
                                }
                            });
                        } else if mime_type.starts_with("video/") {
                            // Play video from memory
                            let data = secure_buffer.as_slice().to_vec();
                            let filename_for_video = file_name.clone();

                            match video_state::start_video_from_memory(data, &filename_for_video) {
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
                                        }
                                    });
                                }
                                Err(e) => {
                                    tracing::error!("Failed to start video from memory: {}", e);
                                }
                            }
                        } else if mime_type == "application/pdf" || file_name.to_lowercase().ends_with(".pdf") {
                            // Show PDF preview from memory
                            let data = secure_buffer.as_slice().to_vec();
                            let filename_for_ui = file_name.clone();
                            let filename_for_state = file_name.clone();

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
                        } else {
                            // For other files (office docs, etc.), show Save As dialog
                            let data = secure_buffer.as_slice().to_vec();
                            let filename_for_save = file_name.clone();
                            let att_id_for_save = att_id_for_download;

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
                                            tracing::info!(
                                                "Exported attachment {} to {}",
                                                att_id_for_save,
                                                destination.display()
                                            );
                                            // Open the saved file
                                            let opener = ExternalOpener;
                                            if let Err(e) = opener.open_file(&destination) {
                                                tracing::warn!("Could not open exported file: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("Failed to save attachment: {}", e);
                                        }
                                    }
                                }
                            });
                        }
                        // secure_buffer dropped here -> memory zeroized
                    }
                    Err(e) => {
                        tracing::error!(
                            cdn_key = %cdn_key,
                            error = %e,
                            "Failed to download attachment to database"
                        );

                        // Mark as failed so it can be retried
                        if let Ok(Some(mut att)) = attachment_repo_for_download.get(&att_id_for_download) {
                            att.status = AttachmentStatus::Failed;
                            let _ = attachment_repo_for_download.update(&att);
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
            return; // Download is async
        }

        // Legacy fallback: Handle old attachments that have local_path (pre-migration)
        // This code path will be removed once all attachments are migrated to in-database storage
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

fn wire_audio_seek(window: &MainWindow, _ctx: &AppContext) {
    window.on_audio_seek(move |attachment_id: SharedString, position: f32| {
        let attachment_id_str = attachment_id.to_string();

        tracing::debug!("Audio seek: {} to position {:.2}", attachment_id_str, position);

        // Check if this attachment is currently playing
        let current_playing = audio_state::get_current_playing_id();
        if current_playing.as_deref() != Some(attachment_id.as_str()) {
            tracing::debug!("Seek ignored: {} is not currently playing", attachment_id_str);
            return;
        }

        // Get duration from audio state
        let duration = match audio_state::get_current_duration() {
            Some(d) => d,
            None => {
                tracing::warn!("No duration available for seeking");
                return;
            }
        };

        // Calculate seek position
        let seek_ms = (position as f64 * duration.as_millis() as f64) as u64;
        let seek_duration = Duration::from_millis(seek_ms);

        tracing::debug!("Seeking to {:?} (position {})", seek_duration, position);

        // Run seek in background thread to avoid blocking UI
        // rodio's skip_duration decodes linearly which is CPU-intensive
        tokio::task::spawn_blocking(move || {
            let player_arc = audio_state::get_audio_player();
            match player_arc.lock() {
                Ok(mut player_guard) => {
                    if let Some(ref mut player) = *player_guard {
                        // We need to reload and seek because rodio doesn't support native seek
                        // The AudioPlayer.seek() method handles this internally
                        if let Err(e) = player.seek(seek_duration) {
                            tracing::error!("Failed to seek audio: {}", e);
                        } else {
                            // Update position tracking in audio_state
                            audio_state::set_seek_position(seek_duration);
                            tracing::info!("Audio seek successful to {:?}", seek_duration);
                        }
                    } else {
                        tracing::error!("Audio player not initialized");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to lock audio player: {}", e);
                }
            }
        });
    });
}
