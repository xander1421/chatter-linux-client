//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions).

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::attachment::{Attachment, AttachmentStatus, FileMetadata};
use chatter_linux::domain::ids::{AttachmentId, ChannelId};
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::network::grpc::file_client::FileClient;
use chatter_linux::state::{ChatState, PendingAttachment};
use chatter_linux::storage::AttachmentRepository;

use crate::bin_ui::converters::pending_attachment_to_slint;
use crate::{MainWindow, PendingAttachmentData};

/// Wire composer callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_attachment_clicked(
        window,
        ctx.grpc_pool.clone(),
        ctx.chat_state.clone(),
        ctx.attachment_repo.clone(),
    );
    wire_remove_attachment(window, ctx.chat_state.clone());
    wire_emoji_clicked(window);
    wire_gif_clicked(window);
    wire_mention_triggered(window);
}

fn wire_attachment_clicked(
    window: &MainWindow,
    grpc_pool: Arc<GrpcClientPool>,
    chat_state: ChatState,
    attachment_repo: Arc<AttachmentRepository>,
) {
    let weak_window = window.as_weak();

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let chat_state = chat_state.clone();
        let attachment_repo = attachment_repo.clone();
        tracing::debug!("Attachment clicked");

        // Use tokio::spawn for the main async work to keep the runtime alive
        tokio::spawn(async move {
            use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

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

            // Get active channel ID for upload permission
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot upload: no active channel selected");
                    return;
                }
            };

            // Upload each file
            let mut uploaded_count = 0;
            for file_path in &selected_files {
                tracing::debug!("Uploading file: {:?}", file_path);

                match upload_file(&grpc_pool, file_path, &chat_state, &channel_id, &attachment_repo).await {
                    Ok(()) => {
                        uploaded_count += 1;
                        tracing::info!("Successfully uploaded: {:?}", file_path);
                    }
                    Err(e) => {
                        tracing::error!("Failed to upload {:?}: {}", file_path, e);
                    }
                }
            }

            // Update UI with attachment model
            let attachments = chat_state.get_pending_attachments();
            tracing::debug!(
                "Updating UI with {} pending attachments",
                attachments.len()
            );
            for att in &attachments {
                tracing::debug!(
                    "  - {} ({} bytes, {})",
                    att.filename,
                    att.size,
                    att.mime_type
                );
            }

            let weak_final = weak.clone();
            match slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_final.upgrade() {
                    let slint_attachments: Vec<PendingAttachmentData> = attachments
                        .iter()
                        .map(pending_attachment_to_slint)
                        .collect();

                    tracing::debug!(
                        "Setting {} attachments to UI model",
                        slint_attachments.len()
                    );

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
                    tracing::debug!("UI updated with attachments");
                } else {
                    tracing::warn!("Failed to upgrade window weak reference");
                }
            }) {
                Ok(_) => tracing::debug!("invoke_from_event_loop queued successfully"),
                Err(e) => tracing::error!("invoke_from_event_loop failed: {:?}", e),
            }

            if uploaded_count < selected_files.len() {
                tracing::warn!(
                    "Only uploaded {} of {} selected files",
                    uploaded_count,
                    selected_files.len()
                );
            }
        });
    });
}

