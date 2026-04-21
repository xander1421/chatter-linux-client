//! Upload operations for attachments.

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::crypto::attachment_crypto::AttachmentCrypto;
use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use crate::domain::ids::{AttachmentId, ChannelId};
use crate::proto::chat_v1::FileChunk;

use super::{attachment_file_path, attachments_dir, ensure_attachments_dir, AttachmentError, AttachmentService, CdnUploadResult, UploadResult, CHUNK_SIZE};

/// Files >= 10MB use the resumable multipart API instead of gRPC streaming.
const LARGE_FILE_THRESHOLD: u64 = 10 * 1024 * 1024;

impl AttachmentService {
    /// Upload a file to a channel.
    ///
    /// Streams the file from disk in 256KB chunks — memory usage is O(CHUNK_SIZE)
    /// regardless of file size. Files >= 10MB use the resumable multipart API
    /// for automatic retry/resume support.
    pub async fn upload_file(
        &self,
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
        let mime_type = detect_mime_type(file_path).await;

        tracing::debug!(
            "Uploading {} ({} bytes, {}) to channel {}",
            filename,
            file_size,
            mime_type,
            channel_id
        );

        let file_metadata = if file_size >= LARGE_FILE_THRESHOLD {
            // Large files: resumable multipart API (supports retry/resume)
            self.upload_file_resumable(file_path, channel_id, &filename, &mime_type, file_size)
                .await?
        } else {
            // Small files: lazy gRPC stream from disk
            self.upload_file_streaming(file_path, channel_id, &filename, &mime_type, file_size)
                .await?
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
                channel_id: &ChannelId,
        filename: &str,
        file_size: u64,
    ) -> Result<crate::proto::chat_v1::FileMetadata, AttachmentError> {
        let mut verify_client = self.grpc_pool()?.file_client();

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

    /// Upload a small file via lazy gRPC streaming from disk.
    ///
    /// Reads CHUNK_SIZE at a time from disk — never loads the full file.
    async fn upload_file_streaming(
        &self,
        file_path: &Path,
        channel_id: &ChannelId,
        filename: &str,
        mime_type: &str,
        file_size: u64,
    ) -> Result<crate::proto::chat_v1::FileMetadata, AttachmentError> {
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let chunk_count = (file_size as usize + CHUNK_SIZE - 1) / CHUNK_SIZE;
        tracing::info!(
            "Streaming {} chunks ({} bytes) for {}",
            chunk_count, file_size, filename
        );

        let file_path_owned = file_path.to_path_buf();
        let transfer_id_clone = transfer_id.clone();

        // Open file before creating the stream so I/O errors are returned immediately
        let file = tokio::fs::File::open(&file_path_owned).await
            .map_err(|e| AttachmentError::FileError(format!(
                "Failed to open file for upload: {}", e
            )))?;

        // If the file produces a read error partway through we MUST NOT let
        // the upload silently complete with fewer chunks — that produces a
        // truncated blob on the server. `read_error` is a write-once slot:
        // the stream stores the first `io::Error` it sees, and the awaiter
        // below reads it after the RPC returns.
        let read_error: Arc<std::sync::OnceLock<std::io::Error>> =
            Arc::new(std::sync::OnceLock::new());
        let read_error_stream = read_error.clone();

        let chunk_stream = async_stream::stream! {
            let mut file = file;
            let mut buf = vec![0u8; CHUNK_SIZE];
            let mut sequence = 0u32;
            loop {
                let n = match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to read chunk from file during upload");
                        let _ = read_error_stream.set(e);
                        break;
                    }
                };
                if n == 0 { break; }
                let mut hasher = Sha256::new();
                hasher.update(&buf[..n]);
                let hash = hasher.finalize();
                yield FileChunk {
                    transfer_id: transfer_id_clone.clone(),
                    sequence,
                    data: buf[..n].to_vec(),
                    auth_tag: hash[..16].to_vec(),
                };
                sequence += 1;
            }
        };

        let mut file_client = self.grpc_pool()?.file_client();

        let upload_result = file_client
            .upload_file_with_metadata(
                &channel_id.to_string(),
                filename,
                mime_type,
                file_size,
                chunk_stream,
            )
            .await;

        // If the stream aborted early because of a file read error, fail the
        // whole upload regardless of what the server responded with — a
        // "success" here would actually be a truncated blob.
        if let Some(err) = read_error.get() {
            return Err(AttachmentError::FileError(format!(
                "File read failed during upload (truncated): {}", err
            )));
        }

        match upload_result {
            Ok(metadata) => {
                tracing::debug!("Streaming upload completed successfully");
                Ok(metadata)
            }
            Err(e) => {
                let error_str = format!("{}", e);
                if error_str.contains("h2 protocol error") || error_str.contains("http2 error") {
                    tracing::warn!("Got h2 protocol error - verifying upload for: {}", filename);
                    self.verify_upload(channel_id, filename, file_size).await
                } else {
                    Err(AttachmentError::NetworkError(format!("Upload failed: {}", e)))
                }
            }
        }
    }

