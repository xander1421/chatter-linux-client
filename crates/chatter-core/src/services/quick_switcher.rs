//! Quick switcher ordering rules.
//!
//! The Ctrl+K command palette aggregates channels, DMs, anon DMs, and private
//! groups into one list. This module owns the canonical ordering and cap
//! rules so the UI layer stays a thin presenter.
//!
//! Ordering rule: unread-count descending, then case-insensitive alphabetical
//! by display name. The list is capped at [`QUICK_SWITCHER_MAX_ENTRIES`] so
//! the modal stays scannable.

/// Maximum entries shown in the quick switcher.
pub const QUICK_SWITCHER_MAX_ENTRIES: usize = 20;

/// One conversation row eligible for the quick switcher list.
///
/// Holds the minimum data needed to apply the ordering rule and to render an
/// entry. Concrete UI types (e.g. Slint structs) are built from this on the
/// presenter side.
#[derive(Debug, Clone)]
pub struct QuickSwitcherEntry {
    /// Stable conversation id (channel id, DM channel id, group id, etc.).
    pub id: String,
    /// Display name used for alphabetical tie-breaking.
    pub name: String,
    /// Conversation kind tag — one of "channel", "dm", "anon_dm", "private_group".
    pub conv_type: String,
    /// Optional presence string (currently DM-only).
    pub presence_status: String,
    /// Unread message count; primary sort key.
    pub unread_count: i32,
    /// An invite-only channel: the row shows the sidebar's lock glyph.
    pub private_channel: bool,
}

/// Apply the canonical quick switcher ordering and cap the list.
///
/// Sort rule: unread-count descending, then case-insensitive alphabetical
/// by `name`. Truncates to [`QUICK_SWITCHER_MAX_ENTRIES`].
pub fn arrange_entries(mut entries: Vec<QuickSwitcherEntry>) -> Vec<QuickSwitcherEntry> {
    entries.sort_by(|a, b| {
        b.unread_count
            .cmp(&a.unread_count)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries.truncate(QUICK_SWITCHER_MAX_ENTRIES);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, unread: i32) -> QuickSwitcherEntry {
        QuickSwitcherEntry {
            id: name.to_string(),
            name: name.to_string(),
            conv_type: "channel".into(),
            presence_status: String::new(),
            unread_count: unread,
            private_channel: false,
        }
    }

    #[test]
    fn unread_first_then_alphabetical() {
        let input = vec![
            entry("Zeta", 0),
            entry("alpha", 0),
            entry("Beta", 5),
            entry("gamma", 5),
        ];
        let sorted = arrange_entries(input);
        let names: Vec<&str> = sorted.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Beta", "gamma", "alpha", "Zeta"]);
    }

    #[test]
    fn caps_at_max_entries() {
        let input: Vec<QuickSwitcherEntry> =
            (0..30).map(|i| entry(&format!("c{:02}", i), 0)).collect();
        let sorted = arrange_entries(input);
        assert_eq!(sorted.len(), QUICK_SWITCHER_MAX_ENTRIES);
    }
}