/// Wire up the remove-attachment callback.
fn wire_remove_attachment(window: &MainWindow, chat_state: ChatState) {
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

/// Upload a single file and add it to pending attachments.
///
/// Uses streaming upload with channel-id header for permission checking.
/// Follows the canonical tonic pattern: tokio_stream::iter(vec) for client streaming.
async fn upload_file(
    grpc_pool: &GrpcClientPool,
    file_path: &std::path::Path,
    chat_state: &ChatState,
    channel_id: &ChannelId,
    attachment_repo: &AttachmentRepository,
) -> Result<(), String> {
    use chatter_linux::proto::chat_v1::FileChunk;
    use sha2::{Digest, Sha256};

    // Read file metadata
    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| format!("Failed to read file metadata: {}", e))?;

    let file_size = metadata.len();
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Determine MIME type
    let mime_type = mime_guess::from_path(file_path)
        .first_or_octet_stream()
        .to_string();

    tracing::debug!(
        "Uploading {} ({} bytes, {}) to channel {}",
        filename,
        file_size,
        mime_type,
        channel_id
    );

    // Read file content
    let file_content = tokio::fs::read(file_path)
        .await
        .map_err(|e| format!("Failed to read file: {}", e))?;

    // Generate a transfer_id UUID for this upload session
    let transfer_id = uuid::Uuid::new_v4().to_string();
    tracing::debug!("Starting upload with transfer_id={}", transfer_id);

    // Create file client
    let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    // Create chunks with consistent transfer_id
    const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks

    let chunks: Vec<FileChunk> = file_content
        .chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(seq, chunk_data)| {
            // Compute Poly1305-like auth tag (16 bytes from SHA256)
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            let hash = hasher.finalize();

            FileChunk {
                transfer_id: transfer_id.clone(),
                sequence: seq as u32,
                data: chunk_data.to_vec(),
                auth_tag: hash[..16].to_vec(), // Use first 16 bytes as Poly1305-sized tag
            }
        })
        .collect();

    let chunk_count = chunks.len();
    let total_data_size: usize = chunks.iter().map(|c| c.data.len()).sum();
    tracing::info!(
        "Prepared {} chunks ({} bytes total, avg {} bytes/chunk) for {}",
        chunk_count,
        total_data_size,
        total_data_size / chunk_count.max(1),
        filename
    );

    // Create a logging wrapper stream to track chunk transmission
    let chunks_with_logging: Vec<FileChunk> = chunks;
    let chunk_stream = tokio_stream::iter(chunks_with_logging.into_iter().enumerate().map(|(i, chunk)| {
        tracing::trace!(
            chunk_seq = i,
            chunk_size = chunk.data.len(),
            transfer_id = %chunk.transfer_id,
            "Sending chunk"
        );
        chunk
    }));

    tracing::debug!("Starting upload request to server...");

    // Upload with channel-id header for permission checking
    let upload_result = file_client
        .upload_file_with_metadata(
            &channel_id.to_string(),
            &filename,
            &mime_type,
            file_size,
            chunk_stream,
        )
        .await;

    // Handle the result - h2 errors can occur even when upload succeeds
    let file_metadata = match upload_result {
        Ok(metadata) => {
            tracing::debug!("Upload request completed successfully");
            metadata
        }
        Err(e) => {
            let error_str = format!("{}", e);
            let error_debug = format!("{:?}", e);

            tracing::error!(
                error = %error_str,
                error_debug = %error_debug,
                filename = %filename,
                file_size = %file_size,
                chunk_count = %chunk_count,
                "Upload request failed"
            );

            // Check if this is an h2 protocol error - the upload may have succeeded
            if error_str.contains("h2 protocol error") || error_str.contains("http2 error") {
                tracing::warn!(
                    "Got h2 protocol error - this typically occurs when the server closes \
                     the stream before the client reads the response. Verifying if upload \
                     actually succeeded for: {}",
                    filename
                );

                // Verify upload by checking channel files
                let mut verify_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
                match verify_client.list_channel_files(&channel_id.to_string(), 20, 0).await {
                    Ok(response) => {
                        // Look for our file by name and size in recent uploads
                        if let Some(uploaded_file) = response.files.iter().find(|f| {
                            f.name == filename && f.size == file_size
                        }) {
                            tracing::info!(
                                "Upload verified successful despite h2 error: {} (id={})",
                                filename,
                                uploaded_file.id
                            );
                            // Clone the metadata we need
                            chatter_linux::proto::chat_v1::FileMetadata {
                                id: uploaded_file.id.clone(),
                                name: uploaded_file.name.clone(),
                                mime_type: uploaded_file.mime_type.clone(),
                                size: uploaded_file.size,
                                sha256: uploaded_file.sha256.clone(),
                                chunk_size: uploaded_file.chunk_size,
                            }
                        } else {
                            tracing::error!("Upload verification failed - file not found in channel");
                            return Err(format!("Upload failed (h2 error, file not found): {}", e));
                        }
                    }
                    Err(verify_err) => {
                        tracing::error!("Failed to verify upload: {}", verify_err);
                        return Err(format!("Upload failed (h2 error, verification failed): {}", e));
                    }
                }
            } else {
                return Err(format!("Upload failed: {}", e));
            }
        }
    };

    // Parse attachment ID from returned file metadata
    let attachment_id: AttachmentId = file_metadata
        .id
        .parse()
        .map_err(|e| format!("Invalid attachment ID '{}': {}", file_metadata.id, e))?;

    // Create domain Attachment and save to repository for later retrieval
    let domain_metadata = FileMetadata {
        name: filename.clone(),
        mime_type: mime_type.clone(),
        size: file_size,
        sha256: file_metadata.sha256.clone(),
        chunk_size: file_metadata.chunk_size,
        width: None,  // TODO: Extract from image metadata if applicable
        height: None,
        duration_seconds: None,
    };

    let attachment = Attachment {
        id: attachment_id,
        metadata: domain_metadata,
        status: AttachmentStatus::Ready,
        local_path: Some(file_path.to_string_lossy().to_string()),
        remote_url: None, // Could be constructed from server URL if needed
        encryption_key: None,
        encryption_iv: None,
        created_at: chrono::Utc::now(),
    };

    // Save to repository so it can be resolved when displaying messages
    if let Err(e) = attachment_repo.insert(&attachment) {
        tracing::warn!("Failed to save attachment to repository: {} (will still show in pending)", e);
    } else {
        tracing::debug!("Saved attachment {} to repository", attachment_id);
    }

    // Add to pending attachments (for composer UI)
    let pending = PendingAttachment::new(
        attachment_id,
        filename.clone(),
        file_path.to_path_buf(),
        file_size,
        mime_type,
    );

    chat_state.add_pending_attachment(pending);

    tracing::info!(
        "File '{}' uploaded successfully, attachment_id={}",
        filename,
        file_metadata.id
    );

    Ok(())
}

fn wire_emoji_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Emoji picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Emoji picker toggled (UI component not yet implemented)");
            }
        });
    });
}

fn wire_gif_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("GIF picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("GIF picker toggled (UI component not yet implemented)");
            }
        });
    });
}

fn wire_mention_triggered(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_mention_triggered(move |query: SharedString| {
        let query_str = query.to_string();
        let weak = weak_window.clone();

        if query_str.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    tracing::trace!("Mention autocomplete closed");
                }
            });
            return;
        }

        tracing::debug!("Mention triggered with query: {}", query_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!(
                    "Mention search for '{}' (autocomplete not yet implemented)",
                    query_str
                );
            }
        });
    });
}