    /// Upload a large file via the resumable multipart API.
    ///
    /// Reads CHUNK_SIZE at a time from disk. Each chunk is uploaded individually
    /// with per-chunk SHA-256 verification. Supports resume from the last
    /// successfully uploaded chunk.
    async fn upload_file_resumable(
        &self,
        file_path: &Path,
        channel_id: &ChannelId,
        filename: &str,
        mime_type: &str,
        file_size: u64,
    ) -> Result<crate::proto::chat_v1::FileMetadata, AttachmentError> {
        let mut file_client = self.grpc_pool()?.file_client();

        // Initiate upload session
        let session = file_client
            .initiate_upload(
                &channel_id.to_string(),
                filename,
                mime_type,
                file_size,
                CHUNK_SIZE as u32,
            )
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to initiate upload: {}", e)))?;

        let transfer_id = session.transfer_id.clone();
        tracing::info!(
            transfer_id = %transfer_id,
            chunks_total = session.chunks_total,
            chunks_uploaded = session.chunks_uploaded,
            "Resumable upload session started for {}",
            filename
        );

        // Open file and seek past already-uploaded chunks
        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to open file: {}", e)))?;

        if session.chunks_uploaded > 0 {
            let skip_bytes = session.chunks_uploaded as u64 * CHUNK_SIZE as u64;
            tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(skip_bytes))
                .await
                .map_err(|e| AttachmentError::FileError(format!("Failed to seek: {}", e)))?;
        }

        // Hash the whole file for the final complete call
        let mut whole_file_hasher = Sha256::new();

        // If resuming, we need to hash the already-uploaded portion too
        if session.chunks_uploaded > 0 {
            let mut hash_file = tokio::fs::File::open(file_path)
                .await
                .map_err(|e| AttachmentError::FileError(format!("Failed to open for hashing: {}", e)))?;
            let skip_bytes = session.chunks_uploaded as u64 * CHUNK_SIZE as u64;
            let mut remaining = skip_bytes;
            let mut buf = vec![0u8; CHUNK_SIZE];
            while remaining > 0 {
                let to_read = remaining.min(CHUNK_SIZE as u64) as usize;
                hash_file.read_exact(&mut buf[..to_read])
                    .await
                    .map_err(|e| AttachmentError::FileError(format!("Failed to hash skipped data: {}", e)))?;
                whole_file_hasher.update(&buf[..to_read]);
                remaining -= to_read as u64;
            }
        }

        // Upload remaining chunks
        let mut buf = vec![0u8; CHUNK_SIZE];
        for sequence in session.chunks_uploaded..session.chunks_total {
            let n = file.read(&mut buf)
                .await
                .map_err(|e| AttachmentError::FileError(format!("Failed to read chunk {}: {}", sequence, e)))?;
            if n == 0 {
                break;
            }

            let chunk_data = buf[..n].to_vec();

            // Per-chunk SHA-256
            let mut chunk_hasher = Sha256::new();
            chunk_hasher.update(&chunk_data);
            let chunk_sha256 = chunk_hasher.finalize().to_vec();

            // Update whole-file hash
            whole_file_hasher.update(&chunk_data);

            // Upload chunk with retry
            let mut retries = 0;
            loop {
                match file_client
                    .upload_chunk(&transfer_id, sequence, chunk_data.clone(), chunk_sha256.clone())
                    .await
                {
                    Ok(resp) => {
                        tracing::debug!(
                            sequence,
                            progress = resp.progress,
                            "Chunk uploaded"
                        );
                        break;
                    }
                    Err(e) if retries < 3 => {
                        retries += 1;
                        tracing::warn!(
                            sequence,
                            retries,
                            error = %e,
                            "Chunk upload failed, retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500 * retries as u64)).await;
                    }
                    Err(e) => {
                        // Abort the upload session on permanent failure
                        let _ = file_client.abort_upload(&transfer_id).await;
                        return Err(AttachmentError::NetworkError(
                            format!("Chunk {} upload failed after retries: {}", sequence, e),
                        ));
                    }
                }
            }
        }

        // Complete upload
        let file_sha256 = whole_file_hasher.finalize().to_vec();
        let metadata = file_client
            .complete_upload(&transfer_id, file_sha256)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to complete upload: {}", e)))?;

        tracing::info!(
            transfer_id = %transfer_id,
            file_id = %metadata.id,
            "Resumable upload completed for {}",
            filename
        );

        Ok(metadata)
    }

