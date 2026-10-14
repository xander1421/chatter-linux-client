//! Upload operations for attachments.

use std::path::Path;

use crate::crypto::attachment_crypto::{AttachmentCrypto, max_attachment_size};
use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use crate::domain::ids::AttachmentId;
use crate::error::CryptoError;

use super::{
    AttachmentError, AttachmentService, CdnUploadResult, attachment_file_path, attachments_dir,
    ensure_attachments_dir,
};

/// Ciphertext > this uses multipart; ≤ uses single-PUT.
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Upper bound on multipart parts. With the 2 GiB attachment cap and S3's
/// 5 MiB minimum part size, a legitimate upload is well under 500 parts; this
/// caps a hostile/broken server `part_size` (e.g. 1) before `part_ranges`
/// allocates a multi-GB Vec.
const MAX_MULTIPART_PARTS: u64 = 10_000;

/// Split `total` bytes into (offset, len) ranges of `part_size` (last may be smaller).
fn part_ranges(total: u64, part_size: u64) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < total {
        let len = part_size.min(total - offset);
        ranges.push((offset, len));
        offset += len;
    }
    ranges
}

impl AttachmentService {
    /// PUT one part (byte range of the temp file) to its presigned URL, 3× retry.
    /// Returns the ETag from the response headers. Routes over Tor in anon mode
    /// via `cdn_http_put` (never a direct request that would leak the real IP).
    async fn upload_one_part(
        &self,
        url: &str,
        temp_path: &std::path::Path,
        offset: u64,
        len: u64,
    ) -> Result<String, AttachmentError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut last_err = String::new();
        for attempt in 0..3 {
            // Read the exact byte range fresh each attempt.
            let mut f = tokio::fs::File::open(temp_path)
                .await
                .map_err(|e| AttachmentError::FileError(format!("open temp: {}", e)))?;
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| AttachmentError::FileError(format!("seek: {}", e)))?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf)
                .await
                .map_err(|e| AttachmentError::FileError(format!("read part: {}", e)))?;

            match self
                .cdn_http_put(url, buf, "application/octet-stream")
                .await
            {
                Ok((status, etag)) if (200..300).contains(&status) => match etag {
                    Some(e) => return Ok(e),
                    None => last_err = "part PUT succeeded but no ETag header".to_string(),
                },
                Ok((status, _)) => last_err = format!("part PUT status {}", status),
                Err(e) => last_err = format!("part PUT error: {}", e),
            }
            tracing::warn!(attempt, offset, len, error = %last_err, "part upload retry");
        }
        Err(AttachmentError::NetworkError(format!(
            "part upload failed after 3 attempts: {}",
            last_err
        )))
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
        self.upload_to_cdn_encrypted_with_media(file_path, None)
            .await
    }

    /// Recorded voice notes already have these values from the capture worker.
    /// Re-probing the WAV starts a decoder and scans the audio a second time.
    pub async fn upload_recorded_voice_note(
        &self,
        file_path: &Path,
        waveform: &[u8],
        duration_ms: u64,
    ) -> Result<CdnUploadResult, AttachmentError> {
        let media = MediaMetadata {
            waveform: Some(waveform.to_vec()),
            duration_ms,
            ..MediaMetadata::default()
        };
        self.upload_to_cdn_encrypted_with_media(file_path, Some(media))
            .await
    }

    async fn upload_to_cdn_encrypted_with_media(
        &self,
        file_path: &Path,
        known_media: Option<MediaMetadata>,
    ) -> Result<CdnUploadResult, AttachmentError> {
        let started = std::time::Instant::now();
        let plaintext_size = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| {
                AttachmentError::FileError(format!("Failed to read file metadata: {}", e))
            })?
            .len();

        // Enforce attachment size cap at the service boundary. The same check
        // exists inside encrypt_file_to_file, but failing here avoids the work
        // of opening a temp file / spawn_blocking for a known-rejected upload.
        let max_size = max_attachment_size();
        if plaintext_size > max_size {
            return Err(AttachmentError::EncryptionError(
                CryptoError::AttachmentTooLarge {
                    size: plaintext_size,
                    max: max_size,
                }
                .to_string(),
            ));
        }

        // Sanitize outgoing media before encryption (MP4 metadata strip/relocate,
        // WebP validation). File-streaming variant — never loads the attachment
        // into memory. Malformed MP4/WebP is rejected; other formats untouched.
        let sanitized_temp = {
            let p = file_path.to_path_buf();
            tokio::task::spawn_blocking(move || super::sanitize::sanitize_media_file(&p))
                .await
                .map_err(|e| AttachmentError::FileError(format!("Sanitize task failed: {e}")))??
        };
        let plaintext_path: &Path = sanitized_temp
            .as_ref()
            .map(|t| t.path())
            .unwrap_or(file_path);
        let plaintext_size = match &sanitized_temp {
            Some(t) => tokio::fs::metadata(t.path())
                .await
                .map_err(|e| {
                    AttachmentError::FileError(format!("Failed to stat sanitized file: {}", e))
                })?
                .len(),
            None => plaintext_size,
        };

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        let mime_type = detect_mime_type(file_path).await;

        // No filename, object key or plaintext size in upload logs: the same
        // path uploads secret-chat files.
        tracing::debug!(
            mime_type = %mime_type,
            "Encrypting attachment for CDN upload (streaming)"
        );

        // Extract media metadata from the original file before encryption
        let media_start = std::time::Instant::now();
        let media_meta = match known_media {
            Some(media) => media,
            None => self.extract_media_metadata(file_path, &mime_type).await,
        };
        if self.secret_chat {
            tracing::info!(
                elapsed_ms = media_start.elapsed().as_millis(),
                "secret attachment upload: media metadata ready"
            );
        }

        // Streaming encryption to temp file (O(CHUNK_SIZE) memory)
        ensure_attachments_dir().map_err(|e| {
            AttachmentError::FileError(format!("Failed to create attachments dir: {}", e))
        })?;

        let temp_file = tempfile::NamedTempFile::new_in(attachments_dir()).map_err(|e| {
            AttachmentError::FileError(format!("Failed to create temp file: {}", e))
        })?;

        let input_path = plaintext_path.to_path_buf();
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

        // Route large ciphertexts through multipart (parallel part PUTs direct to
        // RustFS); small ones keep the single-PUT path. Both arms converge on a
        // confirmed `cdn_key`, which feeds the shared persist/build tail below.
        let cdn_key = if encrypt_result.ciphertext_size > MULTIPART_THRESHOLD {
            self.upload_multipart_to_cdn(temp_file.path(), encrypt_result.ciphertext_size)
                .await?
        } else {
            // Get presigned upload URL
            let mut file_client = self.file_client()?;
            let mint_start = std::time::Instant::now();
            let upload_response = file_client
                .get_attachment_upload_url(encrypt_result.ciphertext_size)
                .await
                .map_err(|e| {
                    AttachmentError::NetworkError(format!("Failed to get upload URL: {}", e))
                })?;
            if self.secret_chat {
                tracing::info!(
                    elapsed_ms = mint_start.elapsed().as_millis(),
                    "secret attachment upload: URL minted"
                );
            }

            let cdn_key = upload_response.cdn_key;
            let upload_url = upload_response.upload_url;

            tracing::debug!("Got presigned URL, streaming encrypted content");

            let upload_start = std::time::Instant::now();

            // Anon mode: tunnel the PUT over Tor. The Tor client takes an
            // in-memory body, so read the ciphertext (bounded by the ≤100 MiB
            // single-PUT threshold) into memory rather than streaming the file.
            #[cfg(feature = "onion")]
            let sent = if self.cdn_over_tor()? {
                let bytes = tokio::fs::read(temp_file.path()).await.map_err(|e| {
                    AttachmentError::FileError(format!("Failed to read temp file: {}", e))
                })?;
                let (status, _etag) = self
                    .cdn_http_put(&upload_url, bytes, "application/octet-stream")
                    .await?;
                Some(status)
            } else {
                None
            };
            #[cfg(not(feature = "onion"))]
            let sent: Option<u16> = if self.cdn_over_tor()? {
                return Err(super::no_tor());
            } else {
                None
            };

            let status = if let Some(status) = sent {
                status
            } else {
                // Direct: stream the encrypted temp file to PUT (O(CHUNK_SIZE) memory).
                let encrypted_file =
                    tokio::fs::File::open(temp_file.path()).await.map_err(|e| {
                        AttachmentError::FileError(format!("Failed to open temp file: {}", e))
                    })?;
                let stream = tokio_util::io::ReaderStream::new(encrypted_file);
                let body = reqwest::Body::wrap_stream(stream);
                let response = super::cdn_client()?
                    .put(&upload_url)
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", encrypt_result.ciphertext_size)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| {
                        AttachmentError::NetworkError(format!(
                            "HTTP upload failed: {}",
                            e.without_url()
                        ))
                    })?;
                response.status().as_u16()
            };

            if !(200..300).contains(&status) {
                return Err(AttachmentError::NetworkError(format!(
                    "CDN upload failed with status: {}",
                    status
                )));
            }

            tracing::info!(elapsed = ?upload_start.elapsed(), "CDN upload complete");

            // Confirm upload with server
            let confirm_start = std::time::Instant::now();
            file_client
                .confirm_attachment_upload(&cdn_key)
                .await
                .map_err(|e| {
                    AttachmentError::NetworkError(format!("Failed to confirm upload: {}", e))
                })?;
            if self.secret_chat {
                tracing::info!(
                    elapsed_ms = confirm_start.elapsed().as_millis(),
                    "secret attachment upload: confirmed"
                );
            }

            cdn_key
        };

        // Generate deterministic attachment ID from cdn_key
        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

        // Persist temp file to permanent location (atomic rename on same filesystem)
        let permanent_path = attachment_file_path(&attachment_id);
        temp_file.persist(&permanent_path).map_err(|e| {
            AttachmentError::FileError(format!("Failed to persist encrypted file: {}", e))
        })?;

        tracing::debug!(
            "Persisted encrypted content to {}",
            permanent_path.display()
        );

        // Upload the thumbnail as a separate small CDN object (sidecar) so the
        // attachment pointer stays tiny and fits every transport's cap.
        let (thumb_cdn_key, thumb_key, thumb_digest, thumb_pt_size) = match &media_meta.thumbnail {
            Some(t) => self.upload_thumbnail_sidecar(t).await.unwrap_or_default(),
            None => Default::default(),
        };

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.clone(),
            mime_type: mime_type.clone(),
            size: plaintext_size,
            sha256: encrypt_result.digest.to_vec(),
            chunk_size: 0,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 {
                Some((media_meta.duration_ms / 1000) as f64)
            } else {
                None
            },
            waveform_samples: media_meta.waveform.clone(),
            thumbnail: media_meta.thumbnail.clone(),
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
            thumbnail_cdn_key: (!thumb_cdn_key.is_empty()).then(|| thumb_cdn_key.clone()),
            thumbnail_key: (!thumb_key.is_empty()).then(|| thumb_key.clone()),
            thumbnail_digest: (!thumb_digest.is_empty()).then(|| thumb_digest.clone()),
            thumbnail_plaintext_size: (thumb_pt_size > 0).then_some(thumb_pt_size),
        };

        // Create attachment record
        let attachment = Attachment {
            id: attachment_id,
            metadata: domain_metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: Some(cdn_key.clone()),
            encryption_key: Some(encrypt_result.key.to_vec()),
            encryption_iv: Some(encrypt_result.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: self.secret_chat,
        };

        // Save to repository with file path
        if let Err(e) = self
            .attachment_repo
            .insert_with_file_path(&attachment, &permanent_path)
        {
            let _ = std::fs::remove_file(&permanent_path);
            tracing::warn!("Failed to save CDN attachment: {}", e);
            return Err(AttachmentError::StorageError(e.to_string()));
        }

        tracing::info!(
            encrypted_size = encrypt_result.ciphertext_size,
            "Encrypted attachment uploaded to CDN (streaming)"
        );

        if self.secret_chat {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "secret attachment upload: ready"
            );
        }
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
            thumbnail_cdn_key: thumb_cdn_key,
            thumbnail_key: thumb_key,
            thumbnail_digest: thumb_digest,
            thumbnail_plaintext_size: thumb_pt_size,
            thumbnail_width: media_meta.thumbnail_width.unwrap_or(0),
            thumbnail_height: media_meta.thumbnail_height.unwrap_or(0),
            width: media_meta.width.unwrap_or(0),
            height: media_meta.height.unwrap_or(0),
            ciphertext_size: encrypt_result.ciphertext_size,
        })
    }

    /// Upload a decoded thumbnail as its own small encrypted CDN object.
    ///
    /// Returns `(cdn_key, key[64], digest[32], plaintext_size)`. Best-effort:
    /// on any failure returns `None` so the parent upload still succeeds — the
    /// receiver just won't have a before-download preview. Keeping the thumbnail
    /// out-of-band is what lets the attachment pointer fit the sealed-DM padding
    /// budget (≤4096 bytes) on every transport.
    async fn upload_thumbnail_sidecar(
        &self,
        thumbnail: &[u8],
    ) -> Option<(String, Vec<u8>, Vec<u8>, u32)> {
        if thumbnail.is_empty() {
            return None;
        }
        let plaintext_size = thumbnail.len() as u32;
        let encrypted = match AttachmentCrypto::encrypt(thumbnail) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar encrypt failed; sending without preview");
                return None;
            }
        };
        let encrypted_size = encrypted.ciphertext.len() as u64;
        let mut file_client = self.file_client().ok()?;
        let upload_response = match file_client.get_attachment_upload_url(encrypted_size).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar upload-url failed; sending without preview");
                return None;
            }
        };
        let cdn_key = upload_response.cdn_key;
        // Over Tor in anon mode; direct otherwise.
        match self
            .cdn_http_put(
                &upload_response.upload_url,
                encrypted.ciphertext.clone(),
                "application/octet-stream",
            )
            .await
        {
            Ok((status, _etag)) if (200..300).contains(&status) => {}
            Ok((status, _etag)) => {
                tracing::warn!(
                    status,
                    "Thumbnail sidecar PUT failed; sending without preview"
                );
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar PUT errored; sending without preview");
                return None;
            }
        }
        let _ = file_client.confirm_attachment_upload(&cdn_key).await;
        tracing::debug!("Thumbnail sidecar uploaded");
        Some((
            cdn_key,
            encrypted.key.to_vec(),
            encrypted.digest.to_vec(),
            plaintext_size,
        ))
    }

    /// Upload an already-encrypted temp file via multipart, direct to RustFS.
    /// Returns the confirmed cdn_key. Bytes never touch the app server.
    async fn upload_multipart_to_cdn(
        &self,
        temp_path: &std::path::Path,
        ciphertext_size: u64,
    ) -> Result<String, AttachmentError> {
        use futures::stream::StreamExt;

        let mut file_client = self.file_client()?;
        let create = file_client
            .create_attachment_multipart_upload(ciphertext_size)
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("create multipart failed: {}", e))
            })?;

        let cdn_key = create.cdn_key.clone();
        let upload_id = create.upload_id.clone();
        let part_size = create.part_size as u64;
        // Guard against a malformed server response: part_size 0 would make
        // part_ranges loop forever (offset never advances), growing an
        // unbounded Vec until OOM. Fail loudly instead.
        if part_size == 0 {
            let _ = file_client
                .abort_attachment_multipart_upload(&cdn_key, &upload_id)
                .await;
            return Err(AttachmentError::NetworkError(
                "server returned part_size 0 for multipart upload".to_string(),
            ));
        }
        // Reject an absurd implied part count (e.g. hostile part_size=1) BEFORE
        // part_ranges builds the Vec — otherwise it allocates ~ciphertext/part_size
        // tuples (multi-GB) before the len-mismatch check below can fire.
        let implied_parts = ciphertext_size.div_ceil(part_size);
        if implied_parts > MAX_MULTIPART_PARTS {
            let _ = file_client
                .abort_attachment_multipart_upload(&cdn_key, &upload_id)
                .await;
            return Err(AttachmentError::NetworkError(format!(
                "server part_size {} implies {} parts (max {})",
                part_size, implied_parts, MAX_MULTIPART_PARTS
            )));
        }
        let ranges = part_ranges(ciphertext_size, part_size);
        if ranges.len() != create.part_urls.len() {
            return Err(AttachmentError::NetworkError(format!(
                "part count mismatch: {} ranges vs {} urls",
                ranges.len(),
                create.part_urls.len()
            )));
        }

        let temp_path = temp_path.to_path_buf();

        // Build one future per part; run 4 at a time.
        let uploads = ranges.into_iter().enumerate().map(|(i, (offset, len))| {
            let url = create.part_urls[i].clone();
            let temp_path = temp_path.clone();
            async move {
                let part_number = (i as u32) + 1;
                let etag = self.upload_one_part(&url, &temp_path, offset, len).await?;
                Ok::<crate::proto::chat_v1::CompletedPart, AttachmentError>(
                    crate::proto::chat_v1::CompletedPart {
                        number: part_number,
                        etag,
                    },
                )
            }
        });

        let results: Vec<Result<crate::proto::chat_v1::CompletedPart, AttachmentError>> =
            futures::stream::iter(uploads)
                .buffer_unordered(4)
                .collect()
                .await;

        let mut parts = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Ok(p) => parts.push(p),
                Err(e) => {
                    let _ = file_client
                        .abort_attachment_multipart_upload(&cdn_key, &upload_id)
                        .await;
                    return Err(e);
                }
            }
        }
        parts.sort_by_key(|p| p.number);

        file_client
            .complete_attachment_multipart_upload(&cdn_key, &upload_id, parts)
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("complete multipart failed: {}", e))
            })?;

        Ok(cdn_key)
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

        // Enforce attachment size cap at the service boundary. The in-memory
        // encrypt() path does not check the cap itself, so this is the only
        // gate that prevents oversized buffers (e.g. GIFs from Klipy/Tenor)
        // from being uploaded.
        let max_size = max_attachment_size();
        if plaintext_size > max_size {
            return Err(AttachmentError::EncryptionError(
                CryptoError::AttachmentTooLarge {
                    size: plaintext_size,
                    max: max_size,
                }
                .to_string(),
            ));
        }

        // Sanitize outgoing media (MP4 metadata strip/relocate, WebP validation).
        // Malformed MP4/WebP is rejected; other formats pass through unchanged.
        let content: std::borrow::Cow<'_, [u8]> = match super::sanitize::sanitize_media(content)? {
            super::sanitize::SanitizeOutcome::Rewritten(bytes) => bytes.into(),
            super::sanitize::SanitizeOutcome::Unchanged => content.into(),
        };
        let content = content.as_ref();
        let plaintext_size = content.len() as u64;

        tracing::debug!(mime_type = %mime_type, "Encrypting bytes for CDN upload");

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
        let mut file_client = self.file_client()?;
        let upload_response = file_client
            .get_attachment_upload_url(encrypted_size)
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("Failed to get upload URL: {}", e))
            })?;

        let cdn_key = upload_response.cdn_key;
        let upload_url = upload_response.upload_url;

        tracing::debug!("Got presigned URL, uploading encrypted content");

        // Upload encrypted ciphertext via HTTP PUT (over Tor in anon mode).
        let upload_start = std::time::Instant::now();
        let (status, _etag) = self
            .cdn_http_put(
                &upload_url,
                encrypted.ciphertext.clone(),
                "application/octet-stream",
            )
            .await?;

        if !(200..300).contains(&status) {
            return Err(AttachmentError::NetworkError(format!(
                "CDN upload failed with status: {}",
                status
            )));
        }

        tracing::info!(elapsed = ?upload_start.elapsed(), "CDN upload complete");

        // Confirm upload with server
        file_client
            .confirm_attachment_upload(&cdn_key)
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("Failed to confirm upload: {}", e))
            })?;

        // Generate deterministic attachment ID from cdn_key
        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

        // Extract media metadata from bytes (for GIFs)
        let media_meta = self.extract_media_metadata_from_bytes(content, mime_type);

        // Thumbnail sidecar (see upload_thumbnail_sidecar).
        let (thumb_cdn_key, thumb_key, thumb_digest, thumb_pt_size) = match &media_meta.thumbnail {
            Some(t) => self.upload_thumbnail_sidecar(t).await.unwrap_or_default(),
            None => Default::default(),
        };

        // Build metadata
        let domain_metadata = FileMetadata {
            name: filename.to_string(),
            mime_type: mime_type.to_string(),
            size: plaintext_size,
            sha256: encrypted.digest.to_vec(),
            chunk_size: 0,
            width: media_meta.width,
            height: media_meta.height,
            duration_seconds: if media_meta.duration_ms > 0 {
                Some((media_meta.duration_ms / 1000) as f64)
            } else {
                None
            },
            waveform_samples: media_meta.waveform.clone(),
            thumbnail: media_meta.thumbnail.clone(),
            thumbnail_width: media_meta.thumbnail_width,
            thumbnail_height: media_meta.thumbnail_height,
            thumbnail_cdn_key: (!thumb_cdn_key.is_empty()).then(|| thumb_cdn_key.clone()),
            thumbnail_key: (!thumb_key.is_empty()).then(|| thumb_key.clone()),
            thumbnail_digest: (!thumb_digest.is_empty()).then(|| thumb_digest.clone()),
            thumbnail_plaintext_size: (thumb_pt_size > 0).then_some(thumb_pt_size),
        };

        // Ensure attachments directory exists
        ensure_attachments_dir().map_err(|e| {
            AttachmentError::FileError(format!("Failed to create attachments dir: {}", e))
        })?;

        // Write encrypted content to filesystem
        let file_path = attachment_file_path(&attachment_id);
        std::fs::write(&file_path, &encrypted.ciphertext).map_err(|e| {
            AttachmentError::FileError(format!("Failed to write attachment file: {}", e))
        })?;

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
            remote_url: Some(cdn_key.clone()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: self.secret_chat,
        };

        // Save to repository with file path
        if let Err(e) = self
            .attachment_repo
            .insert_with_file_path(&attachment, &file_path)
        {
            // Clean up file on failure
            let _ = std::fs::remove_file(&file_path);
            tracing::warn!("Failed to save CDN attachment with file path: {}", e);
            return Err(AttachmentError::StorageError(e.to_string()));
        }

        tracing::info!(
            encrypted_size = encrypted_size,
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
            thumbnail_cdn_key: thumb_cdn_key,
            thumbnail_key: thumb_key,
            thumbnail_digest: thumb_digest,
            thumbnail_plaintext_size: thumb_pt_size,
            thumbnail_width: media_meta.thumbnail_width.unwrap_or(0),
            thumbnail_height: media_meta.thumbnail_height.unwrap_or(0),
            width: media_meta.width.unwrap_or(0),
            height: media_meta.height.unwrap_or(0),
            ciphertext_size: encrypted.ciphertext.len() as u64,
        })
    }

    /// Fetch a GIF via the server's GIF media proxy and upload it to CDN (encrypted).
    ///
    /// Combines the fetch + CDN upload into a single operation so the UI layer
    /// doesn't need raw HTTP clients.
    pub async fn download_and_upload_gif(
        &self,
        url: &str,
        gif_id: &str,
    ) -> Result<CdnUploadResult, AttachmentError> {
        // The media proxy takes account auth only; a secret chat sends no GIFs
        // (`ConversationType::gif_picker_available`).
        if self.secret_chat {
            return Err(AttachmentError::InvalidAttachment(
                "GIFs are not available in a secret chat".into(),
            ));
        }
        let bytes = self.fetch_gif_media(url).await?;

        let filename = format!(
            "{}.gif",
            gif_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
        );

        self.upload_bytes_to_cdn_encrypted(&bytes, &filename, "image/gif")
            .await
    }

    /// Fetch a GifResult media URL through the server's GIF media proxy.
    ///
    /// The provider CDN is a public-PKI host the pinned client cannot (and must
    /// not) reach directly; the server fetches it so the provider never sees the
    /// client's IP. In anon mode the RPC itself already rides the onion channel.
    pub async fn fetch_gif_media(&self, url: &str) -> Result<Vec<u8>, AttachmentError> {
        self.grpc_pool()?
            .gif_client()
            .media(url)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("GIF media fetch failed: {}", e)))
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
            if let Ok((thumb_bytes, thumb_w, thumb_h)) =
                crate::media::ImageProcessor::generate_thumbnail_from_memory(content)
            {
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
                })
                .await
                {
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
            })
            .await
            .unwrap_or(0);
        } else if mime_type.starts_with("image/") {
            // Image processing - use spawn_blocking for large images
            let path_for_dims = file_path.to_path_buf();
            let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                crate::media::ImageProcessor::get_dimensions(&path_for_dims).unwrap_or((0, 0))
            })
            .await
            .unwrap_or((0, 0));
            meta.width = if orig_w > 0 { Some(orig_w) } else { None };
            meta.height = if orig_h > 0 { Some(orig_h) } else { None };

            // Generate thumbnail in blocking thread (large images can take time/memory)
            let path_for_thumb = file_path.to_path_buf();
            match tokio::task::spawn_blocking(move || {
                crate::media::ImageProcessor::generate_thumbnail_bytes(&path_for_thumb)
            })
            .await
            {
                Ok(Ok((bytes, thumb_w, thumb_h))) => {
                    tracing::debug!(
                        "Generated thumbnail for image: {}x{} -> {}x{}, {} bytes",
                        orig_w,
                        orig_h,
                        thumb_w,
                        thumb_h,
                        bytes.len()
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
                })
                .await
                {
                    Ok(Ok((bytes, thumb_w, thumb_h, vid_duration_ms))) => {
                        let path_for_dims = file_path.to_path_buf();
                        let (orig_w, orig_h) = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_dimensions(&path_for_dims)
                                .unwrap_or((thumb_w, thumb_h))
                        })
                        .await
                        .unwrap_or((thumb_w, thumb_h));

                        tracing::debug!(
                            "Generated thumbnail for video: {}x{} -> {}x{}, {} bytes, {}ms",
                            orig_w,
                            orig_h,
                            thumb_w,
                            thumb_h,
                            bytes.len(),
                            vid_duration_ms
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
                        })
                        .await
                        .unwrap_or((0, 0));
                        let vid_duration = tokio::task::spawn_blocking(move || {
                            crate::media::get_video_duration(&path_for_dur).unwrap_or(0)
                        })
                        .await
                        .unwrap_or(0);
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
        if tokio::io::AsyncReadExt::read_exact(&mut f, &mut buf)
            .await
            .is_ok()
        {
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
    if data.starts_with(&[0xFF, 0xFB])
        || data.starts_with(&[0xFF, 0xFA])
        || data.starts_with(b"ID3")
    {
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

#[cfg(test)]
mod size_cap_tests {
    //! Defense-in-depth tests: every public upload entrypoint must enforce the
    //! attachment size cap returned by `max_attachment_size()`. Each test sets
    //! the cap to 100 bytes, calls the entrypoint with a 200-byte payload, and
    //! asserts the returned error round-trips Signal's `AttachmentTooLarge`
    //! display string. The cap is then restored via the captured `original`.
    use super::*;
    use crate::crypto::attachment_crypto::override_max_attachment_size;
    use crate::services::attachment::AttachmentService;
    use crate::storage::database::Database;
    use crate::storage::repositories::AttachmentRepository;
    use std::sync::Arc;

    /// Build a real but offline `AttachmentService`. The size cap check fires
    /// before any gRPC or repository access, so `new_local` (no grpc pool) is
    /// sufficient — the test will never reach those code paths.
    fn make_service() -> (AttachmentService, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("size_cap_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        let repo = Arc::new(AttachmentRepository::new(db));
        (AttachmentService::new_local(repo), temp_dir)
    }

    #[tokio::test]
    async fn upload_to_cdn_encrypted_rejects_oversized_payload() {
        let _cap = override_max_attachment_size(100);

        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 200];
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), &payload).unwrap();

        let result = svc.upload_to_cdn_encrypted(input.path()).await;

        match result {
            Err(AttachmentError::EncryptionError(msg)) => {
                assert!(
                    msg.contains("attachment too large"),
                    "unexpected msg: {}",
                    msg
                );
            }
            other => panic!(
                "expected EncryptionError(AttachmentTooLarge), got {:?}",
                other.as_ref().err().map(|e| e.to_string())
            ),
        }
    }

    #[tokio::test]
    async fn upload_bytes_to_cdn_encrypted_rejects_oversized_payload() {
        let _cap = override_max_attachment_size(100);

        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 200];

        let result = svc
            .upload_bytes_to_cdn_encrypted(&payload, "big.bin", "application/octet-stream")
            .await;

        match result {
            Err(AttachmentError::EncryptionError(msg)) => {
                assert!(
                    msg.contains("attachment too large"),
                    "unexpected msg: {}",
                    msg
                );
            }
            other => panic!(
                "expected EncryptionError(AttachmentTooLarge), got {:?}",
                other.as_ref().err().map(|e| e.to_string())
            ),
        }
    }

    #[tokio::test]
    async fn upload_bytes_under_default_cap_passes_check() {
        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 32];

        let result = svc
            .upload_bytes_to_cdn_encrypted(&payload, "small.bin", "application/octet-stream")
            .await;

        if let Err(AttachmentError::EncryptionError(ref msg)) = result {
            assert!(
                !msg.contains("attachment too large"),
                "small payload incorrectly tripped size cap: {}",
                msg
            );
        }
    }
}

#[cfg(test)]
mod part_range_tests {
    use super::*;

    #[test]
    fn part_ranges_covers_exactly() {
        let total = 40 * 1024 * 1024u64;
        let ps = 16 * 1024 * 1024u64;
        let r = part_ranges(total, ps);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], (0, ps));
        assert_eq!(r[2], (2 * ps, total - 2 * ps));
        let covered: u64 = r.iter().map(|(_, l)| *l).sum();
        assert_eq!(covered, total);
    }
}
