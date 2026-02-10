//! Attachment domain entity.
//!
//! Represents file attachments and metadata.

use crate::domain::ids::AttachmentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Attachment status in upload/download lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AttachmentStatus {
    #[default]
    Pending,
    Uploading,
    Processing,
    Ready,
    Failed,
    Expired,
}

/// File category for UI display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileCategory {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Code,
    Other,
}

impl FileCategory {
    /// Infer category from MIME type.
    #[must_use]
    pub fn from_mime_type(mime: &str) -> Self {
        if mime.starts_with("image/") {
            Self::Image
        } else if mime.starts_with("video/") {
            Self::Video
        } else if mime.starts_with("audio/") {
            Self::Audio
        } else if mime.starts_with("text/") || mime.contains("javascript") || mime.contains("json")
        {
            Self::Code
        } else if mime == "application/pdf"
            || mime.contains("document")
            || mime.contains("sheet")
            || mime.contains("presentation")
        {
            Self::Document
        } else if mime.contains("zip") || mime.contains("tar") || mime.contains("compressed") {
            Self::Archive
        } else {
            Self::Other
        }
    }
}

/// Represents a file attachment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub metadata: FileMetadata,
    pub status: AttachmentStatus,
    /// Local file path (after download)
    pub local_path: Option<String>,
    /// Remote URL (signed URL for download)
    pub remote_url: Option<String>,
    /// Encryption key for encrypted attachments
    pub encryption_key: Option<Vec<u8>>,
    /// Encryption IV/nonce
    pub encryption_iv: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
}

impl Attachment {
    /// Create a new pending attachment.
    #[must_use]
    pub fn new(id: AttachmentId, metadata: FileMetadata) -> Self {
        Self {
            id,
            metadata,
            status: AttachmentStatus::Pending,
            local_path: None,
            remote_url: None,
            encryption_key: None,
            encryption_iv: None,
            created_at: Utc::now(),
        }
    }

    /// Check if attachment is ready for use.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.status, AttachmentStatus::Ready)
    }

    /// Check if attachment is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.encryption_key.is_some()
    }
}

/// File metadata information (matches proto FileMetadata).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    /// SHA-256 hash of original (unencrypted) file
    pub sha256: Vec<u8>,
    /// Chunk size for resumable uploads (0 = not chunked)
    pub chunk_size: u32,
    /// Image/video dimensions
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Audio/video duration in seconds
    pub duration_seconds: Option<f64>,
}

impl FileMetadata {
    /// Create metadata for a local file.
    #[must_use]
    pub fn new(name: String, mime_type: String, size: u64) -> Self {
        Self {
            name,
            mime_type,
            size,
            sha256: Vec::new(),
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: None,
        }
    }

    /// Get file category based on MIME type.
    #[must_use]
    pub fn category(&self) -> FileCategory {
        FileCategory::from_mime_type(&self.mime_type)
    }

    /// Check if file is an image.
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(self.category(), FileCategory::Image)
    }

    /// Check if file is a video.
    #[must_use]
    pub fn is_video(&self) -> bool {
        matches!(self.category(), FileCategory::Video)
    }

    /// Check if file is audio.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        matches!(self.category(), FileCategory::Audio)
    }

    /// Check if file is code/text (syntax-highlightable).
    #[must_use]
    pub fn is_code(&self) -> bool {
        matches!(self.category(), FileCategory::Code)
    }

    /// Check if file is a PDF document.
    #[must_use]
    pub fn is_pdf(&self) -> bool {
        self.mime_type == "application/pdf"
    }

    /// Check if file is a document (PDF, Word, etc).
    #[must_use]
    pub fn is_document(&self) -> bool {
        matches!(self.category(), FileCategory::Document)
    }

    /// Human-readable file size.
    #[must_use]
    pub fn formatted_size(&self) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        if self.size >= GB {
            format!("{:.1} GB", self.size as f64 / GB as f64)
        } else if self.size >= MB {
            format!("{:.1} MB", self.size as f64 / MB as f64)
        } else if self.size >= KB {
            format!("{:.1} KB", self.size as f64 / KB as f64)
        } else {
            format!("{} bytes", self.size)
        }
    }
}

/// GIF attachment metadata (from Tenor/Giphy).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GifMetadata {
    pub id: String,
    pub provider: GifProvider,
    pub title: Option<String>,
    /// URL to the GIF file
    pub url: String,
    /// URL to smaller preview
    pub preview_url: String,
    pub width: u32,
    pub height: u32,
}

/// GIF provider source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GifProvider {
    Tenor,
    Giphy,
}

/// Thumbnail cache entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
    pub attachment_id: AttachmentId,
    pub local_path: String,
    pub width: u32,
    pub height: u32,
    pub created_at: DateTime<Utc>,
}
