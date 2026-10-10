//! Attachment service module.
//!
//! Upload, download, share, and encrypt attachments.

mod crypto;
mod download;
mod media;
mod range_decrypt;
mod sanitize;
mod upload;

pub use media::{
    GifDecodeResult, GifFullDecodeResult, ImageDecodeResult, ThumbnailOwner,
    register_video_thumbnail_extractor,
};
pub use range_decrypt::{CbcRangeDecryptor, CbcRangeReader};
pub use sanitize::{SanitizeOutcome, sanitize_media, sanitize_media_file, validate_media};

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::domain::attachment::{Attachment, ContentLocation};
use crate::domain::ids::AttachmentId;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::content_v1::AttachmentPointer;
use crate::storage::repositories::AttachmentRepository;

// Submodules add impl blocks to AttachmentService
// No re-exports needed - methods are on the struct

/// The transfer needs Tor and this build has none (`cdn_over_tor` only says
/// so for a secret chat, which it already refuses without a Tor route).
#[cfg(not(feature = "onion"))]
fn no_tor() -> AttachmentError {
    AttachmentError::NetworkError(
        crate::error::NetworkError::SecretChatNeedsTor("this build has none").to_string(),
    )
}

/// Use the common pinned storage policy for every direct CDN transfer.
fn cdn_client() -> Result<reqwest::Client, AttachmentError> {
    crate::network::storage_http::storage_client().map_err(AttachmentError::NetworkError)
}

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
    /// Decoded thumbnail bytes - only for images/videos (kept for the sender's
    /// own local preview; not sent inline).
    pub thumbnail: Vec<u8>,
    /// Thumbnail sidecar reference: the thumbnail is uploaded as its own small
    /// encrypted CDN object so the pointer fits the sealed-DM budget on every
    /// transport. Empty when there is no thumbnail.
    pub thumbnail_cdn_key: String,
    pub thumbnail_key: Vec<u8>,
    pub thumbnail_digest: Vec<u8>,
    pub thumbnail_plaintext_size: u32,
    /// Thumbnail width.
    pub thumbnail_width: u32,
    /// Thumbnail height.
    pub thumbnail_height: u32,
    /// Original image/video dimensions.
    pub width: u32,
    pub height: u32,
    /// Total ciphertext size in bytes.
    pub ciphertext_size: u64,
}

impl CdnUploadResult {
    /// Convert to AttachmentPointer proto.
    pub fn to_attachment_pointer(&self) -> AttachmentPointer {
        AttachmentPointer {
            cdn_key: self.cdn_key.clone(),
            encryption_key: self.encryption_key.to_vec(),
            digest: self.digest.to_vec(),
            plaintext_size: self.plaintext_size,
            content_type: self.content_type.clone(),
            file_name: self.file_name.clone(),
            waveform: self.waveform.clone(),
            duration_ms: self.duration_ms,
            thumbnail_cdn_key: self.thumbnail_cdn_key.clone(),
            thumbnail_key: self.thumbnail_key.clone(),
            thumbnail_digest: self.thumbnail_digest.clone(),
            thumbnail_plaintext_size: self.thumbnail_plaintext_size,
            thumbnail_width: self.thumbnail_width,
            thumbnail_height: self.thumbnail_height,
            width: self.width,
            height: self.height,
            ciphertext_size: self.ciphertext_size,
            ..Default::default()
        }
    }
}

// ============================================================================
// Filesystem Storage Helpers
// ============================================================================

/// Override for the attachments base directory.
/// Set during init on Android where the platform data-local dir is read-only.
static ATTACHMENTS_BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set the base directory for attachment storage.
/// Call once during app init with the writable data directory.
pub fn set_attachments_base_dir(dir: PathBuf) {
    let _ = ATTACHMENTS_BASE.set(dir);
}

/// Get the attachments directory path.
///
/// Uses the override set by `set_attachments_base_dir()` if available,
/// otherwise falls back to `~/.local/share/chatter/attachments/` on desktop.
pub fn attachments_dir() -> PathBuf {
    if let Some(base) = ATTACHMENTS_BASE.get() {
        base.join("attachments")
    } else {
        directories::BaseDirs::new()
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("chatter")
            .join("attachments")
    }
}