    /// Upload a file to CDN with client-side encryption.
    ///
    /// Encrypts the file in a streaming fashion (O(CHUNK_SIZE) memory) to a temp
    /// file, then streams the encrypted temp file to the presigned PUT URL.
    ///
    /// Returns a `CdnUploadResult` with all data needed for AttachmentPointer.
    pub async fn upload_to_cdn_encrypted(
        &self,
        file_path: &Path,
    ) -> Result<CdnUploadResult, AttachmentError> {
        let plaintext_size = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to read file metadata: {}", e)))?
            .len();

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        let mime_type = detect_mime_type(file_path).await;

        tracing::debug!(
            filename = %filename,
            plaintext_size = plaintext_size,
            mime_type = %mime_type,
            "Encrypting attachment for CDN upload (streaming)"
        );

        // Extract media metadata from the original file before encryption
        let media_meta = self.extract_media_metadata(file_path, &mime_type).await;

        // Streaming encryption to temp file (O(CHUNK_SIZE) memory)
        ensure_attachments_dir()
            .map_err(|e| AttachmentError::FileError(format!("Failed to create attachments dir: {}", e)))?;

        let temp_file = tempfile::NamedTempFile::new_in(attachments_dir())
            .map_err(|e| AttachmentError::FileError(format!("Failed to create temp file: {}", e)))?;

        let input_path = file_path.to_path_buf();
        let output_path = temp_file.path().to_path_buf();

        let encrypt_start = std::time::Instant::now();
        let encrypt_result = tokio::task::spawn_blocking(move || {
            AttachmentCrypto::encrypt_file_to_file(&input_path, &output_path)
        })
        .await
        .map_err(|e| AttachmentError::EncryptionError(format!("Encryption task failed: {}", e)))?
        .map_err(|e| AttachmentError::EncryptionError(e.to_string()))?;

        tracing::info!(
            encrypted_size = encrypt_result.ciphertext_size,
            elapsed = ?encrypt_start.elapsed(),
            "Attachment encrypted (streaming)"
        );

        // Get presigned upload URL
        let mut file_client = self.grpc_pool()?.file_client();
        let upload_response = file_client
            .get_attachment_upload_url(encrypt_result.ciphertext_size)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get upload URL: {}", e)))?;

        let cdn_key = upload_response.cdn_key;
        let upload_url = upload_response.upload_url;

        tracing::debug!(cdn_key = %cdn_key, "Got presigned URL, streaming encrypted content");

        // Stream encrypted temp file to PUT (O(CHUNK_SIZE) memory)
        let upload_start = std::time::Instant::now();
        let encrypted_file = tokio::fs::File::open(temp_file.path())
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to open temp file: {}", e)))?;
        let stream = tokio_util::io::ReaderStream::new(encrypted_file);
        let body = reqwest::Body::wrap_stream(stream);

        let http_client = reqwest::Client::new();
        let response = http_client
            .put(&upload_url)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", encrypt_result.ciphertext_size)
            .body(body)
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

        // Persist temp file to permanent location (atomic rename on same filesystem)
        let permanent_path = attachment_file_path(&attachment_id);
        temp_file.persist(&permanent_path)
            .map_err(|e| AttachmentError::FileError(format!("Failed to persist encrypted file: {}", e)))?;

        tracing::debug!(
            "Persisted encrypted content to {}",
            permanent_path.display()
        );

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: plaintext_size,
            sha256: encrypt_result.digest.to_vec(),
            chunk_size: 0,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 { Some((media_meta.duration_ms / 1000) as f64) } else { None },
            waveform_samples: media_meta.waveform.clone(),
            thumbnail: media_meta.thumbnail.clone(),
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
        };

