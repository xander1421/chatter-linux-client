//! Shared-content row data for Slint UI bindings.
//!
//! Maps a [`SharedContentRow`] off the `shared_content` index to the strings
//! one tile/row of the profile screen's media drawer renders. All the
//! shaping — which line is the title, how a size or a date reads, what a link
//! shows when it has no title — happens here so the UI crate stays a renderer.

use crate::domain::attachment::format_bytes;
use crate::storage::repositories::SharedContentRow;
use crate::ui::chat_data::format_timestamp;

/// One tile (Media/GIFs) or row (Files/Links/Voice/Music) of the shared-content
/// tabs.
///
/// Maps to the `SharedContentItem` struct in `ui/modals/user_profile.slint`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedContentItemData {
    /// Message this item came in — what a tap jumps to.
    pub message_id: String,
    /// Attachment id, empty for a link.
    pub attachment_id: String,
    /// `SharedCategory::as_str` — which tab produced this row.
    pub category: String,
    /// First line. File name, or a link's host.
    pub title: String,
    /// Second line: size and date for a file, the full URL for a link.
    pub subtitle: String,
    /// Decrypted thumbnail JPEG bytes. Empty when there is none — the UI
    /// paints a category glyph on a flat tile instead.
    pub thumbnail: Vec<u8>,
    /// Whether `thumbnail` is worth decoding.
    pub has_thumbnail: bool,
    /// Video tiles get a play badge; the grid can't tell from the thumbnail.
    pub is_video: bool,
}

impl From<&SharedContentRow> for SharedContentItemData {
    fn from(row: &SharedContentRow) -> Self {
        let when = format_timestamp(chrono::DateTime::from_timestamp_nanos(row.sent_at as i64));
        let is_link = row.category == "link";

        // A link's title is its host — the full URL is the second line, where
        // it can wrap. A file leads with its name.
        let (title, subtitle) = if is_link {
            (link_host(&row.title), row.title.clone())
        } else {
            let name = if row.title.is_empty() {
                "Attachment".to_string()
            } else {
                row.title.clone()
            };
            let size = if row.size_bytes > 0 {
                format!("{} · {}", format_bytes(row.size_bytes), when)
            } else {
                when
            };
            (name, size)
        };

        let thumbnail = row.thumbnail.clone().unwrap_or_default();
        Self {
            message_id: row.message_id.clone(),
            attachment_id: row.attachment_id.clone(),
            category: row.category.clone(),
            title,
            subtitle,
            has_thumbnail: !thumbnail.is_empty(),
            thumbnail,
            // Only the file name is available here (the index does not carry
            // MIME), which is the same signal `FileCategory::from_filename`
            // uses for video.
            is_video: matches!(
                crate::domain::attachment::FileCategory::from_filename(&row.title),
                crate::domain::attachment::FileCategory::Video
            ),
        }
    }
}

/// One segment of the shared-content tab strip.
///
/// Maps to the `SharedTab` struct in `ui/modals/user_profile.slint`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedTabData {
    /// `SharedCategory::as_str` — sent back on selection.
    pub id: String,
    /// Human label for the pill.
    pub label: String,
    /// How many items sit behind it.
    pub count: i32,
}

/// Build the tab strip from the index's per-category counts.
///
/// Tabs with nothing behind them are dropped rather than opening onto an empty
/// list — a Voice tab in a conversation that has never had a voice note is a
/// dead end, not information. Order is fixed (not count-sorted) so the strip
/// does not reshuffle as a conversation grows.
#[must_use]
pub fn shared_tabs(counts: &[(String, u32)]) -> Vec<SharedTabData> {
    const ORDER: &[(&str, &str)] = &[
        ("media", "Media"),
        ("file", "Files"),
        ("link", "Links"),
        ("voice", "Voice"),
        ("gif", "GIFs"),
        ("music", "Music"),
    ];
    ORDER
        .iter()
        .filter_map(|(id, label)| {
            let count = counts.iter().find(|(c, _)| c == id).map(|(_, n)| *n)?;
            (count > 0).then(|| SharedTabData {
                id: (*id).to_string(),
                label: (*label).to_string(),
                count: count as i32,
            })
        })
        .collect()
}

/// Host part of a URL, for a link row's headline. Falls back to the whole URL
/// when it does not parse as `scheme://host/...`.
fn link_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(category: &str, title: &str, size: u64) -> SharedContentRow {
        SharedContentRow {
            message_id: "m1".into(),
            attachment_id: "a1".into(),
            category: category.into(),
            sent_at: 1_700_000_000_000_000_000,
            title: title.into(),
            size_bytes: size,
            thumbnail: None,
        }
    }

    #[test]
    fn file_row_leads_with_name_and_size() {
        let d = SharedContentItemData::from(&row("file", "spec.pdf", 2 * 1024 * 1024));
        assert_eq!(d.title, "spec.pdf");
        assert!(d.subtitle.starts_with("2.0 MB · "), "{}", d.subtitle);
        assert!(!d.is_video);
    }

    #[test]
    fn link_row_leads_with_host() {
        let d = SharedContentItemData::from(&row("link", "https://www.rust-lang.org/learn", 0));
        assert_eq!(d.title, "rust-lang.org");
        assert_eq!(d.subtitle, "https://www.rust-lang.org/learn");
    }

    #[test]
    fn video_tiles_are_flagged_for_the_play_badge() {
        assert!(SharedContentItemData::from(&row("media", "clip.mp4", 10)).is_video);
        assert!(!SharedContentItemData::from(&row("media", "cat.jpg", 10)).is_video);
    }

    #[test]
    fn tab_strip_keeps_fixed_order_and_drops_empty_tabs() {
        let tabs = shared_tabs(&[
            ("music".into(), 2),
            ("media".into(), 9),
            ("voice".into(), 0),
            ("nonsense".into(), 5),
        ]);
        assert_eq!(
            tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["media", "music"],
        );
        assert_eq!(tabs[0].label, "Media");
        assert_eq!(tabs[0].count, 9);
    }
}