/// Get the path for an encrypted attachment file.
///
/// Returns `~/.local/share/chatter/attachments/{attachment_id}.enc`
pub fn attachment_file_path(attachment_id: &AttachmentId) -> PathBuf {
    attachments_dir().join(format!("{}.enc", attachment_id))
}

/// Ensure the attachments directory exists with secure permissions.
///
/// Creates `~/.local/share/chatter/attachments/` with mode 0700 (owner only).
/// Safe to call multiple times.
pub fn ensure_attachments_dir() -> std::io::Result<()> {
    let dir = attachments_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
        tracing::info!("Created attachments directory: {}", dir.display());
    }

    // Always ensure correct permissions (in case dir existed with wrong perms)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&dir, permissions)?;
    }

    Ok(())
}

/// Attachment service for file operations.
///
/// The `grpc_pool` field is required for upload/download operations but
/// optional for local-only use (storing records from incoming messages).
/// Decoded image pixels cached for fast UI render.
/// Send+Sync so it can live in a shared cache without touching UI types.
#[derive(Clone)]
pub struct DecodedImagePixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct AttachmentService {
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    pub(crate) grpc_pool: Option<Arc<GrpcClientPool>>,
    /// Serving a secret chat (anonymous DM / group): every URL mint and CDN
    /// transfer goes over Tor, whatever the pool's transport, or fails.
    pub(crate) secret_chat: bool,
}

impl AttachmentService {
    /// Get the attachment repository.
    pub fn attachment_repo(&self) -> Arc<AttachmentRepository> {
        self.attachment_repo.clone()
    }

    /// Get the gRPC pool, returns error if not configured.
    pub(crate) fn grpc_pool(&self) -> Result<&GrpcClientPool, AttachmentError> {
        self.grpc_pool.as_ref().map(|p| p.as_ref()).ok_or_else(|| {
            AttachmentError::NetworkError(
                "gRPC pool not configured — cannot perform network operations".into(),
            )
        })
    }

    /// This service, routed for a secret chat: URL mints over the pool's
    /// secret channel and CDN transfers over Tor, failing closed when there is
    /// no Tor route.
    pub fn for_secret_chat(&self) -> Self {
        Self {
            secret_chat: true,
            ..self.clone()
        }
    }

    /// This service routed for `conversation`: [`Self::for_secret_chat`] for
    /// an anonymous DM or group, unchanged otherwise.
    pub fn for_conversation(
        &self,
        conversation: &crate::state::chat_state::ActiveConversation,
    ) -> Self {
        if conversation.is_anon() {
            self.for_secret_chat()
        } else {
            self.clone()
        }
    }

    /// The service to download `attachment_id` with: routed for a secret chat
    /// when the row's [`Attachment::secret_chat`] marker says it was created
    /// for one.
    ///
    /// The marker is written wherever a row is created, so it is unknown only
    /// on a row stored before it existed (migration v101). Such a row goes
    /// over Tor: an unknown origin never risks a secret-chat download on the
    /// clearnet.
    ///
    /// [`Attachment::secret_chat`]: crate::domain::attachment::Attachment::secret_chat
    fn for_download(&self, attachment_id: &AttachmentId) -> Result<Self, AttachmentError> {
        if self.secret_chat {
            return Ok(self.clone());
        }
        let storage = |e: crate::error::StorageError| AttachmentError::StorageError(e.to_string());
        let secret = match self.attachment_repo.get(attachment_id).map_err(storage)? {
            Some(attachment) => match attachment.secret_chat {
                Some(secret) => secret,
                None => true,
            },
            // No row: the transfer fails on its own lookup before any request.
            None => false,
        };
        Ok(if secret {
            self.for_secret_chat()
        } else {
            self.clone()
        })
    }

    /// File client for URL mints: the secret-chat client (no auth header,
    /// Tor) for a secret chat, else the pool's transport-following one.
    pub(crate) fn file_client(&self) -> Result<crate::network::grpc::FileClient, AttachmentError> {
        let pool = self.grpc_pool()?;
        if self.secret_chat {
            return pool
                .secret_file_client()
                .map_err(|e| AttachmentError::NetworkError(e.to_string()));
        }
        Ok(pool.file_client())
    }

