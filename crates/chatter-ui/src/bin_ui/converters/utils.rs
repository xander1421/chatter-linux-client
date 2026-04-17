//! Utility functions for domain-to-Slint conversions.

use chatter_core::domain::channel::ChannelType;

use crate::ReactionEntry;

/// Maximum characters in a word before inserting a break point.
/// Slint's word-wrap only breaks at spaces, so we insert zero-width spaces
/// to allow long strings without spaces to wrap.
pub(crate) const MAX_WORD_LENGTH: usize = 50;

/// Insert zero-width spaces (U+200B) into long sequences without whitespace.
/// This allows Slint's word-wrap to break long strings that have no natural break points.
pub(crate) fn insert_word_breaks(text: &str) -> String {
    const ZWSP: char = '\u{200B}';  // Zero-width space

    let mut result = String::with_capacity(text.len() + text.len() / MAX_WORD_LENGTH);
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() {
            word_len = 0;
            result.push(ch);
        } else {
            word_len += 1;
            result.push(ch);
            // Insert zero-width space after MAX_WORD_LENGTH consecutive non-whitespace chars
            if word_len >= MAX_WORD_LENGTH {
                result.push(ZWSP);
                word_len = 0;
            }
        }
    }

    result
}

/// Convert typed reaction counts into Slint `ReactionEntry` pills for
/// rendering. This is the only direction: `ReactionCount` (core, source of
/// truth) → `ReactionEntry` (Slint view). No string round-trips.
pub fn reaction_counts_to_entries(
    counts: &[chatter_core::domain::message::ReactionCount],
) -> Vec<ReactionEntry> {
    counts
        .iter()
        .map(|r| ReactionEntry {
            emoji: r.emoji.clone().into(),
            count: r.count as i32,
        })
        .collect()
}

/// Read a Slint `ReactionEntry` model back into typed `ReactionCount`s so the
/// UI can mutate the aggregate via `ReactionCount::apply` and re-render.
pub fn reactions_model_to_counts(
    model: &slint::ModelRc<ReactionEntry>,
) -> Vec<chatter_core::domain::message::ReactionCount> {
    use slint::Model;
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .map(|entry| chatter_core::domain::message::ReactionCount::new(
            entry.emoji.to_string(),
            entry.count.max(0) as u32,
        ))
        .collect()
}

/// Convert ChannelType enum to string for Slint.
pub fn channel_type_to_string(ct: ChannelType) -> &'static str {
    match ct {
        ChannelType::Public => "public",
        ChannelType::Private => "private",
        ChannelType::DirectMessage => "dm",
        ChannelType::SavedMessages => "saved",
    }
}

/// Extract initials from a display name (first letter of each word, max 2).
pub fn get_initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// Convert avatar bytes to a Slint Image.
///
/// Returns None if the bytes are empty or can't be decoded.
pub fn avatar_bytes_to_image(bytes: &[u8]) -> Option<slint::Image> {
    if bytes.is_empty() {
        return None;
    }
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                rgba.as_raw(),
                w,
                h,
            );
            Some(slint::Image::from_rgba8(buffer))
        }
        Err(e) => {
            tracing::warn!("Failed to decode avatar image: {:?}", e);
            None
        }
    }
}

/// Format file size in human-readable form.
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Map MIME type to category for UI display.
pub fn mime_to_category(mime_type: &str) -> &'static str {
    if mime_type.starts_with("image/") {
        "image"
    } else if mime_type.starts_with("video/") {
        "video"
    } else if mime_type.starts_with("audio/") {
        "audio"
    } else if mime_type.starts_with("text/") || mime_type == "application/json" {
        "code"
    } else if mime_type == "application/pdf"
        || mime_type.starts_with("application/msword")
        || mime_type.contains("document")
    {
        "document"
    } else if mime_type == "application/zip"
        || mime_type == "application/x-rar-compressed"
        || mime_type == "application/x-tar"
        || mime_type == "application/gzip"
    {
        "archive"
    } else {
        "other"
    }
}

/// Map category to icon emoji and background color.
pub(crate) fn category_to_icon_and_color(category: &str) -> (&'static str, slint::Color) {
    match category {
        "document" => ("\u{1F4C4}", slint::Color::from_argb_u8(255, 96, 165, 250)),  // #60a5fa
        "code" => ("\u{1F4DD}", slint::Color::from_argb_u8(255, 251, 191, 36)),       // #fbbf24
        "archive" => ("\u{1F4E6}", slint::Color::from_argb_u8(255, 251, 146, 60)),    // #fb923c
        "image" => ("\u{1F5BC}", slint::Color::from_argb_u8(255, 74, 222, 128)),      // #4ade80
        "video" => ("\u{1F3AC}", slint::Color::from_argb_u8(255, 248, 113, 113)),     // #f87171
        "audio" => ("\u{1F3B5}", slint::Color::from_argb_u8(255, 167, 139, 250)),     // #a78bfa
        _ => ("\u{1F4CE}", slint::Color::from_argb_u8(255, 148, 163, 184)),           // #94a3b8
    }
}

/// Format a count with singular/plural form.
pub(crate) fn format_count(count: i32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

pub(crate) use chatter_core::services::message::conversion::extract_quote_from_content_json;
