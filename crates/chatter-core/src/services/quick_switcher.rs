//! Quick switcher row type.
//!
//! The Ctrl+K command palette aggregates channels, DMs, anon DMs, and private
//! groups into one list (`ChatterClient::quick_switcher_entries`).

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
