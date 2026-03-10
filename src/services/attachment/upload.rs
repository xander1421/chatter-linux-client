//! Upload operations for attachments.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::crypto::attachment_crypto::AttachmentCrypto;
use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use crate::domain::ids::{AttachmentId, ChannelId};
use crate::network::grpc::file_client::FileClient;
use crate::network::grpc::GrpcClientPool;
use crate::proto::chat_v1::FileChunk;

use super::{attachment_file_path, ensure_attachments_dir, AttachmentError, AttachmentService, CdnUploadResult, UploadResult, CHUNK_SIZE};

impl AttachmentService {
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

        // Extract media metadata (waveform, thumbnail, dimensions)
        let media_meta = self.extract_media_metadata(file_path, &mime_type).await;

        // Create domain Attachment and save to repository
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: file_size,
            sha256: file_metadata.sha256.clone(),
            chunk_size: file_metadata.chunk_size,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 { Some((media_meta.duration_ms / 1000) as f64) } else { None },
            waveform_samples: media_meta.waveform,
            thumbnail: media_meta.thumbnail,
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
        };

        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::NotDownloaded,
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
    pub(crate) async fn verify_upload(
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

        // Detect MIME type: try extension first, then fall back to magic bytes
        let mime_type = mime_guess::from_path(file_path)
            .first()
            .map(|m| m.to_string())
            .unwrap_or_else(|| detect_mime_from_bytes(&file_content));

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

        // Generate deterministic attachment ID from cdn_key
        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

        // Extract media metadata
        let media_meta = self.extract_media_metadata(file_path, &mime_type).await;

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: plaintext_size,
            sha256: encrypted.digest.to_vec(),
            chunk_size: 0,
            width: if media_meta.width.is_some() { media_meta.width } else { None },
            height: if media_meta.height.is_some() { media_meta.height } else { None },
            duration_seconds: if media_meta.duration_ms > 0 { Some((media_meta.duration_ms / 1000) as f64) } else { None },
            waveform_samples: media_meta.waveform.clone(),
            thumbnail: media_meta.thumbnail.clone(),
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
        };

        // Ensure attachments directory exists
        ensure_attachments_dir()
            .map_err(|e| AttachmentError::FileError(format!("Failed to create attachments dir: {}", e)))?;

        // Write encrypted content to filesystem
        let file_path = attachment_file_path(&attachment_id);
        std::fs::write(&file_path, &encrypted.ciphertext)
            .map_err(|e| AttachmentError::FileError(format!("Failed to write attachment file: {}", e)))?;

        tracing::debug!(
            "Wrote {} bytes encrypted content to {}",
            encrypted.ciphertext.len(),
            file_path.display()
        );

        // Create attachment record with content stored on filesystem
        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            local_path: None,
            remote_url: Some(cdn_key.clone()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
        };

        // Save to repository with file path
        if let Err(e) = self.attachment_repo.insert_with_file_path(&attachment, &file_path) {
            // Clean up file on failure
            let _ = std::fs::remove_file(&file_path);
            tracing::warn!("Failed to save CDN attachment with file path: {}", e);
            return Err(AttachmentError::StorageError(e.to_string()));
        }

        tracing::debug!(
            "Saved CDN attachment {} with file path {} to database",
            attachment_id,
            file_path.display()
        );

        tracing::info!(
            cdn_key = %cdn_key,
            attachment_id = %attachment_id,
            filename = %filename,
            plaintext_size = plaintext_size,
            encrypted_size = encrypted_size,
            file_path = %file_path.display(),
            "Encrypted attachment uploaded to CDN and stored on filesystem"
        );

        Ok(CdnUploadResult {
            attachment_id,
            cdn_key,
            encryption_key: encrypted.key,
            digest: encrypted.digest,
            plaintext_size,
            content_type: mime_type,
            file_name: filename,
            waveform: media_meta.waveform.unwrap_or_default(),
            duration_ms: media_meta.duration_ms,
            thumbnail: media_meta.thumbnail.unwrap_or_default(),
            thumbnail_width: media_meta.thumbnail_width.unwrap_or(0),
            thumbnail_height: media_meta.thumbnail_height.unwrap_or(0),
            width: media_meta.width.unwrap_or(0),
            height: media_meta.height.unwrap_or(0),
        })
    }

    /// Upload bytes directly to CDN with Signal-compatible encryption.
    ///
    /// This method takes raw bytes instead of a file path, useful for:
    /// - GIFs downloaded from external CDNs (Klipy, Tenor)
    /// - Generated content that doesn't need to be written to disk first
    ///
    /// The encrypted content is stored on the local filesystem after upload.
    pub async fn upload_bytes_to_cdn_encrypted(
        &self,
        grpc_pool: &GrpcClientPool,
        content: &[u8],
        filename: &str,
        mime_type: &str,
    ) -> Result<CdnUploadResult, AttachmentError> {
        let plaintext_size = content.len() as u64;

        tracing::debug!(
            filename = %filename,
            plaintext_size = plaintext_size,
            mime_type = %mime_type,
            "Encrypting bytes for CDN upload"
        );

        // Encrypt with Signal-compatible encryption
        let encrypt_start = std::time::Instant::now();
        let encrypted = AttachmentCrypto::encrypt(content)
            .map_err(|e| AttachmentError::EncryptionError(e.to_string()))?;

        let encrypted_size = encrypted.ciphertext.len() as u64;
        tracing::info!(
            encrypted_size = encrypted_size,
            elapsed = ?encrypt_start.elapsed(),
            "Bytes encrypted"
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

        // Generate deterministic attachment ID from cdn_key
        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

        // Extract media metadata from bytes (for GIFs)
        let media_meta = self.extract_media_metadata_from_bytes(content, mime_type);

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.to_string(),
            mime_type: mime_type.to_string(),
            size: plaintext_size,
            sha256: encrypted.digest.to_vec(),
            chunk_size: 0,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 { Some((media_meta.duration_ms / 1000) as f64) } else { None },
            waveform_samples: media_meta.waveform.clone(),
            thumbnail: media_meta.thumbnail.clone(),
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
        };

        // Ensure attachments directory exists
        ensure_attachments_dir()
            .map_err(|e| AttachmentError::FileError(format!("Failed to create attachments dir: {}", e)))?;

        // Write encrypted content to filesystem
        let file_path = attachment_file_path(&attachment_id);
        std::fs::write(&file_path, &encrypted.ciphertext)
            .map_err(|e| AttachmentError::FileError(format!("Failed to write attachment file: {}", e)))?;

        tracing::debug!(
            "Wrote {} bytes encrypted content to {}",
            encrypted.ciphertext.len(),
            file_path.display()
        );

        // Create attachment record with content stored on filesystem
        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            local_path: None,
            remote_url: Some(cdn_key.clone()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
        };

        // Save to repository with file path
        if let Err(e) = self.attachment_repo.insert_with_file_path(&attachment, &file_path) {
            // Clean up file on failure
            let _ = std::fs::remove_file(&file_path);
            tracing::warn!("Failed to save CDN attachment with file path: {}", e);
            return Err(AttachmentError::StorageError(e.to_string()));
        }

        tracing::info!(
            cdn_key = %cdn_key,
            attachment_id = %attachment_id,
            filename = %filename,
            plaintext_size = plaintext_size,
            encrypted_size = encrypted_size,
            file_path = %file_path.display(),
            "Encrypted bytes uploaded to CDN and stored on filesystem"
        );

        Ok(CdnUploadResult {
            attachment_id,
            cdn_key,
            encryption_key: encrypted.key,
            digest: encrypted.digest,
            plaintext_size,
            content_type: mime_type.to_string(),
            file_name: filename.to_string(),
            waveform: media_meta.waveform.unwrap_or_default(),
            duration_ms: media_meta.duration_ms,
            thumbnail: media_meta.thumbnail.unwrap_or_default(),
            thumbnail_width: media_meta.thumbnail_width.unwrap_or(0),
            thumbnail_height: media_meta.thumbnail_height.unwrap_or(0),
            width: media_meta.width.unwrap_or(0),
            height: media_meta.height.unwrap_or(0),
        })
    }

    /// Extract media metadata from bytes (for in-memory content like downloaded GIFs).
    fn extract_media_metadata_from_bytes(&self, content: &[u8], mime_type: &str) -> MediaMetadata {
        let mut meta = MediaMetadata::default();

        if mime_type == "image/gif" || mime_type.starts_with("image/") {
            // Get dimensions from image bytes
            if let Ok((w, h)) = crate::media::ImageProcessor::get_dimensions_from_memory(content) {
                meta.width = Some(w);
                meta.height = Some(h);
            }

            // Generate thumbnail from bytes
            if let Ok((thumb_bytes, thumb_w, thumb_h)) = crate::media::ImageProcessor::generate_thumbnail_from_memory(content) {
                meta.thumbnail = Some(thumb_bytes);
                meta.thumbnail_width = Some(thumb_w);
                meta.thumbnail_height = Some(thumb_h);
            }
        }

        meta
    }

    /// Extract media metadata (waveform, thumbnail, dimensions) from a file.
    pub(crate) async fn extract_media_metadata(
        &self,
        file_path: &Path,
        mime_type: &str,
    ) -> MediaMetadata {
        let mut meta = MediaMetadata::default();

        // Extract waveform and duration for audio files
        if mime_type.starts_with("audio/") {
            let path_for_waveform = file_path.to_path_buf();
            meta.waveform = match tokio::task::spawn_blocking(move || {
                crate::media::extract_waveform(&path_for_waveform)
            }).await {
                Ok(Ok(samples)) => {
                    tracing::debug!("Extracted waveform: {} samples", samples.len());
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
            meta.duration_ms = tokio::task::spawn_blocking(move || {
                crate::media::AudioPlayer::get_duration(&path_for_duration)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }).await.unwrap_or(0);
        } else if mime_type.starts_with("image/") {
            // Image processing - use spawn_blocking for large images
            let path_for_dims = file_path.to_path_buf();
            let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                crate::media::ImageProcessor::get_dimensions(&path_for_dims)
                    .unwrap_or((0, 0))
            }).await.unwrap_or((0, 0));
            meta.width = if orig_w > 0 { Some(orig_w) } else { None };
            meta.height = if orig_h > 0 { Some(orig_h) } else { None };

            // Generate thumbnail in blocking thread (large images can take time/memory)
            let path_for_thumb = file_path.to_path_buf();
            match tokio::task::spawn_blocking(move || {
                crate::media::ImageProcessor::generate_thumbnail_bytes(&path_for_thumb)
            }).await {
                Ok(Ok((bytes, thumb_w, thumb_h))) => {
                    tracing::debug!(
                        "Generated thumbnail for image: {}x{} -> {}x{}, {} bytes",
                        orig_w, orig_h, thumb_w, thumb_h, bytes.len()
                    );
                    meta.thumbnail = Some(bytes);
                    meta.thumbnail_width = Some(thumb_w);
                    meta.thumbnail_height = Some(thumb_h);
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to generate thumbnail: {}", e);
                }
                Err(e) => {
                    tracing::warn!("Thumbnail generation task panicked: {}", e);
                }
            }
        } else if mime_type.starts_with("video/") {
            // Video processing uses FFmpeg - must use spawn_blocking
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

                    tracing::debug!(
                        "Generated thumbnail for video: {}x{} -> {}x{}, {} bytes, {}ms",
                        orig_w, orig_h, thumb_w, thumb_h, bytes.len(), vid_duration_ms
                    );

                    meta.thumbnail = Some(bytes);
                    meta.thumbnail_width = Some(thumb_w);
                    meta.thumbnail_height = Some(thumb_h);
                    meta.width = Some(orig_w);
                    meta.height = Some(orig_h);
                    meta.duration_ms = vid_duration_ms;
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
                    meta.width = if orig_w > 0 { Some(orig_w) } else { None };
                    meta.height = if orig_h > 0 { Some(orig_h) } else { None };
                    meta.duration_ms = vid_duration;
                }
                Err(e) => {
                    tracing::warn!("Video thumbnail task panicked: {}", e);
                }
            }
        }

        meta
    }
}

