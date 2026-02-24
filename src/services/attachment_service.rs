//! Attachment service.
//!
//! Upload, download, share, and encrypt attachments.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::crypto::attachment_crypto::{AttachmentCrypto, KEY_SET_LENGTH};
use crate::crypto::secure_buffer::SecureBuffer;
use crate::domain::attachment::ContentLocation;
use crate::domain::attachment::{Attachment, AttachmentStatus, FileMetadata};
use crate::domain::ids::{AttachmentId, ChannelId};
use crate::network::grpc::file_client::FileClient;
use crate::network::grpc::GrpcClientPool;
use crate::proto::chat_v1::FileChunk;
use crate::proto::content_v1::AttachmentPointer;
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

    /// Encryption error.
    #[error("encryption error: {0}")]
    EncryptionError(String),

    /// Decryption error.
    #[error("decryption error: {0}")]
    DecryptionError(String),
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

/// Result of encrypted CDN upload for E2E attachments.
///
/// Contains all data needed to build an AttachmentPointer for the E2E message.
pub struct CdnUploadResult {
    /// Local attachment ID (stored in database).
    pub attachment_id: AttachmentId,
    /// CDN key for downloading the encrypted blob.
    pub cdn_key: String,
    /// Encryption key (64 bytes: AES key + HMAC key).
    pub encryption_key: [u8; KEY_SET_LENGTH],
    /// SHA-256 digest of the ciphertext.
    pub digest: [u8; 32],
    /// Original plaintext size (before padding).
    pub plaintext_size: u64,
    /// MIME type.
    pub content_type: String,
    /// Original filename.
    pub file_name: String,
    /// Audio waveform (32 samples, 0-255) - only for audio files.
    pub waveform: Vec<u8>,
    /// Duration in milliseconds - only for audio/video.
    pub duration_ms: u64,
    /// Thumbnail JPEG bytes - only for images/videos.
    pub thumbnail: Vec<u8>,
    /// Thumbnail width.
    pub thumbnail_width: u32,
    /// Thumbnail height.
    pub thumbnail_height: u32,
    /// Original image/video dimensions.
    pub width: u32,
    pub height: u32,
}

impl CdnUploadResult {
    /// Convert to AttachmentPointer proto.
    pub fn to_attachment_pointer(&self) -> AttachmentPointer {
        tracing::debug!(
            "Creating AttachmentPointer: cdn_key={}, content_type={}, \
             thumbnail_bytes={}, thumbnail_size={}x{}, width={}, height={}, waveform_len={}",
            self.cdn_key,
            self.content_type,
            self.thumbnail.len(),
            self.thumbnail_width,
            self.thumbnail_height,
            self.width,
            self.height,
            self.waveform.len()
        );
        AttachmentPointer {
            cdn_key: self.cdn_key.clone(),
            encryption_key: self.encryption_key.to_vec(),
            digest: self.digest.to_vec(),
            plaintext_size: self.plaintext_size,
            content_type: self.content_type.clone(),
            file_name: self.file_name.clone(),
            waveform: self.waveform.clone(),
            duration_ms: self.duration_ms,
            thumbnail: self.thumbnail.clone(),
            thumbnail_width: self.thumbnail_width,
            thumbnail_height: self.thumbnail_height,
            width: self.width,
            height: self.height,
            ..Default::default()
        }
    }
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

        // Extract waveform and duration for audio files
        // Must use spawn_blocking because FFmpeg is synchronous and doesn't work well on Tokio threads
        let (waveform_samples, duration_ms) = if mime_type.starts_with("audio/") {
            let waveform_start = std::time::Instant::now();
            let path_for_waveform = file_path.to_path_buf();
            let waveform = match tokio::task::spawn_blocking(move || {
                crate::media::extract_waveform(&path_for_waveform)
            }).await {
                Ok(Ok(samples)) => {
                    tracing::info!(
                        "Extracted waveform for audio: {} samples, took {:?}",
                        samples.len(),
                        waveform_start.elapsed()
                    );
                    Some(samples)
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to extract waveform: {}", e);
                    None
                }
                Err(e) => {
                    tracing::warn!("Waveform extraction task panicked: {}", e);
                    None
                }
            };
            let path_for_duration = file_path.to_path_buf();
            let duration = tokio::task::spawn_blocking(move || {
                crate::media::AudioPlayer::get_duration(&path_for_duration)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }).await.unwrap_or(0);
            tracing::info!("Audio duration: {}ms", duration);
            (waveform, duration)
        } else {
            (None, 0)
        };

