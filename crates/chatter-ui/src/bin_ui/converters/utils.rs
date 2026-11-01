//! Utility functions for domain-to-Slint conversions.

use crate::ReactionEntry;

/// Convert typed reaction counts into Slint `ReactionEntry` pills for
/// rendering. This is the only direction: `ReactionCount` (core, source of
/// truth) → `ReactionEntry` (Slint view). No string round-trips.
pub fn reaction_counts_to_entries(
    counts: &[chatter_core::domain::message::ReactionCount],
    own_emojis: &[String],
) -> Vec<ReactionEntry> {
    chatter_core::domain::reaction::reaction_presentation(counts, own_emojis)
        .into_iter()
        .map(|r| ReactionEntry {
            emoji: r.emoji.clone().into(),
            count: r.count as i32,
            own: r.own,
        })
        .collect()
}

/// Convert avatar bytes to a Slint Image.
///
/// Returns None if the bytes are empty or can't be decoded.
pub fn avatar_bytes_to_image(bytes: &[u8]) -> Option<slint::Image> {
    crate::bin_ui::decode_cache::avatar(bytes).map(slint::Image::from_rgba8)
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
        "document" => ("\u{1F4C4}", slint::Color::from_argb_u8(255, 96, 165, 250)), // #60a5fa
        "code" => ("\u{1F4DD}", slint::Color::from_argb_u8(255, 251, 191, 36)),     // #fbbf24
        "archive" => ("\u{1F4E6}", slint::Color::from_argb_u8(255, 251, 146, 60)),  // #fb923c
        "image" => ("\u{1F4F7}", slint::Color::from_argb_u8(255, 74, 222, 128)),    // #4ade80
        "video" => ("\u{1F3AC}", slint::Color::from_argb_u8(255, 248, 113, 113)),   // #f87171
        "audio" => ("\u{1F3B5}", slint::Color::from_argb_u8(255, 167, 139, 250)),   // #a78bfa
        _ => ("\u{1F4CE}", slint::Color::from_argb_u8(255, 148, 163, 184)),         // #94a3b8
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
