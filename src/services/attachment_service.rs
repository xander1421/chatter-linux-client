//! Attachment service.
//!
//! Upload, download, share, and encrypt attachments.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::attachment::{Attachment, AttachmentStatus, FileMetadata};
use crate::domain::ids::{AttachmentId, ChannelId};
use crate::network::grpc::file_client::FileClient;
use crate::network::grpc::GrpcClientPool;
use crate::proto::chat_v1::FileChunk;
use crate::storage::repositories::AttachmentRepository;

/// Attachment service errors.
#[derive(Debug, Error)]
pub enum AttachmentError {
    /// File system error.
    #[error("file error: {0}")]
    FileError(String),

    /// Network error during upload/download.
    #[error("network error: {0}")]
    NetworkError(String),

    /// Invalid attachment data.
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),

    /// Storage error.
    #[error("storage error: {0}")]
    StorageError(String),
}

/// Result of a successful file upload.
pub struct UploadResult {
    /// The attachment ID assigned by the server.
    pub attachment_id: AttachmentId,
    /// The filename.
    pub filename: String,
    /// File size in bytes.
    pub file_size: u64,
    /// MIME type.
    pub mime_type: String,
}

/// Result of a successful file download.
pub struct DownloadResult {
    /// Local path where the file was saved.
    pub local_path: PathBuf,
    /// The filename.
    pub filename: String,
    /// File size in bytes.
    pub file_size: u64,
    /// MIME type.
    pub mime_type: String,
}

/// Attachment service for file operations.
pub struct AttachmentService {
    attachment_repo: Arc<AttachmentRepository>,
}

/// Chunk size for file uploads (256KB).
const CHUNK_SIZE: usize = 256 * 1024;

impl AttachmentService {
    /// Create a new AttachmentService.
    pub fn new(attachment_repo: Arc<AttachmentRepository>) -> Self {
        Self { attachment_repo }
    }

