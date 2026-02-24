//! Attachment service module.
//!
//! Upload, download, share, and encrypt attachments.

mod crypto;
mod download;
mod upload;

use std::sync::Arc;

use thiserror::Error;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::domain::attachment::ContentLocation;
use crate::domain::ids::AttachmentId;
use crate::proto::content_v1::AttachmentPointer;
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

/// Attachment service for file operations.
pub struct AttachmentService {
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
}

impl AttachmentService {
    /// Create a new AttachmentService.
    pub fn new(attachment_repo: Arc<AttachmentRepository>) -> Self {
        Self { attachment_repo }
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
    ) -> Result<Option<crate::domain::attachment::Attachment>, AttachmentError> {
        self.attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))
    }
}
