//! Raw sidebar rows and their ordering rules.

use crate::domain::message::ConversationType;

/// The display order of the scrollable sidebar. Folder children are present
/// only while expanded, so the UI can virtualize one contiguous model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarSlot {
    Search,
    Saved,
    Friends,
    Calls,
    Requests,
    ChannelActions,
    Empty,
    Loading,
    Chat(usize),
    SectionHeader(usize),
    SectionChat(usize, usize),
    Bottom,
}

pub fn sidebar_slots(chat_count: usize, sections: &[(bool, usize)]) -> Vec<SidebarSlot> {
    use SidebarSlot as S;
    let mut slots = vec![S::Search, S::Saved, S::Friends, S::Calls, S::Requests, S::ChannelActions];
    if chat_count == 0 && sections.is_empty() {
        slots.push(S::Empty);
    }
    if chat_count == 0 {
        slots.push(S::Loading);
    }
    slots.extend((0..chat_count).map(S::Chat));
    for (section, &(expanded, count)) in sections.iter().enumerate() {
        slots.push(S::SectionHeader(section));
        if expanded {
            slots.extend((0..count).map(|chat| S::SectionChat(section, chat)));
        }
    }
    slots.push(S::Bottom);
    slots
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChatRowLabel {
    #[default]
    Plain,
    IncomingSecretRequest,
    OutgoingSecretRequest,
}

// ============================================================================
// Chat list row
// ============================================================================

/// One row of the chat list — a friend DM, an anon DM, a channel (current or
/// former) or a private group — Send-safe, so it is built off the UI thread
/// and turned into a `slint::Image`-carrying row on it.
///
/// The chat list is ONE list ordered by activity (Signal/Telegram), so every
/// kind shares this shape; fields a kind does not use stay at their default.
#[derive(Clone, Debug)]
pub struct ChatRowSendData {
    pub kind: ConversationType,
    /// The conversation key: the peer's user id for a friend DM, the anon DM
    /// id, the channel id, the group id.
    pub id: String,
    /// DM rows: the peer. Empty otherwise.
    pub user_id: String,
    pub display_name: String,
    pub label: ChatRowLabel,
    /// Raw avatar bytes (a DM peer's photo, a channel's icon).
    pub avatar_bytes: Option<Vec<u8>>,
    pub presence_status: String,
    pub last_message: String,
    /// Activity time shown at the right of the row, before local formatting.
    pub timestamp_at: Option<chrono::DateTime<chrono::Utc>>,
    pub unread_count: u32,
    pub mention_count: u32,
    pub is_typing: bool,
    /// Local-only pin flag; pinned rows lead the list.
    pub is_pinned: bool,
    /// Channel rows: invite-only (lock glyph rather than #).
    pub private_channel: bool,
    /// Private-group rows.
    pub member_count: u32,
    /// Channel rows: core's `can_leave_channel`.
    pub can_leave: bool,
    /// Channel rows: core's `is_former_channel` — read-only, offer "Delete chat".
    pub can_delete: bool,
    /// When the conversation last saw activity: the order key. `None` (no
    /// messages yet) sinks below every dated row.
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
}

impl ChatRowSendData {
    /// A row of `kind` with everything else empty; builders fill what they know.
    #[must_use]
    pub fn new(
        kind: ConversationType,
        id: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        let display_name = display_name.into();
        Self {
            kind,
            id: id.into(),
            user_id: String::new(),
            display_name,
            label: ChatRowLabel::Plain,
            avatar_bytes: None,
            presence_status: String::new(),
            last_message: String::new(),
            timestamp_at: None,
            unread_count: 0,
            mention_count: 0,
            is_typing: false,
            is_pinned: false,
            private_channel: false,
            member_count: 0,
            can_leave: false,
            can_delete: false,
            last_activity: None,
        }
    }

    /// True for anonymous (ephemeral) DMs.
    #[must_use]
    pub fn is_anon_dm(&self) -> bool {
        self.kind == ConversationType::AnonDm
    }
}

/// The chat list's order: pinned first, then newest activity first, rows with
/// no activity last. Stable, so rows that tie keep the order they were built in.
pub fn order_chat_rows(rows: &mut [ChatRowSendData]) {
    rows.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    rows.sort_by_key(|r| !r.is_pinned);
}

/// Where a row with new activity goes, in a list already in
/// [`order_chat_rows`] order (`others` = every other row's pin flag, top
/// down): an unpinned row just below the last pinned one, never above a pin;
/// a pinned row to the top of the pinned block.
pub fn chat_row_top_slot(is_pinned: bool, others: impl IntoIterator<Item = bool>) -> usize {
    let pins: Vec<bool> = others.into_iter().collect();
    if is_pinned {
        pins.iter().position(|&p| p).unwrap_or(0)
    } else {
        pins.iter().rposition(|&p| p).map_or(0, |i| i + 1)
    }
}

/// Send-safe intermediate struct for FriendData (Friends-tab rows).
/// Unifies server-side friendships and accepted anon-DM peers so both
/// appear in the same list — `is_anon` is the discriminator, `user_id`
/// doubles as the anon_dm_id for anon rows.
#[derive(Clone, Debug)]
pub struct FriendDataSendData {
    pub user_id: String,
    pub display_name: String,
    pub username: String,
    pub avatar_bytes: Option<Vec<u8>>,
    pub presence_status: String,
    pub custom_status: String,
    pub is_anon: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_rows_pinned_first_then_newest_activity() {
        let row = |id: &str, kind: ConversationType, at: Option<i64>, pinned: bool| {
            let mut r = ChatRowSendData::new(kind, id, id);
            r.last_activity = at.map(|s| chrono::DateTime::from_timestamp(s, 0).unwrap());
            r.is_pinned = pinned;
            r
        };
        let mut rows = vec![
            row("old-dm", ConversationType::ServerDm, Some(10), false),
            row("silent-channel", ConversationType::Channel, None, false),
            row("new-channel", ConversationType::Channel, Some(30), false),
            row("pinned-old", ConversationType::ServerDm, Some(1), true),
            row("group", ConversationType::AnonGroup, Some(20), false),
        ];
        order_chat_rows(&mut rows);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "pinned-old",
                "new-channel",
                "group",
                "old-dm",
                "silent-channel"
            ]
        );
    }

    #[test]
    fn live_activity_never_lands_above_a_pin() {
        assert_eq!(chat_row_top_slot(false, [true, true, false, false]), 2);
        assert_eq!(chat_row_top_slot(false, [false, false]), 0);
        assert_eq!(chat_row_top_slot(false, []), 0);
        assert_eq!(chat_row_top_slot(true, [true, false]), 0);
        assert_eq!(chat_row_top_slot(true, [false, false]), 0);
    }
}
