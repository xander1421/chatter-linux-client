//! Upload operations for attachments.

use std::path::Path;

use chatter_attachment_crypto::{AttachmentCrypto, max_attachment_size};
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

// Server currently selects 16 MiB (attachment.go). Allow headroom while
// bounding four concurrent buffered PUTs, including anonymous HTTP transport.
const MAX_MULTIPART_PART_SIZE: u64 = 32 * 1024 * 1024;

fn checked_part_len(len: u64) -> Result<usize, AttachmentError> {
    if len == 0 || len > MAX_MULTIPART_PART_SIZE {
        return Err(AttachmentError::NetworkError(format!(
            "server multipart part size {len} is outside client budget (1..={} bytes); retry with a compatible server",
            MAX_MULTIPART_PART_SIZE
        )));
    }
    usize::try_from(len).map_err(|_| {
        AttachmentError::NetworkError("multipart part size overflows this device".into())
    })
}

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
    // If the route changes, restart from the URL mint on the new route. A
    // presigned URL or multipart upload ID may belong to a different server.
    async fn upload_on_current_route<T, F>(
        &self,
        mut attempt: impl FnMut(bool) -> F,
    ) -> Result<T, AttachmentError>
    where
        F: std::future::Future<Output = Result<T, AttachmentError>> + Send,
    {
        let (tor, generation) = self.cdn_route()?;
        match super::fence_cdn_transfer(generation, AttachmentError::TransportChanged, attempt(tor))
            .await
        {
            Err(AttachmentError::TransportChanged) => {
                let (tor, generation) = self.cdn_route()?;
                super::fence_cdn_transfer(
                    generation,
                    AttachmentError::TransportChanged,
                    attempt(tor),
                )
                .await
            }
            result => result,
        }
    }

    /// PUT one part to its presigned URL (up to three attempts on the same route).
    /// Returns the ETag from the response headers.
    async fn upload_one_part(
        &self,
        url: &str,
        temp_path: &std::path::Path,
        offset: u64,
        len: u64,
        tor: bool,
    ) -> Result<String, AttachmentError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let part_len = checked_part_len(len)?;
        let mut last_err = String::new();
        for attempt in 0..3 {
            // Read the exact byte range fresh each attempt.
            let mut f = tokio::fs::File::open(temp_path)
                .await
                .map_err(|e| AttachmentError::FileError(format!("open temp: {}", e)))?;
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| AttachmentError::FileError(format!("seek: {}", e)))?;
            let mut buf = vec![0u8; part_len];
            f.read_exact(&mut buf)
                .await
                .map_err(|e| AttachmentError::FileError(format!("read part: {}", e)))?;

            match self
                .cdn_http_put_on_route(url, buf, "application/octet-stream", tor)
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
        self.upload_to_cdn_encrypted_with_media(file_path, None, None)
            .await
    }

    /// Recorded voice notes already have these values from the capture worker.
    /// Re-probing the audio starts a decoder and scans it a second time.
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
        self.upload_to_cdn_encrypted_with_media(
            file_path,
            Some(media),
            Some(if cfg!(target_os = "android") {
                "audio/mp4"
            } else {
                "audio/wav"
            }),
        )
        .await
    }

    async fn upload_to_cdn_encrypted_with_media(
        &self,
        file_path: &Path,
        known_media: Option<MediaMetadata>,
        known_mime: Option<&str>,
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

        // Sanitize before encryption. Metadata removal streams to disk, with a
        // bounded pixel decode when an image needs its EXIF orientation baked in.
        let sanitized_temp = {
            let p = file_path.to_path_buf();
            super::run_attachment_work(move || super::sanitize::sanitize_media_file(&p))
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

        let mime_type = match known_mime {
            Some(mime) => mime.to_string(),
            None => detect_mime_type(file_path).await,
        };

        // No filename, object key or plaintext size in upload logs: the same
        // path uploads secret-chat files.
        tracing::debug!(
            mime_type = %mime_type,
            "Encrypting attachment for CDN upload (streaming)"
        );

        // Derive image dimensions and previews from the same pixels we upload.
        let media_start = std::time::Instant::now();
        let media_meta = match known_media {
            Some(media) => media,
            None => {
                let media_path = if mime_type.starts_with("image/") {
                    plaintext_path
                } else {
                    file_path
                };
                self.extract_media_metadata(media_path, &mime_type).await
            }
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
        let encrypt_result = super::run_attachment_work(move || {
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

        // Keep the main object and its optional preview on the same server
        // generation. Upload them in parallel so neither extends the other's
        // time on the wire.
        let ciphertext_size = encrypt_result.ciphertext_size;
        let temp_path = temp_file.path();
        let thumbnail = media_meta.thumbnail.as_deref();
        let (cdn_key, (thumb_cdn_key, thumb_key, thumb_digest, thumb_pt_size)) = self
            .upload_on_current_route(|tor| async move {
                let thumbnail_upload = async {
                    match thumbnail {
                        Some(t) => self
                            .upload_thumbnail_sidecar(t, tor)
                            .await
                            .unwrap_or_default(),
                        None => Default::default(),
                    }
                };
                let main_upload = async {
                    if ciphertext_size > MULTIPART_THRESHOLD {
                        return self
                            .upload_multipart_to_cdn(temp_path, ciphertext_size, tor)
                            .await;
                    }
                    self.upload_single_to_cdn(ciphertext_size, |url| async move {
                        self.put_encrypted_file(&url, temp_path, ciphertext_size, tor)
                            .await
                    })
                    .await
                };
                let (cdn_key, thumbnail) = tokio::join!(main_upload, thumbnail_upload);
                Ok((cdn_key?, thumbnail))
            })
            .await?;

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
            private_channel: self.private_channel,
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

    async fn put_encrypted_file(
        &self,
        url: &str,
        path: &Path,
        ciphertext_size: u64,
        tor: bool,
    ) -> Result<u16, AttachmentError> {
        if tor {
            #[cfg(feature = "onion")]
            {
                // The Tor client's single PUT takes a bounded in-memory body.
                let bytes = tokio::fs::read(path).await.map_err(|e| {
                    AttachmentError::FileError(format!("Failed to read temp file: {e}"))
                })?;
                return self
                    .cdn_http_put_on_route(url, bytes, "application/octet-stream", true)
                    .await
                    .map(|(status, _)| status);
            }
            #[cfg(not(feature = "onion"))]
            return Err(super::no_tor());
        }

        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to open temp file: {e}")))?;
        let stream = tokio_util::io::ReaderStream::new(file);
        let response = super::cdn_client()?
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", ciphertext_size)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("HTTP upload failed: {}", e.without_url()))
            })?;
        Ok(response.status().as_u16())
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
        tor: bool,
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
        let cdn_key = match self
            .upload_ciphertext_to_cdn(&encrypted.ciphertext, tor)
            .await
        {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar upload failed; sending without preview");
                return None;
            }
        };
        tracing::debug!("Thumbnail sidecar uploaded");
        Some((
            cdn_key,
            encrypted.key.to_vec(),
            encrypted.digest.to_vec(),
            plaintext_size,
        ))
    }

    async fn upload_ciphertext_to_cdn(
        &self,
        ciphertext: &[u8],
        tor: bool,
    ) -> Result<String, AttachmentError> {
        self.upload_single_to_cdn(ciphertext.len() as u64, |url| async move {
            self.cdn_http_put_on_route(&url, ciphertext.to_vec(), "application/octet-stream", tor)
                .await
                .map(|(status, _)| status)
        })
        .await
    }

    pub(crate) async fn upload_encrypted_blob_to_cdn(
        &self,
        ciphertext: &[u8],
    ) -> Result<String, AttachmentError> {
        self.upload_on_current_route(|tor| self.upload_ciphertext_to_cdn(ciphertext, tor))
            .await
    }

    async fn upload_single_to_cdn<F>(
        &self,
        ciphertext_size: u64,
        put: impl FnOnce(String) -> F,
    ) -> Result<String, AttachmentError>
    where
        F: std::future::Future<Output = Result<u16, AttachmentError>> + Send,
    {
        let mut file_client = self.file_client()?;
        let minted_at = std::time::Instant::now();
        let response = file_client
            .get_attachment_upload_url(ciphertext_size)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get upload URL: {e}")))?;
        if self.secret_chat {
            tracing::info!(
                elapsed_ms = minted_at.elapsed().as_millis(),
                "secret attachment upload: URL minted"
            );
        }

        let upload_start = std::time::Instant::now();
        let status = put(response.upload_url).await?;
        if !(200..300).contains(&status) {
            return Err(AttachmentError::NetworkError(format!(
                "CDN upload failed with status: {status}"
            )));
        }
        tracing::info!(elapsed = ?upload_start.elapsed(), "CDN upload complete");

        let confirm_start = std::time::Instant::now();
        file_client
            .confirm_attachment_upload(&response.cdn_key)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to confirm upload: {e}")))?;
        if self.secret_chat {
            tracing::info!(
                elapsed_ms = confirm_start.elapsed().as_millis(),
                "secret attachment upload: confirmed"
            );
        }
        Ok(response.cdn_key)
    }

    /// Upload an already-encrypted temp file via multipart, direct to RustFS.
    /// Returns the confirmed cdn_key. Bytes never touch the app server.
    async fn upload_multipart_to_cdn(
        &self,
        temp_path: &std::path::Path,
        ciphertext_size: u64,
        tor: bool,
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
        if let Err(error) = checked_part_len(part_size) {
            let _ = file_client
                .abort_attachment_multipart_upload(&cdn_key, &upload_id)
                .await;
            return Err(error);
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
                let etag = self
                    .upload_one_part(&url, &temp_path, offset, len, tor)
                    .await?;
                Ok::<chatter_proto::chat_v1::CompletedPart, AttachmentError>(
                    chatter_proto::chat_v1::CompletedPart {
                        number: part_number,
                        etag,
                    },
                )
            }
        });

        let results: Vec<Result<chatter_proto::chat_v1::CompletedPart, AttachmentError>> =
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

        let media_meta = self.extract_media_metadata_from_bytes(content, mime_type);
        let ciphertext = encrypted.ciphertext.as_slice();
        let thumbnail = media_meta.thumbnail.as_deref();
        let (cdn_key, (thumb_cdn_key, thumb_key, thumb_digest, thumb_pt_size)) = self
            .upload_on_current_route(|tor| async move {
                let cdn_key = self.upload_ciphertext_to_cdn(ciphertext, tor).await?;
                let thumbnail = match thumbnail {
                    Some(t) => self
                        .upload_thumbnail_sidecar(t, tor)
                        .await
                        .unwrap_or_default(),
                    None => Default::default(),
                };
                Ok((cdn_key, thumbnail))
            })
            .await?;

        let attachment_id = AttachmentId::from_cdn_key(&cdn_key);

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
            private_channel: self.private_channel,
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
                meta.waveform = match super::run_attachment_work(move || {
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
            meta.duration_ms = super::run_attachment_work(move || {
                crate::media::AudioPlayer::get_duration(&path_for_duration)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            })
            .await
            .unwrap_or(0);
        } else if mime_type.starts_with("image/") {
            // Image processing - use spawn_blocking for large images
            let path_for_dims = file_path.to_path_buf();
            let (orig_w, orig_h) = super::run_attachment_work(move || {
                crate::media::ImageProcessor::get_dimensions(&path_for_dims).unwrap_or((0, 0))
            })
            .await
            .unwrap_or((0, 0));
            meta.width = if orig_w > 0 { Some(orig_w) } else { None };
            meta.height = if orig_h > 0 { Some(orig_h) } else { None };

            // Generate thumbnail in blocking thread (large images can take time/memory)
            let path_for_thumb = file_path.to_path_buf();
            match super::run_attachment_work(move || {
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
            // Desktop uses GStreamer; Android installs its platform metadata
            // extractor at startup. Both populate the same attachment fields.
            #[cfg(target_os = "android")]
            {
                let path_for_video = file_path.to_path_buf();
                match super::run_attachment_work(move || {
                    crate::media::analyze_video(&path_for_video)
                })
                .await
                {
                    Ok(Ok(video)) => {
                        if !video.thumbnail.is_empty() {
                            meta.thumbnail = Some(video.thumbnail);
                            meta.thumbnail_width = Some(video.thumbnail_width);
                            meta.thumbnail_height = Some(video.thumbnail_height);
                        } else {
                            tracing::warn!("Android video metadata contained no thumbnail frame");
                        }
                        meta.width = Some(video.width);
                        meta.height = Some(video.height);
                        meta.duration_ms = video.duration_ms;
                        tracing::info!(
                            width = video.width,
                            height = video.height,
                            has_thumbnail = meta.thumbnail.is_some(),
                            "Android video metadata extracted"
                        );
                    }
                    Ok(Err(e)) => tracing::warn!("Failed to analyze Android video: {e}"),
                    Err(e) => tracing::warn!("Android video metadata task panicked: {e}"),
                }
            }
            #[cfg(feature = "desktop-gstreamer")]
            {
                let path_for_thumb = file_path.to_path_buf();
                match super::run_attachment_work(move || {
                    crate::media::extract_video_thumbnail(&path_for_thumb)
                })
                .await
                {
                    Ok(Ok((bytes, thumb_w, thumb_h, vid_duration_ms))) => {
                        let path_for_dims = file_path.to_path_buf();
                        let (orig_w, orig_h) = super::run_attachment_work(move || {
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
                        let (orig_w, orig_h) = super::run_attachment_work(move || {
                            crate::media::get_video_dimensions(&path_for_dims).unwrap_or((0, 0))
                        })
                        .await
                        .unwrap_or((0, 0));
                        let vid_duration = super::run_attachment_work(move || {
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
    use chatter_attachment_crypto::override_max_attachment_size;
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

    #[tokio::test]
    async fn upload_restarts_once_after_its_route_is_retired() {
        use crate::network::grpc::client_pool::GrpcClientPool;
        use crate::storage::{database::Database, repositories::AttachmentRepository};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("attachments.db"), "testpass").unwrap());
        let pool = Arc::new(GrpcClientPool::new("https://localhost:50051").unwrap());
        let service = AttachmentService::new(Arc::new(AttachmentRepository::new(db)), pool.clone());
        let pool = pool.as_ref();
        let mut attempts = 0;
        let upload = service.upload_on_current_route(|tor| {
            assert!(!tor);
            attempts += 1;
            let first = attempts == 1;
            async move {
                if first {
                    pool.set_url("https://localhost:50052").unwrap();
                    return std::future::pending().await;
                }
                Ok(())
            }
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), upload)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(attempts, 2);

        attempts = 0;
        let result: Result<(), _> = service
            .upload_on_current_route(|_| {
                attempts += 1;
                std::future::ready(Err(AttachmentError::NetworkError(
                    "unexpected HTTP error".into(),
                )))
            })
            .await;
        assert!(matches!(result, Err(AttachmentError::NetworkError(_))));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn multipart_part_bytes_are_bounded_before_allocation() {
        assert_eq!(
            checked_part_len(16 * 1024 * 1024).unwrap(),
            16 * 1024 * 1024
        );
        assert_eq!(
            checked_part_len(MAX_MULTIPART_PART_SIZE).unwrap() as u64,
            MAX_MULTIPART_PART_SIZE
        );
        for invalid in [
            0,
            MAX_MULTIPART_PART_SIZE + 1,
            2 * 1024 * 1024 * 1024,
            u64::MAX,
        ] {
            assert!(checked_part_len(invalid).is_err());
        }
    }

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
