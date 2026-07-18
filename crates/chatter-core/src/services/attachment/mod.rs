//! Attachment service module.
//!
//! Upload, download, share, and encrypt attachments.

mod crypto;
mod download;
mod media;
mod sanitize;
mod upload;

pub use media::{GifDecodeResult, GifFullDecodeResult, ImageDecodeResult};
pub use sanitize::{sanitize_media, validate_media, SanitizeOutcome};

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::domain::attachment::{Attachment, ContentLocation};
use crate::domain::ids::AttachmentId;
use crate::proto::content_v1::AttachmentPointer;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::storage::repositories::AttachmentRepository;

// Submodules add impl blocks to AttachmentService
// No re-exports needed - methods are on the struct

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
    pub local_path: std::path::PathBuf,
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

/// Chunk size for file uploads (256KB).
pub(crate) const CHUNK_SIZE: usize = 256 * 1024;

// ============================================================================
// Filesystem Storage Helpers
// ============================================================================

/// Get the attachments directory path.
///
/// Returns `~/.local/share/chatter/attachments/` on Linux.
/// Creates the directory structure if needed when `ensure_attachments_dir()` is called.
pub fn attachments_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("chatter")
        .join("attachments")
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
pub struct AttachmentService {
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    pub(crate) grpc_pool: Option<Arc<GrpcClientPool>>,
}

impl AttachmentService {
    /// Get the attachment repository.
    pub fn attachment_repo(&self) -> Arc<AttachmentRepository> {
        self.attachment_repo.clone()
    }

    /// Get the gRPC pool, panics if not configured (upload/download requires it).
    pub(crate) fn grpc_pool(&self) -> &GrpcClientPool {
        self.grpc_pool.as_ref().expect("AttachmentService: grpc_pool required for network operations")
    }

    /// Create a new AttachmentService with network capability.
    pub fn new(attachment_repo: Arc<AttachmentRepository>, grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self { attachment_repo, grpc_pool: Some(grpc_pool) }
    }

