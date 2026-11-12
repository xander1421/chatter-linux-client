//! Attachment display data: category icons and the shaped fields an
//! attachment row shows.

use std::sync::Arc;

use crate::domain::attachment::{Attachment, AttachmentStatus, FileCategory};
use crate::ui::format::{format_file_size, mime_to_category, waveform_bar};

/// Get an icon emoji for a file category.
///
/// Returns an appropriate emoji icon based on the file category string.
#[must_use]
pub fn category_icon(category: &str) -> &'static str {
    match category {
        "image" => "\u{1F4F7}",        // Camera: colour-default, 🖼️ was tofu on Android 10
        "video" => "\u{1F3AC}",         // Clapper board
        "audio" => "\u{1F3B5}",         // Musical note
        "document" => "\u{1F4C4}",      // Page facing up
        "code" => "\u{1F4DD}",          // Memo
        "archive" => "\u{1F4E6}",       // Package
        _ => "\u{1F4CE}",               // Paperclip (default)
    }
}

/// Everything an attachment row shows that does not need Slint: labels,
/// category, transfer flags and the normalised waveform. The UI adds pixels,
/// the icon colour and the live playing flag.
#[derive(Clone, Debug)]
pub struct AttachmentView {
    pub id: String,
    pub filename: String,
    pub size_display: String,
    pub mime_type: String,
    pub category: &'static str,
    pub download_url: String,
    pub is_image: bool,
    pub is_video: bool,
    pub is_audio: bool,
    pub width: i32,
    pub height: i32,
    pub icon: &'static str,
    /// Audio waveform samples (32 values, 0-100) for visualization.
    pub waveform: Vec<i32>,
    /// Formatted duration string (e.g., "1:23").
    pub duration_display: String,
    pub is_uploading: bool,
    pub is_downloading: bool,
    pub is_download_failed: bool,
    /// Thumbnail JPEG bytes from sender (for instant preview without download).
    pub thumbnail_bytes: Arc<[u8]>,
}

/// Shape a stored attachment for display.
#[must_use]
pub fn attachment_view(attachment: &Attachment) -> AttachmentView {
    let meta = &attachment.metadata;
    let category = mime_to_category(&meta.mime_type);

    // Normalize wire/storage bytes (0-255) to the Slint bar scale (0-100).
    let waveform: Vec<i32> = meta
        .waveform_samples
        .as_ref()
        .filter(|samples| !samples.is_empty())
        .map(|samples| {
            let converted: Vec<i32> = samples
                .iter()
                .map(|&s| waveform_bar(i32::from(s)))
                .collect();
            if meta.is_audio() {
                let max = converted.iter().max().copied().unwrap_or(0);
                tracing::debug!(
                    "attachment_to_display_data: waveform has {} samples, max={}, first_5={:?}",
                    converted.len(),
                    max,
                    &converted[..5.min(converted.len())]
                );
            }
            converted
        })
        .unwrap_or_else(|| {
            if meta.is_audio() {
                tracing::warn!(
                    "attachment_to_display_data: no waveform_samples for audio, using flat default"
                );
            }
            vec![10; 32]
        }); // Default flat waveform

    // Format duration for audio
    let duration_display = meta
        .duration_seconds
        .map(|secs| {
            let total_secs = secs as u64;
            let minutes = total_secs / 60;
            let seconds = total_secs % 60;
            format!("{}:{:02}", minutes, seconds)
        })
        .unwrap_or_default();

    AttachmentView {
        id: attachment.id.to_string(),
        filename: meta.bubble_filename().to_string(),
        size_display: format_file_size(meta.size),
        mime_type: meta.mime_type.clone(),
        category,
        download_url: attachment.remote_url.clone().unwrap_or_default(),
        is_image: meta.is_image(),
        is_video: FileCategory::from_mime_and_filename(&meta.mime_type, &meta.name)
            == FileCategory::Video,
        is_audio: meta.is_audio(),
        width: meta.width.unwrap_or(0) as i32,
        height: meta.height.unwrap_or(0) as i32,
        icon: category_icon(category),
        waveform,
        duration_display,
        // Determine transfer status from attachment status
        is_uploading: attachment.status == AttachmentStatus::Uploading,
        is_downloading: attachment.status == AttachmentStatus::Downloading,
        is_download_failed: attachment.status == AttachmentStatus::Failed
            && attachment.remote_url.is_some(),
        // Use stored thumbnail from sender (for instant preview)
        thumbnail_bytes: {
            let bytes: Arc<[u8]> = meta.thumbnail.as_deref().unwrap_or_default().into();
            if meta.is_image() || meta.is_video() {
                tracing::debug!(
                    "attachment_to_display_data: {} thumbnail_bytes={}, waveform_samples={:?}",
                    meta.name,
                    bytes.len(),
                    meta.waveform_samples.as_ref().map(|w| w.len())
                );
            }
            bytes
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category_icon() {
        assert_eq!(category_icon("image"), "\u{1F4F7}");
        assert_eq!(category_icon("video"), "\u{1F3AC}");
        assert_eq!(category_icon("audio"), "\u{1F3B5}");
        assert_eq!(category_icon("document"), "\u{1F4C4}");
        assert_eq!(category_icon("code"), "\u{1F4DD}");
        assert_eq!(category_icon("archive"), "\u{1F4E6}");
        assert_eq!(category_icon("unknown"), "\u{1F4CE}");
        assert_eq!(category_icon(""), "\u{1F4CE}");
    }
}