    /// Whether CDN HTTP must be tunnelled over Tor, never sent direct (a direct
    /// request would leak the real IP): always for a secret chat, else when
    /// the pool is routing over Tor (anon mode). A secret chat with no Tor
    /// route is an error, never a direct request.
    pub(crate) fn cdn_over_tor(&self) -> Result<bool, AttachmentError> {
        if self.secret_chat {
            let pool = self.grpc_pool()?;
            if !pool.secret_route_available() {
                return Err(AttachmentError::NetworkError(
                    pool.secret_channel()
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "secret chats need Tor (onion)".into()),
                ));
            }
            return Ok(true);
        }
        Ok(self.grpc_pool.as_ref().is_some_and(|p| {
            p.transport_mode() == crate::network::grpc::client_pool::TransportMode::Onion
        }))
    }

    /// GET a presigned/CDN URL, over Tor when [`Self::cdn_over_tor`] and
    /// direct otherwise.
    ///
    /// Returns the full body on 2xx, else a `NetworkError` carrying the status —
    /// same fail-on-non-2xx semantics the direct path had. Over Tor this
    /// tunnels through the pool's Tor client (real IP hidden); it never falls
    /// back to a direct request, so the leak stays closed.
    pub(crate) async fn cdn_http_get(&self, url: &str) -> Result<Vec<u8>, AttachmentError> {
        if self.cdn_over_tor()? {
            #[cfg(feature = "onion")]
            {
                let tor = self.grpc_pool()?.onion_tor();
                return crate::network::anon_http::get(&tor, url)
                    .await
                    .map_err(AttachmentError::NetworkError);
            }
            #[cfg(not(feature = "onion"))]
            return Err(no_tor());
        }

        let mut response = cdn_client()?.get(url).send().await.map_err(|e| {
            AttachmentError::NetworkError(format!("HTTP download failed: {}", e.without_url()))
        })?;
        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(format!(
                "CDN download failed with status: {}",
                response.status()
            )));
        }
        let limit = crate::network::anon_http::storage_response_limit();
        if response
            .content_length()
            .is_some_and(|len| len > limit as u64)
        {
            return Err(AttachmentError::NetworkError(
                "storage response exceeds attachment size limit".into(),
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            AttachmentError::NetworkError(format!("Failed to read response: {}", e.without_url()))
        })? {
            crate::network::anon_http::append_storage_chunk(&mut body, &chunk, limit)
                .map_err(AttachmentError::NetworkError)?;
        }
        Ok(body)
    }

    /// Stream ciphertext into a private staging file without retaining the
    /// entire object in RAM. Routing and trust match the in-memory CDN path.
    pub(crate) async fn cdn_http_get_to_file(
        &self,
        url: &str,
        file: &mut tokio::fs::File,
    ) -> Result<u64, AttachmentError> {
        let limit = crate::network::anon_http::storage_response_limit() as u64;
        if self.cdn_over_tor()? {
            #[cfg(feature = "onion")]
            {
                let tor = self.grpc_pool()?.onion_tor();
                return crate::network::anon_http::get_to_file(&tor, url, file, limit)
                    .await
                    .map_err(AttachmentError::NetworkError);
            }
            #[cfg(not(feature = "onion"))]
            return Err(no_tor());
        }

        let mut response = cdn_client()?
            .get(url)
            .send()
            .await
            .map_err(|_| AttachmentError::NetworkError("storage download failed".into()))?;
        if !response.status().is_success() {
            return Err(AttachmentError::NetworkError(format!(
                "storage GET returned status {}",
                response.status()
            )));
        }
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(AttachmentError::NetworkError(
                "storage response exceeds attachment size limit".into(),
            ));
        }
        let mut total = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AttachmentError::NetworkError("storage response read failed".into()))?
        {
            total = crate::network::anon_http::write_storage_chunk(file, total, &chunk, limit)
                .await
                .map_err(AttachmentError::NetworkError)?;
        }
        Ok(total)
    }

    /// PUT `body` to a presigned URL, over Tor when [`Self::cdn_over_tor`] and
    /// direct otherwise.
    ///
    /// Returns `(status, etag)`; the caller decides how to treat the status
    /// (single PUTs error on non-2xx, multipart parts retry). Never falls back to
    /// a direct request when Tor is required.
    pub(crate) async fn cdn_http_put(
        &self,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(u16, Option<String>), AttachmentError> {
        if self.cdn_over_tor()? {
            #[cfg(feature = "onion")]
            {
                let tor = self.grpc_pool()?.onion_tor();
                let resp = crate::network::anon_http::put(&tor, url, body, content_type)
                    .await
                    .map_err(AttachmentError::NetworkError)?;
                return Ok((resp.status, resp.etag));
            }
            #[cfg(not(feature = "onion"))]
            return Err(no_tor());
        }

        let content_length = body.len();
        let response = cdn_client()?
            .put(url)
            .header("Content-Type", content_type)
            .header("Content-Length", content_length)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("HTTP upload failed: {}", e.without_url()))
            })?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| !s.is_empty());
        Ok((status, etag))
    }

    /// Create a new AttachmentService with network capability.
    pub fn new(attachment_repo: Arc<AttachmentRepository>, grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self {
            attachment_repo,
            grpc_pool: Some(grpc_pool),
            secret_chat: false,
        }
    }

    /// Create a local-only AttachmentService (no upload/download).
    ///
    /// Used by services that only need to store attachment records from incoming messages.
    pub fn new_local(attachment_repo: Arc<AttachmentRepository>) -> Self {
        Self {
            attachment_repo,
            grpc_pool: None,
            secret_chat: false,
        }
    }

    /// Get an attachment by ID from local storage.
    pub fn get(&self, attachment_id: &AttachmentId) -> Result<Option<Attachment>, AttachmentError> {
        self.attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
    }

    /// Update an attachment in local storage.
    pub fn update(&self, attachment: &Attachment) -> Result<(), AttachmentError> {
        self.attachment_repo
            .update(attachment)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
    }

    /// Create attachment records from incoming message pointers.
    ///
    /// This is the single entry point for creating attachment records from
    /// received messages. Called by inbox handlers after message decryption.
    ///
    /// Returns the IDs of every pointer's row — created now or already
    /// held. A row already held (the same CDN key from another conversation,
    /// or a concurrent receipt that won the insert) keeps its data, and a
    /// secret chat's receipt raises its mark so its transfers go over Tor
    /// (see [`AttachmentRepository::insert_or_raise_secret`]). Any storage
    /// failure is an error and no ID is returned: a pointer whose origin was
    /// not persisted must not be published as stored, or a later download
    /// could trust a stale ordinary origin.
    ///
    /// [`AttachmentRepository::insert_or_raise_secret`]: crate::storage::repositories::attachment_repo::AttachmentRepository::insert_or_raise_secret
    pub fn create_from_pointers(
        &self,
        pointers: &[AttachmentPointer],
    ) -> Result<Vec<AttachmentId>, crate::error::StorageError> {
        use crate::domain::attachment::{Attachment, AttachmentStatus, FileMetadata};

        let mut ids = Vec::with_capacity(pointers.len());

        for pointer in pointers {
            if pointer.cdn_key.is_empty() {
                continue;
            }

            let att_id = AttachmentId::from_cdn_key(&pointer.cdn_key);

            let attachment = Attachment {
                id: att_id,
                metadata: FileMetadata {
                    name: pointer.file_name.clone(),
                    mime_type: pointer.content_type.clone(),
                    size: pointer.plaintext_size,
                    sha256: pointer.digest.clone(),
                    chunk_size: 0,
                    width: if pointer.width > 0 {
                        Some(pointer.width)
                    } else {
                        None
                    },
                    height: if pointer.height > 0 {
                        Some(pointer.height)
                    } else {
                        None
                    },
                    duration_seconds: if pointer.duration_ms > 0 {
                        Some(pointer.duration_ms as f64 / 1000.0)
                    } else {
                        None
                    },
                    waveform_samples: if pointer.waveform.is_empty() {
                        None
                    } else {
                        Some(pointer.waveform.clone())
                    },
                    // Thumbnail bytes are fetched lazily from the sidecar (see
                    // ensure_thumbnail); only the reference lands here on receive.
                    thumbnail: None,
                    thumbnail_width: if pointer.thumbnail_width > 0 {
                        Some(pointer.thumbnail_width)
                    } else {
                        None
                    },
                    thumbnail_height: if pointer.thumbnail_height > 0 {
                        Some(pointer.thumbnail_height)
                    } else {
                        None
                    },
                    thumbnail_cdn_key: if pointer.thumbnail_cdn_key.is_empty() {
                        None
                    } else {
                        Some(pointer.thumbnail_cdn_key.clone())
                    },
                    thumbnail_key: if pointer.thumbnail_key.is_empty() {
                        None
                    } else {
                        Some(pointer.thumbnail_key.clone())
                    },
                    thumbnail_digest: if pointer.thumbnail_digest.is_empty() {
                        None
                    } else {
                        Some(pointer.thumbnail_digest.clone())
                    },
                    thumbnail_plaintext_size: if pointer.thumbnail_plaintext_size > 0 {
                        Some(pointer.thumbnail_plaintext_size)
                    } else {
                        None
                    },
                },
                status: AttachmentStatus::Pending,
                content_location: ContentLocation::NotDownloaded,
                remote_url: Some(pointer.cdn_key.clone()),
                encryption_key: Some(pointer.encryption_key.clone()),
                encryption_iv: Some(pointer.digest.clone()),
                created_at: chrono::Utc::now(),
                secret_chat: Some(self.secret_chat),
            };

            let inserted = self.attachment_repo.insert_or_raise_secret(&attachment)?;
            ids.push(att_id);
            if self.secret_chat {
                // Nothing about a secret chat's file reaches the log.
                tracing::debug!(inserted, "Stored secret-chat attachment record");
            } else {
                tracing::info!(
                    inserted,
                    content_type = %pointer.content_type,
                    "Stored attachment record from pointer"
                );
            }
        }

        Ok(ids)
    }

    /// Get preview image data (decoded RGBA) from stored thumbnail.
    ///
    /// Returns (rgba_bytes, width, height) if thumbnail is available and decodable.
    /// This is fast and synchronous - use for instant preview while full image loads.
    pub fn get_preview_image(&self, attachment_id: &AttachmentId) -> Option<(Vec<u8>, u32, u32)> {
        let attachment = self.attachment_repo.get(attachment_id).ok()??;
        let thumbnail_bytes = attachment.metadata.thumbnail.as_ref()?;

        if thumbnail_bytes.is_empty() {
            return None;
        }

        // Thumbnail bytes come from the sender's AttachmentPointer (untrusted).
        // Reject malformed WebP before the image parser sees it.
        if let Err(e) = sanitize::validate_media(thumbnail_bytes) {
            tracing::warn!(
                error = %e,
                "Rejected malformed thumbnail"
            );
            return None;
        }

        // Decode thumbnail to RGBA
        match image::load_from_memory(thumbnail_bytes) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                tracing::debug!(
                    width = width,
                    height = height,
                    "Decoded preview image from thumbnail"
                );
                Some((rgba.into_raw(), width, height))
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "Failed to decode thumbnail for preview"
                );
                None
            }
        }
    }

    /// Fetch + decrypt the thumbnail sidecar for a received attachment and cache
    /// the decoded bytes into the local record, so the receiver gets a preview
    /// WITHOUT downloading the (potentially huge) full blob. Idempotent and
    /// cheap: returns immediately if the thumbnail is already present locally.
    ///
    /// Returns the decoded thumbnail bytes (JPEG/WebP), or `None` when the
    /// attachment has no thumbnail sidecar or the fetch/decrypt fails.
    pub async fn ensure_thumbnail(&self, attachment_id: &AttachmentId) -> Option<Vec<u8>> {
        self.for_download(attachment_id)
            .ok()?
            .ensure_thumbnail_routed(attachment_id)
            .await
    }

    async fn ensure_thumbnail_routed(&self, attachment_id: &AttachmentId) -> Option<Vec<u8>> {
        let mut attachment = self.attachment_repo.get(attachment_id).ok()??;

        // Already have the decoded bytes locally (sender's own upload, or a
        // previous fetch) — no network.
        if let Some(t) = &attachment.metadata.thumbnail {
            if !t.is_empty() {
                return Some(t.clone());
            }
        }

        let cdn_key = attachment.metadata.thumbnail_cdn_key.clone()?;
        let key_vec = attachment.metadata.thumbnail_key.clone()?;
        let digest_vec = attachment.metadata.thumbnail_digest.clone()?;
        let plaintext_size = attachment.metadata.thumbnail_plaintext_size? as usize;
        if key_vec.len() != KEY_SET_LENGTH || digest_vec.len() != 32 {
            tracing::warn!("Thumbnail sidecar refs malformed");
            return None;
        }

        // Download the small encrypted thumbnail blob.
        let mut file_client = self.file_client().ok()?;
        let dl = file_client
            .get_attachment_download_url(&cdn_key)
            .await
            .ok()?;
        // Over Tor in anon mode; cdn_http_get already fails on non-2xx.
        let ciphertext = match self.cdn_http_get(&dl.download_url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar GET failed");
                return None;
            }
        };

        // Decrypt — verifies whole-ciphertext digest + trailing HMAC.
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(&key_vec);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_vec);
        let plaintext = match crate::crypto::attachment_crypto::AttachmentCrypto::decrypt(
            &ciphertext,
            &key,
            &digest,
            plaintext_size,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "Thumbnail sidecar decrypt failed");
                return None;
            }
        };

        // Cache into the local record for instant subsequent previews.
        attachment.metadata.thumbnail = Some(plaintext.clone());
        if let Err(e) = self.attachment_repo.update(&attachment) {
            tracing::debug!(error = %e, "Failed to persist fetched thumbnail (continuing)");
        }
        tracing::debug!(bytes = plaintext.len(), "Fetched thumbnail sidecar");
        Some(plaintext)
    }

    /// Get attachment metadata without loading content.
    pub fn get_metadata(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Option<crate::domain::attachment::Attachment>, AttachmentError> {
        self.attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
    }
}

