//! Attachment-only summaries for the forwarding picker. No media reads or transport.

use crate::domain::attachment::{FileCategory, FileMetadata, SharedCategory};

/// Captioned messages retain the existing truncation and skip attachment lookup.
pub fn forward_message_preview(
    text: Option<&str>,
    truncate: impl FnOnce(&str) -> String,
    attachments: impl FnOnce() -> Vec<Option<FileMetadata>>,
) -> String {
    match text.filter(|text| !text.trim().is_empty()) {
        Some(text) => truncate(text),
        None => attachment_preview(&attachments()),
    }
}

/// Missing metadata still represents an attachment and contributes to the count.
pub fn attachment_preview(metadata: &[Option<FileMetadata>]) -> String {
    let mut counts = [0usize; 6];
    for meta in metadata {
        let kind = match meta {
            Some(meta) => match SharedCategory::of(meta) {
                SharedCategory::Voice => 0,
                SharedCategory::Gif => 3,
                SharedCategory::Audio => 4,
                _ => match FileCategory::from_mime_and_filename(&meta.mime_type, &meta.name) {
                    FileCategory::Image => 1,
                    FileCategory::Video => 2,
                    _ => 5,
                },
            },
            None => 5,
        };
        counts[kind] += 1;
    }
    let labels = [
        ("Voice message", "voice messages"),
        ("Photo", "photos"),
        ("Video", "videos"),
        ("GIF", "GIFs"),
        ("Audio file", "audio files"),
        ("File", "files"),
    ];
    let parts: Vec<_> = counts
        .iter()
        .zip(labels)
        .filter_map(|(&count, (one, many))| match count {
            0 => None,
            1 => Some(one.to_string()),
            _ => Some(format!("{count} {many}")),
        })
        .collect();
    if parts.is_empty() {
        "Message".to_string()
    } else {
        parts.join(", ")
    }
}

/// The UI supplies only attachment display fields; core owns the summary.
pub struct ActionPreviewAttachment {
    pub is_image: bool,
    pub category: String,
    pub filename: String,
}

pub fn message_action_preview(text: &str, attachments: &[ActionPreviewAttachment]) -> String {
    let text_preview = crate::domain::message::truncate_preview_text(text, 200);
    let image_count = attachments.iter().filter(|a| a.is_image).count();
    let files: Vec<_> = attachments.iter().filter(|a| !a.is_image).collect();
    let mut parts = Vec::new();
    if image_count == 1 { parts.push("\u{1F4F7} Photo".to_string()); }
    else if image_count > 1 { parts.push(format!("\u{1F4F7} {image_count} Photos")); }
    if files.len() == 1 {
        let file = files[0];
        let emoji = match file.category.as_str() {
            "audio" => "\u{1F3B5}", "video" => "\u{1F3AC}",
            "document" => "\u{1F4C4}", "code" => "\u{1F4DD}",
            "archive" => "\u{1F4E6}", _ => "\u{1F4CE}",
        };
        parts.push(format!("{emoji} {}", file.filename));
    } else if files.len() > 1 { parts.push(format!("\u{1F4CE} {} files", files.len())); }
    let attachment_label = parts.join(", ");
    match (text_preview.is_empty(), attachment_label.is_empty()) {
        (false, false) => format!(
            "{} \u{2022} {attachment_label}",
            crate::domain::message::format_preview_body("", &text_preview, None)
        ),
        (true, false) => attachment_label,
        (false, true) => crate::domain::message::format_preview_body("", &text_preview, None),
        (true, true) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(name: &str, mime: &str) -> Option<FileMetadata> {
        Some(FileMetadata::new(name.into(), mime.into(), 123))
    }

    #[test]
    fn text_keeps_original_content_and_truncation_without_attachment_lookup() {
        let text = "  caption with photo  ";
        assert_eq!(
            forward_message_preview(
                Some(text),
                |body| {
                    assert_eq!(body, text);
                    "existing truncated preview".into()
                },
                || panic!("text preview must not resolve attachments")
            ),
            "existing truncated preview"
        );
        for empty in [None, Some(""), Some(" \n\t")] {
            assert_eq!(
                forward_message_preview(
                    empty,
                    |_| panic!("empty body"),
                    || vec![metadata("photo.jpg", "image/jpeg")]
                ),
                "Photo"
            );
        }
    }

    #[test]
    fn recorded_voice_never_exposes_generated_filename() {
        let mut voice = metadata("voice_1729000000.ogg", "audio/ogg").unwrap();
        voice.waveform_samples = Some(vec![1, 2, 3]);
        assert_eq!(attachment_preview(&[Some(voice.clone())]), "Voice message");
        assert_eq!(
            attachment_preview(&[Some(voice.clone()), Some(voice)]),
            "2 voice messages"
        );
    }

    #[test]
    fn attachment_types_and_mixed_counts_are_readable() {
        assert_eq!(
            attachment_preview(&[metadata("IMG_001.jpg", "image/jpeg")]),
            "Photo"
        );
        assert_eq!(
            attachment_preview(&[metadata("clip.mp4", "video/mp4")]),
            "Video"
        );
        assert_eq!(
            attachment_preview(&[metadata("report.pdf", "application/pdf")]),
            "File"
        );
        assert_eq!(
            attachment_preview(&[metadata("cat.gif", "image/gif")]),
            "GIF"
        );
        assert_eq!(
            attachment_preview(&[metadata("song.ogg", "audio/ogg")]),
            "Audio file"
        );
        assert_eq!(
            attachment_preview(&[
                metadata("a.jpg", "image/jpeg"),
                metadata("b.jpg", "image/jpeg"),
                metadata("v.mp4", "video/mp4"),
                metadata("a.pdf", "application/pdf"),
                None,
            ]),
            "2 photos, Video, 2 files"
        );
    }

    #[test]
    fn unresolved_and_absent_metadata_never_leave_blank_preview() {
        assert_eq!(attachment_preview(&[None]), "File");
        assert_eq!(attachment_preview(&[None, None]), "2 files");
        assert_eq!(attachment_preview(&[]), "Message");
    }
}
