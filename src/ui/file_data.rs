//! File attachment data structures for Slint UI bindings.
//!
//! Provides Rust structs that map to Slint struct definitions for
//! file attachment display components.

use crate::domain::attachment::{Attachment, AttachmentStatus, FileCategory};

// ============================================================================
// Attachment Item Data
// ============================================================================

/// Attachment item data for Slint model binding.
///
/// Maps to the properties of `AttachmentItem` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct AttachmentItemData {
    /// Attachment ID for identification.
    pub attachment_id: String,
    /// File name to display.
    pub file_name: String,
    /// Formatted file size (e.g., "2.5 MB").
    pub file_size: String,
    /// MIME type of the file.
    pub mime_type: String,
    /// File category: "image", "video", "audio", "document", "code", "archive", "other".
    pub category: String,
    /// URL for thumbnail preview (empty if no preview available).
    pub thumbnail_url: String,
    /// Whether the file is ready (downloaded/available).
    pub is_ready: bool,
    /// Whether the file is currently uploading.
    pub is_uploading: bool,
    /// Upload progress (0.0-1.0).
    pub upload_progress: f32,
    /// Whether the file is currently downloading.
    pub is_downloading: bool,
    /// Download progress (0.0-1.0).
    pub download_progress: f32,
}

impl AttachmentItemData {
    /// Create a new attachment item data entry.
    #[must_use]
    pub fn new(attachment_id: impl Into<String>) -> Self {
        Self {
            attachment_id: attachment_id.into(),
            ..Default::default()
        }
    }

    /// Set file name.
    #[must_use]
    pub fn with_file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = name.into();
        self
    }

    /// Set formatted file size.
    #[must_use]
    pub fn with_file_size(mut self, size: impl Into<String>) -> Self {
        self.file_size = size.into();
        self
    }

    /// Set MIME type.
    #[must_use]
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = mime_type.into();
        self
    }

    /// Set file category.
    #[must_use]
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    /// Set thumbnail URL.
    #[must_use]
    pub fn with_thumbnail(mut self, url: impl Into<String>) -> Self {
        self.thumbnail_url = url.into();
        self
    }

    /// Set ready state.
    #[must_use]
    pub fn with_ready(mut self, is_ready: bool) -> Self {
        self.is_ready = is_ready;
        self
    }

    /// Set uploading state with progress.
    #[must_use]
    pub fn with_uploading(mut self, is_uploading: bool, progress: f32) -> Self {
        self.is_uploading = is_uploading;
        self.upload_progress = progress.clamp(0.0, 1.0);
        self
    }

    /// Set downloading state with progress.
    #[must_use]
    pub fn with_downloading(mut self, is_downloading: bool, progress: f32) -> Self {
        self.is_downloading = is_downloading;
        self.download_progress = progress.clamp(0.0, 1.0);
        self
    }
}