#[cfg(test)]
mod waveform_tests {
    use super::*;

    #[test]
    fn incoming_voice_pointer_preserves_waveform_in_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::database::Database::open(&dir.path().join("waveform.db"), "testpass")
                .unwrap(),
        );
        db.run_migrations().unwrap();
        let service = AttachmentService::new_local(Arc::new(AttachmentRepository::new(db)));
        let pointer = AttachmentPointer {
            cdn_key: "voice-waveform-test".into(),
            content_type: "audio/wav".into(),
            file_name: "Voice message.wav".into(),
            voice_note: true,
            waveform: vec![0, 13, 128, 255],
            duration_ms: 1250,
            ..Default::default()
        };
        let ids = service
            .create_from_pointers(std::slice::from_ref(&pointer))
            .unwrap();
        let stored = service.get(&ids[0]).unwrap().unwrap();
        assert_eq!(
            stored.metadata.waveform_samples,
            Some(pointer.waveform.clone())
        );
        assert_eq!(stored.metadata.duration_seconds, Some(1.25));
        assert_eq!(service.create_from_pointers(&[pointer]).unwrap(), ids);
        assert_eq!(
            service
                .get(&ids[0])
                .unwrap()
                .unwrap()
                .metadata
                .waveform_samples,
            stored.metadata.waveform_samples
        );
    }

    #[test]
    fn stored_voice_note_pointer_carries_its_waveform() {
        let mut meta = crate::domain::attachment::FileMetadata::new(
            "Voice message.wav".into(),
            "audio/wav".into(),
            4,
        );
        meta.waveform_samples = Some(vec![13, 128, 255]);
        let mut att = crate::domain::attachment::Attachment::new(AttachmentId::new(), meta);
        att.remote_url = Some("cdn".into());
        att.encryption_key = Some(vec![1; 64]);
        att.encryption_iv = Some(vec![2; 32]);
        let pointer = att.to_pointer().unwrap();
        assert_eq!(pointer.waveform, vec![13, 128, 255]);
        assert!(pointer.voice_note);
    }
}

