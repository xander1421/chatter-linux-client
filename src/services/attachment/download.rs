//! Download operations for attachments.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use zeroize::Zeroize;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use crate::domain::ids::AttachmentId;
use crate::network::grpc::file_client::FileClient;
use crate::network::grpc::GrpcClientPool;
use crate::proto::content_v1::AttachmentPointer;

use super::{AttachmentError, AttachmentService, DownloadResult, CHUNK_SIZE};

impl AttachmentService {
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

        // Extract media metadata
        let media_meta = self.extract_media_metadata(&local_path, &mime_type).await;

        // Update repository with local path
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: actual_size,
            sha256: expected_hash,
            chunk_size: CHUNK_SIZE as u32,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 { Some((media_meta.duration_ms / 1000) as f64) } else { None },
            waveform_samples: media_meta.waveform,
            thumbnail: media_meta.thumbnail,
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
        };

        let attachment = Attachment {
            id: *attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::NotDownloaded,
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
        use crate::crypto::attachment_crypto::AttachmentCrypto;

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
        let mut plaintext = AttachmentCrypto::decrypt(
            &ciphertext,
            &key,
            &digest,
            pointer.plaintext_size as usize,
        )
        .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        // Also zeroize the encryption key after use
        key.zeroize();

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
        let plaintext_size = plaintext.len();
        tokio::fs::write(&local_path, &plaintext)
            .await
            .map_err(|e| {
                // Zeroize on error path too
                plaintext.zeroize();
                AttachmentError::FileError(format!("Failed to write file: {}", e))
            })?;

        // Zeroize decrypted plaintext immediately after writing to disk
        plaintext.zeroize();

        tracing::info!(
            cdn_key = %pointer.cdn_key,
            filename = %filename,
            plaintext_size = plaintext_size,
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
            file_size: plaintext_size as u64,
            mime_type,
        })
    }

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
        let mut file_client = FileClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
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
        // This atomically sets status=Ready and content_location=InDatabase
        self.attachment_repo
            .store_encrypted_content(attachment_id, &ciphertext)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?;

        tracing::info!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext.len(),
            "Encrypted content stored in database"
        );

        Ok(())
    }
}
