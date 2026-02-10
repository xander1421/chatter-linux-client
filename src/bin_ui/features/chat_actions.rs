//! Chat action callbacks.
//!
//! Handles message interactions (reply, react, thread, more, view attachment).

use std::path::Path;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::{AttachmentId, MessageId};
use chatter_linux::media::CodePreview;
use chatter_linux::state::ChatState;
use chatter_linux::storage::{AttachmentRepository, MessageRepository, UserRepository};
use chatter_linux::system::ExternalOpener;

use crate::bin_ui::audio_state;
use crate::bin_ui::converters::{format_file_size, message_to_display_data};
use crate::{CodeLine, MainWindow};

/// Wire message action callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_message_clicked(window);
    wire_reply_clicked(window, ctx);
    wire_react_clicked(window);
    wire_thread_clicked(window, ctx);
    // wire_more_clicked moved to chat.rs (shows context menu)
    wire_view_attachment(window, ctx);
    wire_close_image_viewer(window);
    wire_code_preview_callbacks(window, ctx);
    wire_pdf_preview_callbacks(window, ctx);
}

fn wire_close_image_viewer(window: &MainWindow) {
    window.on_close_image_viewer(|| {
        tracing::debug!("Image viewer closed");
    });
}

fn wire_code_preview_callbacks(window: &MainWindow, _ctx: &AppContext) {
    use crate::bin_ui::preview_state;

    // Close code preview
    window.on_close_code_preview(|| {
        preview_state::clear_code_preview_path();
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

    // Open code in external editor
    window.on_open_code_external(|| {
        if let Some(path) = preview_state::get_code_preview_path() {
            let opener = ExternalOpener;
            match opener.open_file(&path) {
                Ok(()) => tracing::info!("Opened {} in external editor", path.display()),
                Err(e) => tracing::error!("Failed to open {}: {}", path.display(), e),
            }
        } else {
            tracing::warn!("No code file path available for external open");
        }
    });
}

fn wire_pdf_preview_callbacks(window: &MainWindow, _ctx: &AppContext) {
    use crate::bin_ui::preview_state;

    // Close PDF preview
    window.on_close_pdf_preview(|| {
        preview_state::clear_pdf_preview_path();
        tracing::debug!("PDF preview closed");
    });

    // Open PDF in external viewer
    window.on_open_pdf_external(|| {
        if let Some(path) = preview_state::get_pdf_preview_path() {
            let opener = ExternalOpener;
            match opener.open_file(&path) {
                Ok(()) => tracing::info!("Opened {} in external PDF viewer", path.display()),
                Err(e) => tracing::error!("Failed to open {}: {}", path.display(), e),
            }
        } else {
            tracing::warn!("No PDF file path available for external open");
        }
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

    window.on_reply_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

        tokio::spawn(async move {
            let msg_id: MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for reply: {}", e);
                    return;
                }
            };

            let reply_preview = match message_repo.get(&msg_id) {
                Ok(Some(msg)) => {
                    let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                    let content = msg.as_text().unwrap_or("").to_string();
                    let preview = if content.len() > 50 {
                        format!("{}...", &content[..47])
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
                    tracing::info!("Reply mode activated for message: {}", msg_id);
                }
            });
        });
    });
}

fn wire_react_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_react_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("React clicked for message: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Emoji picker requested for message: {}", message_id_str);
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

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();

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
            channel_id,
            msg_id,
        );
    });
}

// wire_more_clicked moved to chat.rs (shows context menu with message info)