        // Create attachment record
        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            local_path: None,
            remote_url: Some(cdn_key.clone()),
            encryption_key: Some(encrypt_result.key.to_vec()),
            encryption_iv: Some(encrypt_result.digest.to_vec()),
            created_at: chrono::Utc::now(),
        };

        // Save to repository with file path
        if let Err(e) = self.attachment_repo.insert_with_file_path(&attachment, &permanent_path) {
            let _ = std::fs::remove_file(&permanent_path);
            tracing::warn!("Failed to save CDN attachment: {}", e);
            return Err(AttachmentError::StorageError(e.to_string()));
        }

        tracing::info!(
            cdn_key = %cdn_key,
            attachment_id = %attachment_id,
            filename = %filename,
            plaintext_size = plaintext_size,
            encrypted_size = encrypt_result.ciphertext_size,
            "Encrypted attachment uploaded to CDN (streaming)"
        );

        Ok(CdnUploadResult {
            attachment_id,
            cdn_key,
            encryption_key: encrypt_result.key,
            digest: encrypt_result.digest,
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
            incremental_mac: encrypt_result.incremental_mac.clone(),
            chunk_size: encrypt_result.incremental_chunk_size,
            ciphertext_size: encrypt_result.ciphertext_size,
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
        let mut file_client = self.grpc_pool()?.file_client();
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
            // In-memory encryption path: no incremental MAC (used for small content like GIFs)
            incremental_mac: vec![],
            chunk_size: 0,
            ciphertext_size: encrypted.ciphertext.len() as u64,
        })
    }

    /// Download a GIF from a URL and upload it to CDN (encrypted).
    ///
    /// Combines HTTP download + CDN upload into a single operation so the
    /// UI layer doesn't need raw HTTP clients.
    pub async fn download_and_upload_gif(
        &self,
        url: &str,
        gif_id: &str,
    ) -> Result<CdnUploadResult, AttachmentError> {
        let client = reqwest::Client::new();
        let response = client.get(url).send().await
            .map_err(|e| AttachmentError::NetworkError(format!("GIF download failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(
                format!("GIF download HTTP {}", response.status())
            ));
        }

        let bytes = response.bytes().await
            .map_err(|e| AttachmentError::NetworkError(format!("GIF read failed: {}", e)))?;

        let filename = format!("{}.gif", gif_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_"));

        self.upload_bytes_to_cdn_encrypted(&bytes, &filename, "image/gif").await
    }

    /// Download bytes from a URL (for media display, GIF animation, etc).
    pub async fn download_url_bytes(
        &self,
        url: &str,
    ) -> Result<Vec<u8>, AttachmentError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AttachmentError::NetworkError(format!("HTTP client error: {}", e)))?;

        let response = client.get(url).send().await
            .map_err(|e| AttachmentError::NetworkError(format!("Download failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(
                format!("Download HTTP {}", response.status())
            ));
        }

        let bytes = response.bytes().await
            .map_err(|e| AttachmentError::NetworkError(format!("Read failed: {}", e)))?;

        Ok(bytes.to_vec())
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
            #[cfg(feature = "desktop-gstreamer")]
            {
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
            }

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
            // Video processing uses GStreamer — only available with desktop-media feature
            #[cfg(feature = "desktop-gstreamer")]
            {
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

/// Detect MIME type from a file path without reading the entire file.
///
/// Tries extension-based detection first, then falls back to reading
/// only the first 12 bytes for magic byte detection.
async fn detect_mime_type(file_path: &Path) -> String {
    // Fast path: detect from file extension
    if let Some(mime) = mime_guess::from_path(file_path).first() {
        return mime.to_string();
    }
    // Fallback: read first 12 bytes for magic byte detection
    if let Ok(mut f) = tokio::fs::File::open(file_path).await {
        let mut buf = [0u8; 12];
        if tokio::io::AsyncReadExt::read_exact(&mut f, &mut buf).await.is_ok() {
            return detect_mime_from_bytes(&buf);
        }
    }
    "application/octet-stream".to_string()
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
