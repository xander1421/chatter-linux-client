//! Attachment handling for the composer.

use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::state::chat_state::PendingAttachment;
use chatter_linux::state::ChatState;

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

pub(crate) fn wire_attachment_clicked(window: &MainWindow, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        tracing::debug!("Attachment clicked");

        // IMPORTANT: Capture UI state BEFORE spawning async task
        // Slint windows can only be accessed from the main UI thread
        let has_active_channel = chat_state.get().active_channel_id.is_some();
        let (selected_dm_id, selected_private_group_id) = weak
            .upgrade()
            .map(|w| {
                (
                    w.get_selected_dm_id().to_string(),
                    w.get_selected_private_group_id().to_string(),
                )
            })
            .unwrap_or_default();

        let has_active_dm = !selected_dm_id.is_empty();
        let has_active_private_group = !selected_private_group_id.is_empty();

        tracing::info!(
            has_active_channel = has_active_channel,
            has_active_dm = has_active_dm,
            selected_dm_id = %selected_dm_id,
            has_active_private_group = has_active_private_group,
            selected_private_group_id = %selected_private_group_id,
            "Attachment clicked - checking context"
        );

        // Check if we have an active conversation BEFORE opening file picker
        if !has_active_channel && !has_active_dm && !has_active_private_group {
            tracing::warn!("Cannot upload: no active conversation selected");
            return;
        }

        #[cfg(not(target_os = "android"))]
        {
            // Use tokio::spawn for the file picker (async operation)
            tokio::spawn(async move {
                use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

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

                // Process each file
                for file_path in selected_files {
                    if let Err(e) = process_and_add_file(file_path, &chat_state, &weak).await {
                        tracing::error!("{}", e);
                    }
                }

                tracing::info!(
                    "Added file(s) to pending attachments",
                );
            });
        }
        #[cfg(target_os = "android")]
        tracing::warn!("File picker not yet supported on Android");
    });
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