    /// Upload a file to a channel.
    ///
    /// Handles chunking, hashing, and streaming upload to the server.
    /// Returns the attachment details on success.
    pub async fn upload_file(
        &self,
        grpc_pool: &GrpcClientPool,
        file_path: &Path,
        channel_id: &ChannelId,
    ) -> Result<UploadResult, AttachmentError> {
        // Read file metadata
        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to read file metadata: {}", e)))?;

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
            .map_err(|e| AttachmentError::FileError(format!("Failed to read file: {}", e)))?;

        // Generate a transfer_id UUID for this upload session
        let transfer_id = uuid::Uuid::new_v4().to_string();
        tracing::debug!("Starting upload with transfer_id={}", transfer_id);

        // Create file client
        let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

        // Create chunks with auth tags
        let chunks: Vec<FileChunk> = file_content
            .chunks(CHUNK_SIZE)
            .enumerate()
            .map(|(seq, chunk_data)| {
                // Compute auth tag (16 bytes from SHA256)
                let mut hasher = Sha256::new();
                hasher.update(chunk_data);
                let hash = hasher.finalize();

                FileChunk {
                    transfer_id: transfer_id.clone(),
                    sequence: seq as u32,
                    data: chunk_data.to_vec(),
                    auth_tag: hash[..16].to_vec(),
                }
            })
            .collect();

        let chunk_count = chunks.len();
        let total_data_size: usize = chunks.iter().map(|c| c.data.len()).sum();
        tracing::info!(
            "Prepared {} chunks ({} bytes total) for {}",
            chunk_count,
            total_data_size,
            filename
        );

        // Create chunk stream with logging
        let chunk_stream = tokio_stream::iter(chunks.into_iter().enumerate().map(|(i, chunk)| {
            tracing::trace!(
                chunk_seq = i,
                chunk_size = chunk.data.len(),
                "Sending chunk"
            );
            chunk
        }));

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

                // Check if this is an h2 protocol error - the upload may have succeeded
                if error_str.contains("h2 protocol error") || error_str.contains("http2 error") {
                    tracing::warn!(
                        "Got h2 protocol error - verifying if upload succeeded for: {}",
                        filename
                    );

                    // Verify upload by checking channel files
                    self.verify_upload(grpc_pool, channel_id, &filename, file_size).await?
                } else {
                    return Err(AttachmentError::NetworkError(format!("Upload failed: {}", e)));
                }
            }
        };

        // Parse attachment ID from returned file metadata
        let attachment_id: AttachmentId = file_metadata
            .id
            .parse()
            .map_err(|e| AttachmentError::InvalidAttachment(format!("Invalid attachment ID '{}': {}", file_metadata.id, e)))?;

        // Create domain Attachment and save to repository
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: file_size,
            sha256: file_metadata.sha256.clone(),
            chunk_size: file_metadata.chunk_size,
            width: None,
            height: None,
            duration_seconds: None,
        };

        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            local_path: Some(file_path.to_string_lossy().to_string()),
            remote_url: None,
            encryption_key: None,
            encryption_iv: None,
            created_at: chrono::Utc::now(),
        };

        // Save to repository
        if let Err(e) = self.attachment_repo.insert(&attachment) {
            tracing::warn!("Failed to save attachment to repository: {}", e);
        } else {
            tracing::debug!("Saved attachment {} to repository", attachment_id);
        }

        tracing::info!(
            "File '{}' uploaded successfully, attachment_id={}",
            filename,
            attachment_id
        );

        Ok(UploadResult {
            attachment_id,
            filename,
            file_size,
            mime_type,
        })
    }

    /// Verify an upload succeeded by checking channel files.
    ///
    /// Used as fallback when h2 protocol errors occur.
    async fn verify_upload(
        &self,
        grpc_pool: &GrpcClientPool,
        channel_id: &ChannelId,
        filename: &str,
        file_size: u64,
    ) -> Result<crate::proto::chat_v1::FileMetadata, AttachmentError> {
        let mut verify_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

        let response = verify_client
            .list_channel_files(&channel_id.to_string(), 20, 0)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to verify upload: {}", e)))?;

        // Look for our file by name and size in recent uploads
        if let Some(uploaded_file) = response.files.iter().find(|f| {
            f.name == filename && f.size == file_size
        }) {
            tracing::info!(
                "Upload verified successful despite h2 error: {} (id={})",
                filename,
                uploaded_file.id
            );

            Ok(crate::proto::chat_v1::FileMetadata {
                id: uploaded_file.id.clone(),
                name: uploaded_file.name.clone(),
                mime_type: uploaded_file.mime_type.clone(),
                size: uploaded_file.size,
                sha256: uploaded_file.sha256.clone(),
                chunk_size: uploaded_file.chunk_size,
            })
        } else {
            Err(AttachmentError::NetworkError("Upload verification failed - file not found in channel".to_string()))
        }
    }

    /// Download a file from the server.
    ///
    /// Downloads the file in chunks, verifies integrity, and saves to the specified directory.
    /// Returns the local path and file metadata on success.
    ///
    /// Note: Requires the attachment metadata to be cached in the repository first
    /// (usually from receiving a message with the attachment).
    pub async fn download_file(
        &self,
        grpc_pool: &GrpcClientPool,
        attachment_id: &AttachmentId,
        download_dir: &Path,
    ) -> Result<DownloadResult, AttachmentError> {
        use tokio_stream::StreamExt;

        // Check if we already have this attachment locally
        let cached_metadata = self.attachment_repo.get(attachment_id).ok().flatten();

        if let Some(ref attachment) = cached_metadata {
            if let Some(local_path) = &attachment.local_path {
                let path = PathBuf::from(local_path);
                if path.exists() {
                    tracing::debug!(
                        "Attachment {} already exists at {:?}",
                        attachment_id,
                        path
                    );
                    return Ok(DownloadResult {
                        local_path: path,
                        filename: attachment.metadata.name.clone(),
                        file_size: attachment.metadata.size,
                        mime_type: attachment.metadata.mime_type.clone(),
                    });
                }
            }
        }

        // Get filename from cached metadata or use attachment ID as fallback
        let (filename, expected_size, mime_type, expected_hash): (String, Option<u64>, String, Vec<u8>) = match &cached_metadata {
            Some(att) => (
                att.metadata.name.clone(),
                Some(att.metadata.size),
                att.metadata.mime_type.clone(),
                att.metadata.sha256.clone(),
            ),
            None => (
                format!("{}", attachment_id),
                None,
                "application/octet-stream".to_string(),
                Vec::new(),
            ),
        };

        tracing::debug!("Downloading attachment {} ({})", attachment_id, filename);

        // Ensure download directory exists
        tokio::fs::create_dir_all(download_dir)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to create download dir: {}", e)))?;

        // Determine local file path
        let local_path = download_dir.join(&filename);

        // Create file client and download
        let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
        let attachment_id_str = attachment_id.to_string();

        let mut chunk_stream = file_client
            .download_file(&attachment_id_str)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to start download: {}", e)))?;

        // Collect all chunks
        let mut file_content = Vec::new();
        let mut hasher = Sha256::new();

        while let Some(chunk_result) = chunk_stream.next().await {
            let chunk = chunk_result
                .map_err(|e| AttachmentError::NetworkError(format!("Download stream error: {}", e)))?;
            hasher.update(&chunk.data);
            file_content.extend_from_slice(&chunk.data);
        }

        let actual_size = file_content.len() as u64;

        // Verify size if known
        if let Some(expected) = expected_size {
            if actual_size != expected {
                return Err(AttachmentError::InvalidAttachment(format!(
                    "Size mismatch: expected {} bytes, got {} bytes",
                    expected, actual_size
                )));
            }
        }

        // Verify hash if available
        if !expected_hash.is_empty() {
            let computed_hash = hasher.finalize().to_vec();
            if computed_hash != expected_hash {
                return Err(AttachmentError::InvalidAttachment(format!(
                    "Hash mismatch: expected {}, got {}",
                    hex::encode(&expected_hash),
                    hex::encode(&computed_hash)
                )));
            }
            tracing::debug!("Hash verification passed for {}", filename);
        }

        // Write to disk
        tokio::fs::write(&local_path, &file_content)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

        tracing::info!(
            "Downloaded {} ({} bytes) to {:?}",
            filename,
            actual_size,
            local_path
        );

        // Update repository with local path
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: actual_size,
            sha256: expected_hash,
            chunk_size: CHUNK_SIZE as u32,
            width: None,
            height: None,
            duration_seconds: None,
        };

        let attachment = Attachment {
            id: *attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            local_path: Some(local_path.to_string_lossy().to_string()),
            remote_url: None,
            encryption_key: None,
            encryption_iv: None,
            created_at: chrono::Utc::now(),
        };

        if let Err(e) = self.attachment_repo.insert(&attachment) {
            tracing::warn!("Failed to save downloaded attachment to repository: {}", e);
        }

        Ok(DownloadResult {
            local_path,
            filename,
            file_size: actual_size,
            mime_type,
        })
    }
}
