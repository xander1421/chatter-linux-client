//! Raw shared-content row data from the local index.

use crate::storage::repositories::SharedContentRow;

/// One tile (Photos/Videos/GIFs) or row (Files/Links/Voice/Audio) of the shared-content
/// tabs.
///
/// The client presenter maps this to a `SharedContentItem`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedContentItemData {
    /// Message this item came in — what a tap jumps to.
    pub message_id: String,
    /// Attachment id, empty for a link.
    pub attachment_id: String,
    /// `SharedCategory::as_str` — which tab produced this row.
    pub category: String,
    /// Raw index values used to build the visible title and subtitle.
    pub source_title: String,
    pub size_bytes: u64,
    pub sent_at: u64,
    /// Decrypted thumbnail JPEG bytes. Empty when there is none — the UI
    /// paints a category glyph on a flat tile instead.
    pub thumbnail: Vec<u8>,
    /// Classified from stored attachment metadata by the core facade.
    pub is_video: bool,
}

impl From<&SharedContentRow> for SharedContentItemData {
    fn from(row: &SharedContentRow) -> Self {
        let thumbnail = row.thumbnail.clone().unwrap_or_default();
        Self {
            message_id: row.message_id.clone(),
            attachment_id: row.attachment_id.clone(),
            category: row.category.clone(),
            source_title: row.title.clone(),
            size_bytes: row.size_bytes,
            sent_at: row.sent_at,
            thumbnail,
            is_video: false,
        }
    }
}

/// One segment of the shared-content tab strip.
///
/// The label is ready to render, including the count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedTabData {
    /// `SharedCategory::as_str` — sent back on selection.
    pub id: String,
    /// How many items sit behind it.
    pub count: i32,
    /// Ready to render, including count and singular/plural noun.
    pub label: String,
}

/// Build the tab strip from the index's per-category counts.
///
/// Tabs with nothing behind them are dropped rather than opening onto an empty
/// list — a Voice tab in a conversation that has never had a voice note is a
/// dead end, not information. Order is fixed (not count-sorted) so the strip
/// does not reshuffle as a conversation grows.
#[must_use]
pub fn shared_tabs(counts: &[(String, u32)]) -> Vec<SharedTabData> {
    const ORDER: &[&str] = &["photo", "video", "audio", "link", "voice", "gif", "file"];
    ORDER
        .iter()
        .filter_map(|id| {
            let count = counts.iter().find(|(c, _)| c == id).map(|(_, n)| *n)?;
            if count == 0 { return None; }
            let noun = match (*id, count) {
                ("photo", 1) => "photo", ("photo", _) => "photos",
                ("video", 1) => "video", ("video", _) => "videos",
                ("audio", 1) => "audio file", ("audio", _) => "audio files",
                ("link", 1) => "shared link", ("link", _) => "shared links",
                ("voice", 1) => "voice message", ("voice", _) => "voice messages",
                ("gif", 1) => "GIF", ("gif", _) => "GIFs",
                ("file", 1) => "file", _ => "files",
            };
            Some(SharedTabData {
                id: (*id).to_string(),
                count: count as i32,
                label: format!("{count} {noun}"),
            })
        })
        .collect()
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
    fn item_retains_raw_index_values() {
        let item = SharedContentItemData::from(&row("file", "spec.pdf", 2 * 1024 * 1024));
        assert_eq!(item.source_title, "spec.pdf");
        assert_eq!(item.size_bytes, 2 * 1024 * 1024);
        assert_eq!(item.sent_at, 1_700_000_000_000_000_000);
    }

    #[test]
    fn tab_strip_keeps_fixed_order_and_drops_empty_tabs() {
        let tabs = shared_tabs(&[
            ("audio".into(), 2),
            ("photo".into(), 9),
            ("voice".into(), 0),
            ("nonsense".into(), 5),
        ]);
        assert_eq!(
            tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["photo", "audio"],
        );
        assert_eq!(tabs[0].count, 9);
        assert_eq!(tabs[0].label, "9 photos");
        assert_eq!(tabs[1].label, "2 audio files");
    }
}
