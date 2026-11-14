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
    let parts: Vec<_> = [
        counted_kind(counts[0], "Voice message", "{} voice messages"),
        counted_kind(counts[1], "Photo", "{} photos"),
        counted_kind(counts[2], "Video", "{} videos"),
        counted_kind(counts[3], "GIF", "{} GIFs"),
        counted_kind(counts[4], "Audio file", "{} audio files"),
        counted_kind(counts[5], "File", "{} files"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        chatter_i18n::tr("Message")
    } else {
        parts.join(", ")
    }
}

/// Singular stays the bare word ("Photo"); two or more keep the count.
fn counted_kind(count: usize, one: &'static str, many: &'static str) -> Option<String> {
    match count {
        0 => None,
        1 => Some(match one {
            "Voice message" => chatter_i18n::tr("Voice message"),
            "Photo" => chatter_i18n::tr("Photo"),
            "Video" => chatter_i18n::tr("Video"),
            "GIF" => chatter_i18n::tr("GIF"),
            "Audio file" => chatter_i18n::tr("Audio file"),
            "File" => chatter_i18n::tr("File"),
            _ => one.to_string(),
        }),
        n => Some(match many {
            "{} voice messages" => chatter_i18n::trf("{} voice messages", &[&n]),
            "{} photos" => chatter_i18n::trf("{} photos", &[&n]),
            "{} videos" => chatter_i18n::trf("{} videos", &[&n]),
            "{} GIFs" => chatter_i18n::trf("{} GIFs", &[&n]),
            "{} audio files" => chatter_i18n::trf("{} audio files", &[&n]),
            "{} files" => chatter_i18n::trf("{} files", &[&n]),
            _ => format!("{n} {many}"),
        }),
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
    if image_count == 1 {
        parts.push(chatter_i18n::tr("\u{1F4F7} Photo"));
    } else if image_count > 1 {
        parts.push(chatter_i18n::trf("\u{1F4F7} {} Photos", &[&image_count]));
    }
    if files.len() == 1 {
        let file = files[0];
        let emoji = match file.category.as_str() {
            "audio" => "\u{1F3B5}",
            "video" => "\u{1F3AC}",
            "document" => "\u{1F4C4}",
            "code" => "\u{1F4DD}",
            "archive" => "\u{1F4E6}",
            _ => "\u{1F4CE}",
        };
        // Filename is user content; the glyph is the category marker.
        parts.push(format!("{emoji} {}", file.filename));
    } else if files.len() > 1 {
        parts.push(chatter_i18n::trf("\u{1F4CE} {} files", &[&files.len()]));
    }
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