#[cfg(test)]
mod secret_chat_origin_tests {
    use super::*;

    fn service() -> (AttachmentService, Arc<crate::storage::Database>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::database::Database::open(&dir.path().join("origin.db"), "testpass")
                .unwrap(),
        );
        db.run_migrations().unwrap();
        let service = AttachmentService::new_local(Arc::new(AttachmentRepository::new(db.clone())));
        (service, db, dir)
    }

    fn pointer(cdn_key: &str) -> AttachmentPointer {
        AttachmentPointer {
            cdn_key: cdn_key.into(),
            content_type: "image/jpeg".into(),
            file_name: "photo.jpg".into(),
            ..Default::default()
        }
    }

    fn routed_secret(service: &AttachmentService, id: &AttachmentId) -> bool {
        service.for_download(id).unwrap().secret_chat
    }

    /// The receive path marks the row for the conversation it was created
    /// in, and downloads route by that marker — not by whether the
    /// shared-content index happened to record the message.
    #[test]
    fn received_rows_carry_their_origin_and_route_by_it() {
        let (service, _db, _dir) = service();
        let secret = service
            .for_secret_chat()
            .create_from_pointers(&[pointer("secret-key")])
            .unwrap()[0];
        let plain = service.create_from_pointers(&[pointer("plain-key")]).unwrap()[0];

        assert_eq!(service.get(&secret).unwrap().unwrap().secret_chat, Some(true));
        assert_eq!(service.get(&plain).unwrap().unwrap().secret_chat, Some(false));
        assert!(routed_secret(&service, &secret), "secret-chat row left Tor");
        assert!(!routed_secret(&service, &plain));
    }

    /// A CDN key already stored for an ordinary conversation and then received
    /// in a secret chat goes over Tor from then on; an ordinary receive of a
    /// secret row never clears the mark.
    #[test]
    fn the_secret_mark_is_one_way() {
        let (service, _db, _dir) = service();
        let id = service.create_from_pointers(&[pointer("shared-key")]).unwrap()[0];
        service
            .for_secret_chat()
            .create_from_pointers(&[pointer("shared-key")])
            .unwrap();
        assert_eq!(service.get(&id).unwrap().unwrap().secret_chat, Some(true));
        service.create_from_pointers(&[pointer("shared-key")]).unwrap();
        assert_eq!(service.get(&id).unwrap().unwrap().secret_chat, Some(true));
        assert!(routed_secret(&service, &id));
    }

    /// A row from before the marker existed (NULL) goes over Tor.
    #[test]
    fn unknown_origin_goes_over_tor() {
        let (service, db, _dir) = service();
        let id = service.create_from_pointers(&[pointer("legacy-key")]).unwrap()[0];
        db.connection()
            .unwrap()
            .execute("UPDATE attachments SET secret_chat = NULL", [])
            .unwrap();
        assert_eq!(service.get(&id).unwrap().unwrap().secret_chat, None);
        assert!(routed_secret(&service, &id));
        // An ordinary receipt of that key resolves "unknown" to secret,
        // never to ordinary.
        service.create_from_pointers(&[pointer("legacy-key")]).unwrap();
        assert_eq!(service.get(&id).unwrap().unwrap().secret_chat, Some(true));
    }

    /// Normal and secret receipts of the same unknown CDN key racing: the
    /// row ends secret whichever wins the insert.
    #[test]
    fn concurrent_normal_and_secret_receipts_end_secret() {
        let (service, _db, _dir) = service();
        for round in 0..50 {
            let key = format!("race-{round}");
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = [service.clone(), service.for_secret_chat()]
                .into_iter()
                .map(|svc| {
                    let (barrier, key) = (barrier.clone(), key.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        svc.create_from_pointers(&[pointer(&key)]).unwrap()[0]
                    })
                })
                .collect();
            let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(ids[0], ids[1]);
            assert_eq!(
                service.get(&ids[0]).unwrap().unwrap().secret_chat,
                Some(true),
                "round {round}"
            );
            assert!(routed_secret(&service, &ids[0]));
        }
    }

    /// A secret receipt whose mark cannot be written is an error, and no
    /// pointer ID is handed back as stored — likewise a failed insert.
    #[test]
    fn a_failed_origin_write_publishes_no_pointer() {
        let (service, db, _dir) = service();
        let id = service.create_from_pointers(&[pointer("held-key")]).unwrap()[0];
        db.connection()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_mark BEFORE UPDATE OF secret_chat ON attachments
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;
                 CREATE TRIGGER fail_insert BEFORE INSERT ON attachments
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let secret = service.for_secret_chat();
        assert!(secret.create_from_pointers(&[pointer("held-key")]).is_err());
        assert!(
            secret
                .create_from_pointers(&[pointer("new-key"), pointer("held-key")])
                .is_err()
        );
        // The ordinary origin stands, but nothing claimed the secret copy
        // was stored, so no secret message references it.
        assert_eq!(service.get(&id).unwrap().unwrap().secret_chat, Some(false));
    }
}

#[cfg(test)]
mod cdn_security_tests {
    #[tokio::test]
    async fn direct_cdn_rejects_cleartext_before_connecting() {
        let error = super::cdn_client()
            .unwrap()
            .get("http://127.0.0.1:1/object?secret=capability")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_builder());
    }
}