    /// Create a local-only AttachmentService (no upload/download).
    ///
    /// Used by services that only need to store attachment records from incoming messages.
    pub fn new_local(attachment_repo: Arc<AttachmentRepository>) -> Self {
        Self { attachment_repo, grpc_pool: None }
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
    /// Returns the list of attachment IDs that were created or already existed.
    pub fn create_from_pointers(&self, pointers: &[AttachmentPointer]) -> Vec<AttachmentId> {
        use crate::domain::attachment::{Attachment, AttachmentStatus, FileMetadata};

        let mut ids = Vec::with_capacity(pointers.len());

        for pointer in pointers {
            if pointer.cdn_key.is_empty() {
                continue;
            }

            let att_id = AttachmentId::from_cdn_key(&pointer.cdn_key);
            ids.push(att_id);

            // Check if already exists (idempotent)
            if let Ok(Some(_)) = self.attachment_repo.get(&att_id) {
                tracing::debug!(
                    attachment_id = %att_id,
                    cdn_key = %pointer.cdn_key,
                    "Attachment record already exists"
                );
                continue;
            }

            // Create new Attachment record from pointer
            let attachment = Attachment {
                id: att_id,
                metadata: FileMetadata {
                    name: pointer.file_name.clone(),
                    mime_type: pointer.content_type.clone(),
                    size: pointer.plaintext_size,
                    sha256: pointer.digest.clone(),
                    chunk_size: 0,
                    width: if pointer.width > 0 { Some(pointer.width) } else { None },
                    height: if pointer.height > 0 { Some(pointer.height) } else { None },
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
                    thumbnail: if pointer.thumbnail.is_empty() {
                        None
                    } else {
                        Some(pointer.thumbnail.clone())
                    },
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
                },
                status: AttachmentStatus::Pending,
                content_location: ContentLocation::NotDownloaded,
                local_path: None,
                remote_url: Some(pointer.cdn_key.clone()),
                encryption_key: Some(pointer.encryption_key.clone()),
                encryption_iv: Some(pointer.digest.clone()),
                created_at: chrono::Utc::now(),
            };

            // Insert (ignore errors - may be race condition)
            if let Err(e) = self.attachment_repo.insert(&attachment) {
                tracing::debug!(
                    attachment_id = %att_id,
                    cdn_key = %pointer.cdn_key,
                    error = %e,
                    "Attachment insert failed (race condition), continuing"
                );
            } else {
                tracing::info!(
                    attachment_id = %att_id,
                    cdn_key = %pointer.cdn_key,
                    filename = %pointer.file_name,
                    content_type = %pointer.content_type,
                    thumbnail_bytes = pointer.thumbnail.len(),
                    waveform_samples = pointer.waveform.len(),
                    "Created attachment record from pointer"
                );
            }
        }

        ids
    }

    /// Check if attachment content is available locally (in database or filesystem).
    pub fn has_local_content(&self, attachment_id: &AttachmentId) -> bool {
        self.attachment_repo
            .get(attachment_id)
            .ok()
            .flatten()
            .is_some_and(|a| a.content_location == ContentLocation::OnFilesystem)
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
                attachment_id = %attachment_id,
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
                    attachment_id = %attachment_id,
                    width = width,
                    height = height,
                    "Decoded preview image from thumbnail"
                );
                Some((rgba.into_raw(), width, height))
            }
            Err(e) => {
                tracing::warn!(
                    attachment_id = %attachment_id,
                    error = ?e,
                    "Failed to decode thumbnail for preview"
                );
                None
            }
        }
    }

    /// Clean up orphaned attachment files (files with no DB record).
    ///
    /// Scans the attachments directory and removes any `.enc` files that
    /// don't have a corresponding record in the database.
    ///
    /// Returns the number of files cleaned up.
    pub fn cleanup_orphaned_files(&self) -> Result<usize, AttachmentError> {
        let dir = attachments_dir();
        if !dir.exists() {
            return Ok(0);
        }

        let mut cleaned = 0;

        let entries = std::fs::read_dir(&dir)
            .map_err(|e| AttachmentError::FileError(format!("Failed to read attachments dir: {}", e)))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| AttachmentError::FileError(format!("Failed to read dir entry: {}", e)))?;
            let path = entry.path();

            // Only process .enc files
            if path.extension().is_some_and(|ext| ext == "enc") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // Try to parse as AttachmentId
                    if let Ok(id) = stem.parse::<AttachmentId>() {
                        // Check if attachment exists in database
                        match self.attachment_repo.get(&id) {
                            Ok(None) => {
                                // No DB record - orphaned file
                                if let Err(e) = std::fs::remove_file(&path) {
                                    tracing::warn!(
                                        "Failed to remove orphaned attachment file {}: {}",
                                        path.display(),
                                        e
                                    );
                                } else {
                                    tracing::debug!(
                                        "Removed orphaned attachment file: {}",
                                        path.display()
                                    );
                                    cleaned += 1;
                                }
                            }
                            Ok(Some(_)) => {
                                // Attachment exists - keep file
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to check attachment {}: {}",
                                    id,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        if cleaned > 0 {
            tracing::info!("Cleaned up {} orphaned attachment files", cleaned);
        }

        Ok(cleaned)
    }

    /// Delete an attachment and its associated file (if filesystem-stored).
    pub fn delete_attachment(&self, attachment_id: &AttachmentId) -> Result<(), AttachmentError> {
        // Get attachment to check storage location
        if let Ok(Some(attachment)) = self.attachment_repo.get(attachment_id) {
            // If stored on filesystem, delete the file
            if attachment.content_location == ContentLocation::OnFilesystem {
                let file_path = attachment_file_path(attachment_id);
                if file_path.exists() {
                    std::fs::remove_file(&file_path).map_err(|e| {
                        AttachmentError::FileError(format!(
                            "Failed to delete attachment file: {}",
                            e
                        ))
                    })?;
                    tracing::debug!(
                        "Deleted attachment file: {}",
                        file_path.display()
                    );
                }
            }
        }

        // Delete from database
        self.attachment_repo
            .delete(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
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
