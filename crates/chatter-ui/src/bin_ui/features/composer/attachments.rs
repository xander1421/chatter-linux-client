//! Attachment handling for the composer.

use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use chatter_core::domain::ids::AttachmentId;
use chatter_core::state::chat_state::PendingAttachment;
use chatter_core::state::{ActiveConversation, ChatState};

use crate::bin_ui::converters::pending_attachment_to_slint;
use crate::{MainWindow, PendingAttachmentData};

/// Process a single file and add it to pending attachments.
/// Used by both file picker and drag/drop.
pub(crate) async fn process_and_add_file(
    file_path: PathBuf,
    chat_state: &ChatState,
    weak: &Weak<MainWindow>,
) -> Result<(), String> {
    // Get file metadata for display
    let metadata = tokio::fs::metadata(&file_path)
        .await
        .map_err(|e| format!("Failed to read file metadata {:?}: {}", file_path, e))?;

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mime_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    // Generate temporary ID for UI tracking (real ID assigned at upload time)
    let temp_id = AttachmentId::new();

    tracing::debug!(
        "Queued attachment: {} ({} bytes, {})",
        filename,
        metadata.len(),
        mime_type
    );

    // Add to pending attachments for composer UI
    // File will be encrypted and uploaded when message is sent
    let pending = PendingAttachment::new(
        temp_id,
        filename,
        file_path.to_path_buf(),
        metadata.len(),
        mime_type,
    );
    chat_state.add_pending_attachment(pending);

    // Update UI immediately
    update_attachments_ui(chat_state, weak);

    Ok(())
}

/// Update the UI with current pending attachments.
pub(crate) fn update_attachments_ui(chat_state: &ChatState, weak: &Weak<MainWindow>) {
    let attachments = chat_state.get_pending_attachments();
    let weak_update = weak.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_update.upgrade() {
            let slint_attachments: Vec<PendingAttachmentData> = attachments
                .iter()
                .map(pending_attachment_to_slint)
                .collect();

            let model = ModelRc::new(VecModel::from(slint_attachments));
            w.set_pending_attachments(model);

            // Update placeholder text
            let pending_count = attachments.len();
            let placeholder = if pending_count == 0 {
                "Type a message...".to_string()
            } else if pending_count == 1 {
                "1 file attached - type a message...".to_string()
            } else {
                format!("{} files attached - type a message...", pending_count)
            };
            w.set_composer_placeholder(placeholder.into());
        }
    });
}

pub(crate) fn wire_attachment_clicked(
    window: &MainWindow,
    chat_state: ChatState,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    let weak_window = window.as_weak();
    // Only the mesh interception below consumes this; without the feature the
    // whole path is the ordinary staging one.
    #[cfg(feature = "mesh")]
    let ui_ctx_attach = ui_ctx.clone();
    #[cfg(not(feature = "mesh"))]
    let _ = ui_ctx;

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        #[cfg(feature = "mesh")]
        let ui_ctx_attach = ui_ctx_attach.clone();
        tracing::debug!("Attachment clicked");

        // Before the active-conversation check below, not after: a mesh room or
        // cell is not an `ActiveConversation` at all — that enum is the
        // server-side conversation — so the `None` early return would swallow
        // the tap and the picker would never open. `intercept_send` sits in the
        // same position in the send path, for the same reason.
        //
        // A mesh file is fragmented onto the radio, never uploaded, and never
        // staged: staging exists for a later send with text attached.
        #[cfg(feature = "mesh")]
        if crate::bin_ui::features::mesh::intercept_attachment(&ui_ctx_attach, weak.clone()) {
            return;
        }

        // Check if we have an active conversation BEFORE opening file picker
        let conversation = chat_state.active_conversation();

        tracing::info!(
            conversation = ?conversation,
            "Attachment clicked - checking context"
        );

        if matches!(conversation, ActiveConversation::None) {
            tracing::warn!("Cannot upload: no active conversation selected");
            return;
        }

        #[cfg(not(target_os = "android"))]
        {
            tokio::spawn(async move {
                use chatter_core::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

                let config = FilePickerConfig::new()
                    .with_title("Select Attachment")
                    .with_filter(FileFilter::all()) // Default to all files
                    .with_filter(FileFilter::images())
                    .with_filter(FileFilter::videos())
                    .with_filter(FileFilter::audio())
                    .with_filter(FileFilter::documents())
                    .with_multiple(true);

                let selected_files = FilePicker::pick_files(config).await;

                if selected_files.is_empty() {
                    tracing::debug!("File picker cancelled or no files selected");
                    return;
                }

                tracing::info!("Selected {} file(s) for attachment", selected_files.len());

                for file_path in selected_files {
                    if let Err(e) = process_and_add_file(file_path, &chat_state, &weak).await {
                        tracing::error!("{}", e);
                    }
                }

                tracing::info!("Added file(s) to pending attachments");
            });
        }
        #[cfg(target_os = "android")]
        {
            tokio::spawn(async move {
                // Storage Access Framework (no permission needed). "*/*" lets the
                // user pick any file; images/videos/audio/docs all qualify.
                let selected_files =
                    crate::platform::android::file_picker::pick_files(&["*/*"]).await;

                if selected_files.is_empty() {
                    tracing::debug!("File picker cancelled or no files selected");
                    return;
                }

                tracing::info!("Selected {} file(s) for attachment", selected_files.len());

                for file_path in selected_files {
                    if let Err(e) = process_and_add_file(file_path, &chat_state, &weak).await {
                        tracing::error!("{}", e);
                    }
                }
            });
        }
    });
}