/// Media metadata extracted from files.
#[derive(Default)]
pub(crate) struct MediaMetadata {
    pub waveform: Option<Vec<u8>>,
    pub duration_ms: u64,
    pub thumbnail: Option<Vec<u8>>,
    pub thumbnail_width: Option<u32>,
    pub thumbnail_height: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Detect MIME type from file content using magic bytes.
///
/// Used as fallback when file has no extension or extension doesn't match content.
fn detect_mime_from_bytes(data: &[u8]) -> String {
    if data.len() < 12 {
        return "application/octet-stream".to_string();
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return "image/png".to_string();
    }

    // JPEG: FF D8 FF
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".to_string();
    }

    // GIF: GIF87a or GIF89a
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return "image/gif".to_string();
    }

    // WebP: RIFF....WEBP
    if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
        return "image/webp".to_string();
    }

    // BMP: BM
    if data.starts_with(b"BM") {
        return "image/bmp".to_string();
    }

    // MP4/M4A: ftyp at offset 4
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        // Check for audio vs video
        if data.len() >= 12 {
            let brand = &data[8..12];
            if brand == b"M4A " || brand == b"M4B " {
                return "audio/mp4".to_string();
            }
        }
        return "video/mp4".to_string();
    }

    // WebM/MKV: 1A 45 DF A3
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "video/webm".to_string();
    }

    // MP3: FF FB or FF FA or ID3
    if data.starts_with(&[0xFF, 0xFB]) || data.starts_with(&[0xFF, 0xFA]) || data.starts_with(b"ID3") {
        return "audio/mpeg".to_string();
    }

    // OGG: OggS
    if data.starts_with(b"OggS") {
        return "audio/ogg".to_string();
    }

    // FLAC: fLaC
    if data.starts_with(b"fLaC") {
        return "audio/flac".to_string();
    }

    // WAV: RIFF....WAVE
    if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WAVE" {
        return "audio/wav".to_string();
    }

    // PDF: %PDF
    if data.starts_with(b"%PDF") {
        return "application/pdf".to_string();
    }

    "application/octet-stream".to_string()
}