fn wire_view_attachment(window: &MainWindow, ctx: &AppContext) {
    let attachment_repo = ctx.attachment_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let weak_window = window.as_weak();

    window.on_view_attachment(move |attachment_id: SharedString| {
        let attachment_id_str = attachment_id.to_string();
        let attachment_repo = attachment_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo_for_refresh = attachment_repo.clone();
        let chat_state = chat_state.clone();
        let weak = weak_window.clone();

        tracing::debug!("View attachment clicked: {}", attachment_id_str);

        // Parse attachment ID
        let att_id: AttachmentId = match attachment_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid attachment ID: {}", e);
                return;
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

        // Check if this is an audio file - play with built-in player
        if attachment.metadata.is_audio() {
            if let Some(ref local_path) = attachment.local_path {
                let path = Path::new(local_path);
                if path.exists() {
                    let player_arc = audio_state::get_audio_player();
                    let state_arc = audio_state::get_audio_state();

                    if let (Ok(mut player_guard), Ok(state_guard)) = (player_arc.lock(), state_arc.lock()) {
                        if let Some(ref mut player) = *player_guard {
                            let is_same_file = state_guard.current_id.as_ref() == Some(&attachment_id_str);
                            let is_playing = state_guard.is_playing;
                            drop(state_guard); // Release lock before modifying

                            if is_same_file && is_playing {
                                // Pause currently playing audio
                                player.pause();
                                audio_state::set_paused();
                                tracing::info!("Paused audio: {}", local_path);
                            } else if is_same_file && !is_playing {
                                // Resume paused audio
                                if let Err(e) = player.play() {
                                    tracing::error!("Failed to resume audio: {}", e);
                                } else {
                                    audio_state::set_resumed();
                                    tracing::info!("Resumed audio: {}", local_path);
                                }
                            } else {
                                // Load and play new file
                                match player.load(path) {
                                    Ok(duration) => {
                                        tracing::info!(
                                            "Loaded audio {} (duration: {:?})",
                                            local_path,
                                            duration
                                        );
                                        if let Err(e) = player.play() {
                                            tracing::error!("Failed to play audio: {}", e);
                                        } else {
                                            audio_state::set_playing(&attachment_id_str);
                                            tracing::info!("Playing audio: {}", local_path);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to load audio {}: {}", local_path, e);
                                    }
                                }
                            }

                            // Refresh messages to update UI
                            refresh_messages(
                                weak.clone(),
                                chat_state.clone(),
                                message_repo.clone(),
                                user_repo.clone(),
                                attachment_repo_for_refresh.clone(),
                            );
                        } else {
                            tracing::warn!("Audio player not available");
                        }
                    }
                    return;
                }
            }
        }

        // Check if this is an image file - show in in-app viewer
        if attachment.metadata.is_image() {
            if let Some(ref local_path) = attachment.local_path {
                let path = std::path::Path::new(local_path);
                if path.exists() {
                    let filename = attachment.metadata.name.clone();
                    let size_display = format_file_size(attachment.metadata.size);
                    let path_for_load = path.to_path_buf();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Load image and show viewer
                            match slint::Image::load_from_path(&path_for_load) {
                                Ok(img) => {
                                    w.set_image_viewer_source(img);
                                    w.set_image_viewer_filename(filename.into());
                                    w.set_image_viewer_size(size_display.into());
                                    w.set_image_viewer_visible(true);
                                    tracing::info!("Opened image in viewer: {:?}", path_for_load);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to load image {:?}: {:?}", path_for_load, e);
                                }
                            }
                        }
                    });
                    return;
                }
            }
        }

        // Check if this is a code/text file - show in code preview
        if attachment.metadata.is_code() {
            if let Some(ref local_path) = attachment.local_path {
                let path = std::path::Path::new(local_path);
                if path.exists() {
                    let filename = attachment.metadata.name.clone();
                    let path_for_load = path.to_path_buf();
                    let path_for_state = path.to_path_buf();

                    // Store path for "open in external editor" functionality
                    crate::bin_ui::preview_state::set_code_preview_path(path_for_state);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Use CodePreview to get highlighted lines
                            let code_preview = CodePreview::new();
                            match code_preview.highlight_file(&path_for_load) {
                                Ok(highlighted_lines) => {
                                    // Detect language
                                    let language = code_preview
                                        .detect_language(&path_for_load)
                                        .unwrap_or_default();

                                    // Convert to Slint CodeLine format
                                    // Note: Currently CodeLine only supports line-number and content (no color)
                                    // Full syntax highlighting would require updating CodeLine struct
                                    let slint_lines: Vec<CodeLine> = highlighted_lines
                                        .iter()
                                        .map(|line| {
                                            // Concatenate all segments into a single string
                                            let content: String = line
                                                .segments
                                                .iter()
                                                .map(|seg| seg.text.as_str())
                                                .collect();
                                            CodeLine {
                                                line_number: line.line_number as i32,
                                                content: content.into(),
                                            }
                                        })
                                        .collect();

                                    let line_count = slint_lines.len() as i32;

                                    w.set_code_preview_filename(filename.into());
                                    w.set_code_preview_language(language.into());
                                    w.set_code_preview_line_count(line_count);
                                    w.set_code_preview_lines(ModelRc::new(VecModel::from(slint_lines)));
                                    w.set_code_preview_visible(true);

                                    tracing::info!(
                                        "Opened code preview: {:?} ({} lines)",
                                        path_for_load,
                                        line_count
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to highlight code {:?}: {:?}",
                                        path_for_load,
                                        e
                                    );
                                }
                            }
                        }
                    });
                    return;
                }
            }
        }

        // Check if this is a PDF file - show in PDF preview
        if attachment.metadata.is_pdf() {
            if let Some(ref local_path) = attachment.local_path {
                let path = std::path::Path::new(local_path);
                if path.exists() {
                    let filename = attachment.metadata.name.clone();
                    let local_path_clone = local_path.clone();

                    // Store path for "open in external viewer" functionality
                    crate::bin_ui::preview_state::set_pdf_preview_path(path.to_path_buf());

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // For now, we show basic PDF info
                            // Page count extraction would require a PDF library
                            w.set_pdf_preview_filename(filename.into());
                            w.set_pdf_preview_page_count(0); // Unknown without PDF library
                            w.set_pdf_preview_error(SharedString::default());
                            w.set_pdf_preview_visible(true);

                            tracing::info!("Opened PDF preview: {}", local_path_clone);
                        }
                    });
                    return;
                }
            }
        }

        // Check if this is a video file - play with built-in player (future)
        if attachment.metadata.is_video() {
            if let Some(ref local_path) = attachment.local_path {
                let path = Path::new(local_path);
                if path.exists() {
                    // For now, open with system player
                    let opener = ExternalOpener;
                    match opener.open_file(path) {
                        Ok(()) => {
                            tracing::info!("Opened video {} with system player", local_path);
                        }
                        Err(e) => {
                            tracing::error!("Failed to open video {}: {}", local_path, e);
                        }
                    }
                    return;
                }
            }
        }

        let opener = ExternalOpener;

        // Priority 1: Open local file if available
        if let Some(ref local_path) = attachment.local_path {
            let path = Path::new(local_path);
            if path.exists() {
                match opener.open_file(path) {
                    Ok(()) => {
                        tracing::info!(
                            "Opened attachment {} from local path: {}",
                            att_id,
                            local_path
                        );
                    }
                    Err(e) => {
                        tracing::error!("Failed to open file {}: {}", local_path, e);
                    }
                }
                return;
            } else {
                tracing::debug!(
                    "Local path {} doesn't exist, falling back to remote URL",
                    local_path
                );
            }
        }

        // Priority 2: Open remote URL in browser
        if let Some(ref remote_url) = attachment.remote_url {
            if !remote_url.is_empty() {
                match opener.open_url(remote_url) {
                    Ok(()) => {
                        tracing::info!(
                            "Opened attachment {} via remote URL: {}",
                            att_id,
                            remote_url
                        );
                    }
                    Err(e) => {
                        tracing::error!("Failed to open URL {}: {}", remote_url, e);
                    }
                }
                return;
            }
        }

        // No valid path or URL available
        tracing::warn!(
            "Attachment {} has no accessible local path or remote URL",
            att_id
        );
    });
}

/// Refresh the message list to reflect current audio state.
fn refresh_messages(
    weak_window: slint::Weak<MainWindow>,
    chat_state: ChatState,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    attachment_repo: Arc<AttachmentRepository>,
) {
    // Get current channel
    let channel_id = match chat_state.get().active_channel_id {
        Some(id) => id,
        None => return,
    };

    // Fetch messages
    let messages = match message_repo.list_for_channel(&channel_id, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to refresh messages: {}", e);
            return;
        }
    };

    // Convert to display data
    let mut display_messages = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sender_id) = &msg.sender_id {
            user_repo
                .get_profile(sender_id)
                .ok()
                .flatten()
                .map(|p| p.display_name)
                .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                .unwrap_or_else(|| "Unknown".to_string())
        } else {
            "Anonymous".to_string()
        };

        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
            .collect();

        display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
    }

    // Reverse to show oldest first
    display_messages.reverse();

    // Update UI
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint()).collect();
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
        }
    });
}