impl From<&Attachment> for AttachmentItemData {
    fn from(attachment: &Attachment) -> Self {
        let category = match attachment.metadata.category() {
            FileCategory::Image => "image",
            FileCategory::Video => "video",
            FileCategory::Audio => "audio",
            FileCategory::Document => "document",
            FileCategory::Code => "code",
            FileCategory::Archive => "archive",
            FileCategory::Other => "other",
        };

        let is_ready = matches!(attachment.status, AttachmentStatus::Ready);
        let is_uploading = matches!(attachment.status, AttachmentStatus::Uploading);

        Self {
            attachment_id: attachment.id.to_string(),
            file_name: attachment.metadata.name.clone(),
            file_size: attachment.metadata.formatted_size(),
            mime_type: attachment.metadata.mime_type.clone(),
            category: category.to_string(),
            thumbnail_url: String::new(), // Must be set externally
            is_ready,
            is_uploading,
            upload_progress: 0.0,
            is_downloading: false,
            download_progress: 0.0,
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get an icon emoji for a file category.
///
/// Returns an appropriate emoji icon based on the file category string.
#[must_use]
pub fn category_icon(category: &str) -> &'static str {
    match category {
        "image" => "\u{1F5BC}\u{FE0F}", // Framed picture
        "video" => "\u{1F3AC}",          // Clapper board
        "audio" => "\u{1F3B5}",          // Musical note
        "document" => "\u{1F4C4}",       // Page facing up
        "code" => "\u{1F4DD}",           // Memo
        "archive" => "\u{1F4E6}",        // Package
        _ => "\u{1F4CE}",                // Paperclip (default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{Attachment, FileMetadata};
    use crate::domain::ids::AttachmentId;

    #[test]
    fn test_attachment_item_data_new() {
        let item = AttachmentItemData::new("attach-123")
            .with_file_name("document.pdf")
            .with_file_size("2.5 MB")
            .with_mime_type("application/pdf")
            .with_category("document")
            .with_ready(true);

        assert_eq!(item.attachment_id, "attach-123");
        assert_eq!(item.file_name, "document.pdf");
        assert_eq!(item.file_size, "2.5 MB");
        assert_eq!(item.mime_type, "application/pdf");
        assert_eq!(item.category, "document");
        assert!(item.is_ready);
        assert!(!item.is_uploading);
        assert!(!item.is_downloading);
    }

    #[test]
    fn test_attachment_item_with_uploading() {
        let item = AttachmentItemData::new("attach-456")
            .with_uploading(true, 0.75);

        assert!(item.is_uploading);
        assert!((item.upload_progress - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_attachment_item_with_downloading() {
        let item = AttachmentItemData::new("attach-789")
            .with_downloading(true, 0.5);

        assert!(item.is_downloading);
        assert!((item.download_progress - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_attachment_item_progress_clamped() {
        let item = AttachmentItemData::new("attach-abc")
            .with_uploading(true, 1.5)
            .with_downloading(true, -0.5);

        assert!((item.upload_progress - 1.0).abs() < f32::EPSILON);
        assert!((item.download_progress - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_attachment_item_from_attachment() {
        let metadata = FileMetadata::new(
            "photo.jpg".to_string(),
            "image/jpeg".to_string(),
            1024 * 1024 * 2, // 2 MB
        );
        let mut attachment = Attachment::new(AttachmentId::new(), metadata);
        attachment.status = AttachmentStatus::Ready;

        let item = AttachmentItemData::from(&attachment);

        assert_eq!(item.file_name, "photo.jpg");
        assert_eq!(item.mime_type, "image/jpeg");
        assert_eq!(item.category, "image");
        assert!(item.is_ready);
        assert!(!item.is_uploading);
        assert!(item.thumbnail_url.is_empty());
    }

    #[test]
    fn test_attachment_item_from_uploading_attachment() {
        let metadata = FileMetadata::new(
            "video.mp4".to_string(),
            "video/mp4".to_string(),
            1024 * 1024 * 50, // 50 MB
        );
        let mut attachment = Attachment::new(AttachmentId::new(), metadata);
        attachment.status = AttachmentStatus::Uploading;

        let item = AttachmentItemData::from(&attachment);

        assert_eq!(item.category, "video");
        assert!(!item.is_ready);
        assert!(item.is_uploading);
    }

    #[test]
    fn test_category_icon() {
        assert_eq!(category_icon("image"), "\u{1F5BC}\u{FE0F}");
        assert_eq!(category_icon("video"), "\u{1F3AC}");
        assert_eq!(category_icon("audio"), "\u{1F3B5}");
        assert_eq!(category_icon("document"), "\u{1F4C4}");
        assert_eq!(category_icon("code"), "\u{1F4DD}");
        assert_eq!(category_icon("archive"), "\u{1F4E6}");
        assert_eq!(category_icon("unknown"), "\u{1F4CE}");
        assert_eq!(category_icon(""), "\u{1F4CE}");
    }
}