        // Generate thumbnail and get dimensions for images and videos
        let (thumbnail, thumbnail_width, thumbnail_height, width, height, video_duration_ms) =
            if mime_type.starts_with("image/") {
                // Image processing can run on Tokio thread (uses image crate, not FFmpeg)
                let (orig_w, orig_h) = crate::media::ImageProcessor::get_dimensions(file_path)
                    .unwrap_or((0, 0));
                let thumb_start = std::time::Instant::now();
                match crate::media::ImageProcessor::generate_thumbnail_bytes(file_path) {
                    Ok((bytes, thumb_w, thumb_h)) => {
                        tracing::info!(
                            "Generated AVIF thumbnail for image: {}x{} -> {}x{}, {} bytes, took {:?}",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len(), thumb_start.elapsed()
                        );
                        (Some(bytes), Some(thumb_w), Some(thumb_h), Some(orig_w), Some(orig_h), 0u64)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to generate thumbnail: {}", e);
                        (None, None, None, Some(orig_w), Some(orig_h), 0u64)
                    }
                }
            } else if mime_type.starts_with("video/") {
                // Video thumbnail uses FFmpeg - must use spawn_blocking
                let thumb_start = std::time::Instant::now();
                let path_for_thumb = file_path.to_path_buf();
                match tokio::task::spawn_blocking(move || {
                    crate::media::extract_video_thumbnail(&path_for_thumb)
                }).await {
                    Ok(Ok((bytes, thumb_w, thumb_h, vid_duration_ms))) => {
                        let path_for_dims = file_path.to_path_buf();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims)
                                .unwrap_or((thumb_w, thumb_h))
                        }).await.unwrap_or((thumb_w, thumb_h));
                        tracing::info!(
                            "Generated AVIF thumbnail for video: {}x{} -> {}x{}, {} bytes, duration {}ms, took {:?}",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len(), vid_duration_ms, thumb_start.elapsed()
                        );
                        (Some(bytes), Some(thumb_w), Some(thumb_h), Some(orig_w), Some(orig_h), vid_duration_ms)
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to generate video thumbnail: {}", e);
                        let path_for_dims = file_path.to_path_buf();
                        let path_for_dur = file_path.to_path_buf();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims).unwrap_or((0, 0))
                        }).await.unwrap_or((0, 0));
                        let vid_duration = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_duration(&path_for_dur).unwrap_or(0)
                        }).await.unwrap_or(0);
                        (None, None, None, Some(orig_w), Some(orig_h), vid_duration)
                    }
                    Err(e) => {
                        tracing::warn!("Video thumbnail task panicked: {}", e);
                        (None, None, None, None, None, 0u64)
                    }
                }
            } else {
                (None, None, None, None, None, 0u64)
            };

        // Use video duration if available
        let final_duration_ms = if video_duration_ms > 0 { video_duration_ms } else { duration_ms };

        // Create domain Attachment and save to repository
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: file_size,
            sha256: file_metadata.sha256.clone(),
            chunk_size: file_metadata.chunk_size,
            width,
            height,
            duration_seconds: if final_duration_ms > 0 { Some((final_duration_ms / 1000) as f64) } else { None },
            waveform_samples,
            thumbnail,
            thumbnail_width,
            thumbnail_height,
        };

        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::NotDownloaded, // Legacy: local file, not in DB
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

        // Extract waveform and duration for audio files
        // Must use spawn_blocking because FFmpeg is synchronous and doesn't work well on Tokio threads
        let (waveform_samples, duration_ms) = if mime_type.starts_with("audio/") {
            let path_for_waveform = local_path.clone();
            let waveform = match tokio::task::spawn_blocking(move || {
                crate::media::extract_waveform(&path_for_waveform)
            }).await {
                Ok(Ok(samples)) => {
                    tracing::debug!("Extracted waveform for downloaded audio: {} samples", samples.len());
                    Some(samples)
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to extract waveform: {}", e);
                    None
                }
                Err(e) => {
                    tracing::warn!("Waveform extraction task panicked: {}", e);
                    None
                }
            };
            let path_for_duration = local_path.clone();
            let duration = tokio::task::spawn_blocking(move || {
                crate::media::AudioPlayer::get_duration(&path_for_duration)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }).await.unwrap_or(0);
            (waveform, duration)
        } else {
            (None, 0)
        };

        // Generate thumbnail and get dimensions for images and videos
        let (thumbnail, thumbnail_width, thumbnail_height, width, height, video_duration_ms) =
            if mime_type.starts_with("image/") {
                // Image processing can run on Tokio thread (uses image crate, not FFmpeg)
                let (orig_w, orig_h) = crate::media::ImageProcessor::get_dimensions(&local_path)
                    .unwrap_or((0, 0));
                match crate::media::ImageProcessor::generate_thumbnail_bytes(&local_path) {
                    Ok((bytes, thumb_w, thumb_h)) => {
                        tracing::debug!(
                            "Generated thumbnail for downloaded image: {}x{} -> {}x{}, {} bytes",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len()
                        );
                        (Some(bytes), Some(thumb_w), Some(thumb_h), Some(orig_w), Some(orig_h), 0u64)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to generate thumbnail: {}", e);
                        (None, None, None, Some(orig_w), Some(orig_h), 0u64)
                    }
                }
            } else if mime_type.starts_with("video/") {
                // Video processing uses FFmpeg - must use spawn_blocking
                let path_for_thumb = local_path.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::media::extract_video_thumbnail(&path_for_thumb)
                }).await {
                    Ok(Ok((bytes, thumb_w, thumb_h, vid_duration_ms))) => {
                        let path_for_dims = local_path.clone();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims)
                                .unwrap_or((thumb_w, thumb_h))
                        }).await.unwrap_or((thumb_w, thumb_h));
                        tracing::debug!(
                            "Generated thumbnail for downloaded video: {}x{} -> {}x{}, {} bytes, {}ms",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len(), vid_duration_ms
                        );
                        (Some(bytes), Some(thumb_w), Some(thumb_h), Some(orig_w), Some(orig_h), vid_duration_ms)
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to generate video thumbnail: {}", e);
                        let path_for_dims = local_path.clone();
                        let path_for_dur = local_path.clone();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims).unwrap_or((0, 0))
                        }).await.unwrap_or((0, 0));
                        let vid_duration = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_duration(&path_for_dur).unwrap_or(0)
                        }).await.unwrap_or(0);
                        (None, None, None, Some(orig_w), Some(orig_h), vid_duration)
                    }
                    Err(e) => {
                        tracing::warn!("Video thumbnail task panicked: {}", e);
                        (None, None, None, None, None, 0u64)
                    }
                }
            } else {
                (None, None, None, None, None, 0u64)
            };

        // Use video duration if available
        let final_duration_ms = if video_duration_ms > 0 { video_duration_ms } else { duration_ms };

        // Update repository with local path
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: actual_size,
            sha256: expected_hash,
            chunk_size: CHUNK_SIZE as u32,
            width,
            height,
            duration_seconds: if final_duration_ms > 0 { Some((final_duration_ms / 1000) as f64) } else { None },
            waveform_samples,
            thumbnail,
            thumbnail_width,
            thumbnail_height,
        };

        let attachment = Attachment {
            id: *attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::NotDownloaded, // Legacy: local file, not in DB
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

    // ==================== Encrypted CDN Operations ====================

    /// Upload a file to CDN with client-side encryption.
    ///
    /// This is the Signal-style flow for E2E encrypted attachments:
    /// 1. Read file content
    /// 2. Encrypt with AES-256-CBC + HMAC-SHA256
    /// 3. Get presigned upload URL from server
    /// 4. Upload encrypted ciphertext via HTTP PUT
    /// 5. Confirm upload with server
    ///
    /// Returns a `CdnUploadResult` with all data needed for AttachmentPointer.
    pub async fn upload_to_cdn_encrypted(
        &self,
        grpc_pool: &GrpcClientPool,
        file_path: &Path,
    ) -> Result<CdnUploadResult, AttachmentError> {
        // Read file content
        let file_content = tokio::fs::read(file_path)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to read file: {}", e)))?;

        let plaintext_size = file_content.len() as u64;
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        let mime_type = mime_guess::from_path(file_path)
            .first_or_octet_stream()
            .to_string();

        tracing::debug!(
            filename = %filename,
            plaintext_size = plaintext_size,
            mime_type = %mime_type,
            "Encrypting attachment for CDN upload"
        );

        // Encrypt with Signal-compatible encryption
        let encrypt_start = std::time::Instant::now();
        let encrypted = AttachmentCrypto::encrypt(&file_content)
            .map_err(|e| AttachmentError::EncryptionError(e.to_string()))?;

        let encrypted_size = encrypted.ciphertext.len() as u64;
        tracing::info!(
            encrypted_size = encrypted_size,
            elapsed = ?encrypt_start.elapsed(),
            "Attachment encrypted"
        );

        // Get presigned upload URL
        let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
        let upload_response = file_client
            .get_attachment_upload_url(encrypted_size)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get upload URL: {}", e)))?;

        let cdn_key = upload_response.cdn_key;
        let upload_url = upload_response.upload_url;

        tracing::debug!(
            cdn_key = %cdn_key,
            "Got presigned URL, uploading encrypted content"
        );

        // Upload encrypted ciphertext via HTTP PUT
        let upload_start = std::time::Instant::now();
        let http_client = reqwest::Client::new();
        let response = http_client
            .put(&upload_url)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", encrypted_size)
            .body(encrypted.ciphertext.clone())
            .send()
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("HTTP upload failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(format!(
                "CDN upload failed with status: {}",
                response.status()
            )));
        }

        tracing::info!(cdn_key = %cdn_key, elapsed = ?upload_start.elapsed(), "CDN upload complete");

        // Confirm upload with server
        file_client
            .confirm_attachment_upload(&cdn_key)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to confirm upload: {}", e)))?;

        // Generate deterministic attachment ID from cdn_key for consistent lookups
        // This ensures sender and receiver use the same ID for the same attachment
        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

        // Extract waveform and duration for audio files
        // Must use spawn_blocking because FFmpeg is synchronous and doesn't work well on Tokio threads
        let (waveform_samples, duration_ms) = if mime_type.starts_with("audio/") {
            let path_for_waveform = file_path.to_path_buf();
            let waveform = match tokio::task::spawn_blocking(move || {
                crate::media::extract_waveform(&path_for_waveform)
            }).await {
                Ok(Ok(samples)) => {
                    // Log waveform stats for debugging
                    let min = samples.iter().min().copied().unwrap_or(0);
                    let max = samples.iter().max().copied().unwrap_or(0);
                    let avg: f32 = if samples.is_empty() { 0.0 } else { samples.iter().map(|&s| s as f32).sum::<f32>() / samples.len() as f32 };
                    tracing::info!(
                        "Extracted waveform for CDN audio: {} samples, min={}, max={}, avg={:.1}, first_5={:?}",
                        samples.len(), min, max, avg, &samples[..5.min(samples.len())]
                    );
                    Some(samples)
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to extract waveform: {}", e);
                    None
                }
                Err(e) => {
                    tracing::warn!("Waveform extraction task panicked: {}", e);
                    None
                }
            };
            // Extract duration using audio player
            let path_for_duration = file_path.to_path_buf();
            let duration = tokio::task::spawn_blocking(move || {
                crate::media::AudioPlayer::get_duration(&path_for_duration)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }).await.unwrap_or(0);
            (waveform, duration)
        } else {
            (None, 0)
        };

        // Generate thumbnail and get dimensions for images and videos
        let (thumbnail, thumbnail_width, thumbnail_height, width, height, video_duration_ms) =
            if mime_type.starts_with("image/") {
                // Image processing can run on Tokio thread (uses image crate, not FFmpeg)
                let (orig_w, orig_h) = crate::media::ImageProcessor::get_dimensions(file_path)
                    .unwrap_or((0, 0));

                // Generate thumbnail bytes
                match crate::media::ImageProcessor::generate_thumbnail_bytes(file_path) {
                    Ok((bytes, thumb_w, thumb_h)) => {
                        tracing::debug!(
                            "Generated thumbnail for image: {}x{} -> {}x{}, {} bytes",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len()
                        );
                        (bytes, thumb_w, thumb_h, orig_w, orig_h, 0u64)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to generate thumbnail: {}", e);
                        (Vec::new(), 0, 0, orig_w, orig_h, 0u64)
                    }
                }
            } else if mime_type.starts_with("video/") {
                // Video processing uses FFmpeg - must use spawn_blocking
                let path_for_thumb = file_path.to_path_buf();
                match tokio::task::spawn_blocking(move || {
                    crate::media::extract_video_thumbnail(&path_for_thumb)
                }).await {
                    Ok(Ok((bytes, thumb_w, thumb_h, vid_duration_ms))) => {
                        // Get original dimensions
                        let path_for_dims = file_path.to_path_buf();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims)
                                .unwrap_or((thumb_w, thumb_h))
                        }).await.unwrap_or((thumb_w, thumb_h));
                        tracing::debug!(
                            "Generated thumbnail for video: {}x{} -> {}x{}, {} bytes, {}ms",
                            orig_w, orig_h, thumb_w, thumb_h, bytes.len(), vid_duration_ms
                        );
                        (bytes, thumb_w, thumb_h, orig_w, orig_h, vid_duration_ms)
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to generate video thumbnail: {}", e);
                        // Try to get at least dimensions and duration
                        let path_for_dims = file_path.to_path_buf();
                        let path_for_dur = file_path.to_path_buf();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims).unwrap_or((0, 0))
                        }).await.unwrap_or((0, 0));
                        let vid_duration = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_duration(&path_for_dur).unwrap_or(0)
                        }).await.unwrap_or(0);
                        (Vec::new(), 0, 0, orig_w, orig_h, vid_duration)
                    }
                    Err(e) => {
                        tracing::warn!("Video thumbnail task panicked: {}", e);
                        (Vec::new(), 0, 0, 0, 0, 0u64)
                    }
                }
            } else {
                (Vec::new(), 0, 0, 0, 0, 0u64)
            };

        // Use video duration if available (for video files)
        let final_duration_ms = if video_duration_ms > 0 { video_duration_ms } else { duration_ms };

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: plaintext_size,
            sha256: encrypted.digest.to_vec(), // Use ciphertext digest
            chunk_size: 0,
            width: if width > 0 { Some(width) } else { None },
            height: if height > 0 { Some(height) } else { None },
            duration_seconds: if final_duration_ms > 0 { Some((final_duration_ms / 1000) as f64) } else { None },
            waveform_samples: waveform_samples.clone(),
            thumbnail: if !thumbnail.is_empty() { Some(thumbnail.clone()) } else { None },
            thumbnail_width: if thumbnail_width > 0 { Some(thumbnail_width) } else { None },
            thumbnail_height: if thumbnail_height > 0 { Some(thumbnail_height) } else { None },
        };

        // Create attachment record with content stored in database
        // NOTE: We do NOT store local_path - encrypted content goes in DB only
        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::InDatabase, // Content stored in DB
            local_path: None, // No filesystem path - secure in-memory only
            remote_url: Some(cdn_key.clone()), // Store cdn_key for re-download if needed
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()), // Store digest for verification
            created_at: chrono::Utc::now(),
        };

        // Save to repository WITH encrypted content
        // This stores the ciphertext directly in the database for secure playback
        if let Err(e) = self.attachment_repo.insert_with_content(&attachment, &encrypted.ciphertext) {
            tracing::warn!("Failed to save CDN attachment with content: {}", e);
            // Fallback to insert without content (can be re-downloaded)
            if let Err(e2) = self.attachment_repo.insert(&attachment) {
                tracing::warn!("Fallback insert also failed: {}", e2);
            }
        } else {
            tracing::debug!(
                "Saved CDN attachment {} with {} bytes encrypted content to database",
                attachment_id,
                encrypted.ciphertext.len()
            );
        }

        tracing::info!(
            cdn_key = %cdn_key,
            attachment_id = %attachment_id,
            filename = %filename,
            plaintext_size = plaintext_size,
            encrypted_size = encrypted_size,
            "Encrypted attachment uploaded to CDN and stored locally"
        );

        Ok(CdnUploadResult {
            attachment_id,
            cdn_key,
            encryption_key: encrypted.key,
            digest: encrypted.digest,
            plaintext_size,
            content_type: mime_type,
            file_name: filename,
            waveform: waveform_samples.unwrap_or_default(),
            duration_ms: final_duration_ms,
            thumbnail,
            thumbnail_width,
            thumbnail_height,
            width,
            height,
        })
    }

    /// **DEPRECATED**: Use `download_to_database` + `decrypt_to_memory` instead.
    ///
    /// This method saves decrypted content to the filesystem, which is insecure.
    /// The new secure flow:
    /// 1. `download_to_database()` - stores encrypted content in DB
    /// 2. `decrypt_to_memory()` - returns SecureBuffer that auto-zeroizes
    ///
    /// This legacy method is kept only for backward compatibility with tests.
    #[deprecated(
        since = "0.2.0",
        note = "Use download_to_database + decrypt_to_memory for secure in-memory handling"
    )]
    #[allow(dead_code)]
    pub async fn download_from_cdn_encrypted(
        &self,
        grpc_pool: &GrpcClientPool,
        pointer: &AttachmentPointer,
        download_dir: &Path,
    ) -> Result<DownloadResult, AttachmentError> {
        // Validate pointer has required fields
        if pointer.cdn_key.is_empty() {
            return Err(AttachmentError::InvalidAttachment("cdn_key is empty".to_string()));
        }
        if pointer.encryption_key.len() != KEY_SET_LENGTH {
            return Err(AttachmentError::InvalidAttachment(format!(
                "encryption_key must be {} bytes, got {}",
                KEY_SET_LENGTH,
                pointer.encryption_key.len()
            )));
        }
        if pointer.digest.len() != 32 {
            return Err(AttachmentError::InvalidAttachment(format!(
                "digest must be 32 bytes, got {}",
                pointer.digest.len()
            )));
        }

        tracing::debug!(
            cdn_key = %pointer.cdn_key,
            filename = %pointer.file_name,
            plaintext_size = pointer.plaintext_size,
            "Downloading encrypted attachment from CDN"
        );

        // Get presigned download URL
        let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
        let download_response = file_client
            .get_attachment_download_url(&pointer.cdn_key)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get download URL: {}", e)))?;

        let download_url = download_response.download_url;

        // Download encrypted ciphertext via HTTP GET
        let http_client = reqwest::Client::new();
        let response = http_client
            .get(&download_url)
            .send()
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("HTTP download failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(format!(
                "CDN download failed with status: {}",
                response.status()
            )));
        }

        let ciphertext = response
            .bytes()
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to read response: {}", e)))?
            .to_vec();

        tracing::debug!(
            cdn_key = %pointer.cdn_key,
            ciphertext_size = ciphertext.len(),
            "Downloaded ciphertext, decrypting"
        );

        // Convert key and digest to fixed-size arrays
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(&pointer.encryption_key);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&pointer.digest);

        // Decrypt
        let plaintext = AttachmentCrypto::decrypt(
            &ciphertext,
            &key,
            &digest,
            pointer.plaintext_size as usize,
        )
        .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        // Determine filename
        let filename = if pointer.file_name.is_empty() {
            format!("attachment-{}", pointer.cdn_key)
        } else {
            pointer.file_name.clone()
        };

        // Ensure download directory exists
        tokio::fs::create_dir_all(download_dir)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to create download dir: {}", e)))?;

        // Save decrypted file
        let local_path = download_dir.join(&filename);
        tokio::fs::write(&local_path, &plaintext)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

        tracing::info!(
            cdn_key = %pointer.cdn_key,
            filename = %filename,
            plaintext_size = plaintext.len(),
            local_path = %local_path.display(),
            "Decrypted attachment saved"
        );

        let mime_type = if pointer.content_type.is_empty() {
            mime_guess::from_path(&local_path)
                .first_or_octet_stream()
                .to_string()
        } else {
            pointer.content_type.clone()
        };

        Ok(DownloadResult {
            local_path,
            filename,
            file_size: plaintext.len() as u64,
            mime_type,
        })
    }

    // ==================== Secure In-Memory Operations ====================

    /// Download encrypted content from CDN and store in database.
    ///
    /// Downloads the ciphertext and stores it directly in the database
    /// WITHOUT decrypting. Decryption happens only during playback
    /// via `decrypt_to_memory()`.
    ///
    /// This is the secure flow: encrypted content stored in SQLite,
    /// decrypted only to memory during playback, then zeroized.
    pub async fn download_to_database(
        &self,
        grpc_pool: &GrpcClientPool,
        attachment_id: &AttachmentId,
    ) -> Result<(), AttachmentError> {
        // Get attachment with CDN key
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!("Attachment {} not found", attachment_id))
            })?;

        let cdn_key = attachment.remote_url.as_ref().ok_or_else(|| {
            AttachmentError::InvalidAttachment("Attachment has no CDN key".to_string())
        })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            cdn_key = %cdn_key,
            "Downloading encrypted content to database"
        );

        // Get presigned download URL
        let mut file_client =
            crate::network::grpc::file_client::FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
        let download_response = file_client
            .get_attachment_download_url(cdn_key)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get download URL: {}", e)))?;

        // Download encrypted ciphertext via HTTP GET
        let http_client = reqwest::Client::new();
        let response = http_client
            .get(&download_response.download_url)
            .send()
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("HTTP download failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(format!(
                "CDN download failed with status: {}",
                response.status()
            )));
        }

        let ciphertext = response
            .bytes()
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to read response: {}", e)))?
            .to_vec();

        tracing::debug!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext.len(),
            "Downloaded ciphertext, storing in database"
        );

        // Store encrypted content in database (NOT decrypted)
        self.attachment_repo
            .store_encrypted_content(attachment_id, &ciphertext)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?;

        // Update attachment status
        let mut updated = attachment;
        updated.status = AttachmentStatus::Ready;
        updated.content_location = ContentLocation::InDatabase;
        self.attachment_repo
            .update(&updated)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?;

        tracing::info!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext.len(),
            "Encrypted content stored in database"
        );

        Ok(())
    }

    /// Decrypt attachment content to memory.
    ///
    /// Reads encrypted content from database and decrypts it to a
    /// `SecureBuffer` that auto-zeroizes when dropped.
    ///
    /// **Security**: The decrypted plaintext exists ONLY in memory
    /// and is automatically wiped when the buffer goes out of scope.
    pub fn decrypt_to_memory(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        // Get attachment metadata
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!("Attachment {} not found", attachment_id))
            })?;

        // Check if content is in database
        if attachment.content_location != ContentLocation::InDatabase {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not in database - download first".to_string(),
            ));
        }

        // Get encryption keys
        let encryption_key = attachment.encryption_key.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No encryption key".to_string())
        })?;
        let digest = attachment.encryption_iv.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No digest for verification".to_string())
        })?;

        if encryption_key.len() != KEY_SET_LENGTH {
            return Err(AttachmentError::DecryptionError(format!(
                "Invalid key length: expected {}, got {}",
                KEY_SET_LENGTH,
                encryption_key.len()
            )));
        }

        // Get encrypted content from database
        let ciphertext = self
            .attachment_repo
            .get_encrypted_content(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment("Encrypted content not found in database".to_string())
            })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            ciphertext_len = ciphertext.len(),
            plaintext_size = attachment.metadata.size,
            "Decrypting attachment to memory"
        );

        // Convert keys to fixed-size arrays
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(encryption_key);
        let mut digest_arr = [0u8; 32];
        if digest.len() >= 32 {
            digest_arr.copy_from_slice(&digest[..32]);
        } else {
            digest_arr[..digest.len()].copy_from_slice(digest);
        }

        // Decrypt to memory
        let plaintext = AttachmentCrypto::decrypt(
            &ciphertext,
            &key,
            &digest_arr,
            attachment.metadata.size as usize,
        )
        .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        tracing::info!(
            attachment_id = %attachment_id,
            plaintext_len = plaintext.len(),
            "Decrypted attachment to secure buffer"
        );

        Ok(SecureBuffer::new(plaintext))
    }

    /// Check if attachment content is available locally (in database).
    pub fn has_local_content(&self, attachment_id: &AttachmentId) -> bool {
        self.attachment_repo
            .get(attachment_id)
            .ok()
            .flatten()
            .is_some_and(|a| a.content_location == ContentLocation::InDatabase)
    }

    /// Get attachment metadata without loading content.
    pub fn get_metadata(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Option<Attachment>, AttachmentError> {
        self.attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
    }

    /// Export a decrypted attachment to a file.
    ///
    /// Decrypts the attachment content to memory and writes it to the
    /// specified path. Used for "Save As" functionality.
    ///
    /// # Security Note
    ///
    /// This is the only method that writes decrypted content to disk.
    /// It should only be called when the user explicitly requests to
    /// export a file via "Save As".
    pub fn export_to_file(
        &self,
        attachment_id: &AttachmentId,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        // Decrypt to memory
        let secure_buffer = self.decrypt_to_memory(attachment_id)?;

        // Write to destination file
        std::fs::write(destination, secure_buffer.as_slice())
            .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

        tracing::info!(
            attachment_id = %attachment_id,
            destination = %destination.display(),
            size = secure_buffer.len(),
            "Exported attachment to file"
        );

        Ok(())
        // secure_buffer dropped here -> memory zeroized
    }

    /// Export a decrypted attachment asynchronously.
    ///
    /// Same as export_to_file but runs the file write in a blocking task.
    pub async fn export_to_file_async(
        &self,
        attachment_id: &AttachmentId,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        // Decrypt to memory (sync operation on DB)
        let secure_buffer = self.decrypt_to_memory(attachment_id)?;
        let dest = destination.to_path_buf();
        let att_id = *attachment_id;

        // Write to file in blocking task
        tokio::task::spawn_blocking(move || {
            std::fs::write(&dest, secure_buffer.as_slice())
                .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

            tracing::info!(
                attachment_id = %att_id,
                destination = %dest.display(),
                "Exported attachment to file"
            );

            Ok::<(), AttachmentError>(())
        })
        .await
        .map_err(|e| AttachmentError::FileError(format!("Export task failed: {}", e)))??;

        Ok(())
    }
}