/// Wire the paste-image callback: on Ctrl/Cmd+V, attach an image from the
/// clipboard (if any) as a pending attachment. Text paste is handled by the
/// TextInput itself; this only fires the image path.
#[cfg(not(target_os = "android"))]
pub(crate) fn wire_paste_image(window: &MainWindow, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_paste_image(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();

        // No point reading the clipboard if there's nowhere to attach.
        if matches!(chat_state.active_conversation(), ActiveConversation::None) {
            return;
        }

        tokio::spawn(async move {
            // arboard is blocking — keep it off the async runtime threads.
            let path = match tokio::task::spawn_blocking(read_clipboard_image).await {
                Ok(Ok(path)) => path,
                Ok(Err(e)) => {
                    // Common, benign case: clipboard holds text, not an image.
                    tracing::debug!("Clipboard paste: no image to attach ({e})");
                    return;
                }
                Err(e) => {
                    tracing::error!("Clipboard read task failed: {e}");
                    return;
                }
            };

            match process_and_add_file(path, &chat_state, &weak).await {
                Ok(()) => tracing::info!("Attached image pasted from clipboard"),
                Err(e) => tracing::error!("{e}"),
            }
        });
    });
}

/// Read an image off the clipboard and write it to a temp PNG, returning its
/// path so it can flow through the normal attachment pipeline. Errors (incl.
/// "no image on clipboard") are returned as strings for the caller to log.
#[cfg(not(target_os = "android"))]
fn read_clipboard_image() -> Result<PathBuf, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = clipboard.get_image().map_err(|e| e.to_string())?;

    let width = img.width as u32;
    let height = img.height as u32;
    // arboard yields raw RGBA8; rebuild an image buffer to re-encode as PNG.
    let buffer = image::RgbaImage::from_raw(width, height, img.bytes.into_owned())
        .ok_or_else(|| "clipboard image dimensions don't match buffer".to_string())?;

    let path = std::env::temp_dir().join(format!("chatter-paste-{}.png", AttachmentId::new()));
    buffer
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| format!("failed to encode pasted image: {e}"))?;

    Ok(path)
}

/// Wire up the remove-attachment callback.
pub(crate) fn wire_remove_attachment(window: &MainWindow, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_remove_attachment(move |attachment_id: SharedString| {
        let id_str = attachment_id.to_string();
        tracing::debug!("Remove attachment clicked: {}", id_str);

        // Parse the attachment ID
        let parsed_id: Result<AttachmentId, _> = id_str.parse();
        match parsed_id {
            Ok(id) => {
                chat_state.remove_pending_attachment(id);
                tracing::info!("Removed pending attachment: {}", id_str);

                // Refresh the UI
                update_attachments_ui(&chat_state, &weak_window);
            }
            Err(e) => {
                tracing::error!("Failed to parse attachment ID '{}': {}", id_str, e);
            }
        }
    });
}
