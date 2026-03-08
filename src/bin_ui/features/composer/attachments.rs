//! Attachment handling for the composer.

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::state::ChatState;

use crate::bin_ui::converters::pending_attachment_to_slint;
use crate::{MainWindow, PendingAttachmentData};

pub(crate) fn wire_attachment_clicked(window: &MainWindow, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        tracing::debug!("Attachment clicked");

        // IMPORTANT: Capture UI state BEFORE spawning async task
        // Slint windows can only be accessed from the main UI thread
        let has_active_channel = chat_state.get().active_channel_id.is_some();
        let (selected_dm_id, selected_private_group_id) = weak.upgrade()
            .map(|w| (
                w.get_selected_dm_id().to_string(),
                w.get_selected_private_group_id().to_string(),
            ))
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

        // Use tokio::spawn for the file picker (async operation)
        tokio::spawn(async move {
            use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};
            use chatter_linux::state::chat_state::PendingAttachment;

            let config = FilePickerConfig::new()
                .with_title("Select Attachment")
                .with_filter(FileFilter::all())  // Default to all files
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

            // Queue files for upload (upload happens at send time with encryption)
            // This matches the DM flow - files are uploaded when message is sent
            // Add and display each file one by one for visual feedback
            for file_path in &selected_files {
                // Get file metadata for display
                let metadata = match tokio::fs::metadata(file_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!("Failed to read file metadata {:?}: {}", file_path, e);
                        continue;
                    }
                };

                let filename = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                let mime_type = mime_guess::from_path(file_path)
                    .first_or_octet_stream()
                    .to_string();

                // Generate temporary ID for UI tracking (real ID assigned at upload time)
                let temp_id = AttachmentId::new();

                tracing::debug!(
                    "Queued attachment: {} ({} bytes, {})",
                    filename, metadata.len(), mime_type
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

                // Update UI immediately after each file is added (one-by-one appearance)
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

            tracing::info!(
                "Added {} file(s) to pending attachments",
                selected_files.len()
            );
        });
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
                let attachments = chat_state.get_pending_attachments();
                let weak = weak_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
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
            Err(e) => {
                tracing::error!("Failed to parse attachment ID '{}': {}", id_str, e);
            }
        }
    });
}
