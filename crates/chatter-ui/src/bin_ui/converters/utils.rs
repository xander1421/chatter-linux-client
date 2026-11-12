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

/// Background colour of an attachment category's icon.
pub(crate) fn category_color(category: &str) -> slint::Color {
    match category {
        "document" => slint::Color::from_argb_u8(255, 96, 165, 250), // #60a5fa
        "code" => slint::Color::from_argb_u8(255, 251, 191, 36),     // #fbbf24
        "archive" => slint::Color::from_argb_u8(255, 251, 146, 60),  // #fb923c
        "image" => slint::Color::from_argb_u8(255, 74, 222, 128),    // #4ade80
        "video" => slint::Color::from_argb_u8(255, 248, 113, 113),   // #f87171
        "audio" => slint::Color::from_argb_u8(255, 167, 139, 250),   // #a78bfa
        _ => slint::Color::from_argb_u8(255, 148, 163, 184),         // #94a3b8
    }
}
