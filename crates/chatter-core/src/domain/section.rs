//! Custom sidebar section domain types.
//!
//! Pure domain structs for user-defined sidebar sections that group conversations.

use crate::domain::ids::SectionId;
use crate::domain::message::ConversationType;

/// How the conversations inside one section are ordered.
///
/// Filing a conversation into a section used to silently change its order from
/// MRU (which is what every other sidebar list uses) to the order you happened
/// to file it in. `Recent` is the default so a folder sorts the same way the
/// list the conversation came from does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SectionSortMode {
    /// Most recent message first — matches the rest of the sidebar.
    #[default]
    Recent,
    /// Conversations with unread messages first, then most recent.
    Unread,
    /// Case-insensitive by display name.
    Alphabetical,
}

impl SectionSortMode {
    /// Stable string used in the database and across the Slint boundary.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Unread => "unread",
            Self::Alphabetical => "alphabetical",
        }
    }

    /// Parse a stored/UI value. Unknown values fall back to the default rather
    /// than failing — a sort mode is a preference, not data worth erroring on.
    pub fn from_str_value(s: &str) -> Self {
        match s {
            "unread" => Self::Unread,
            "alphabetical" => Self::Alphabetical,
            _ => Self::Recent,
        }
    }
}

/// A custom sidebar section that groups conversations.
#[derive(Clone, Debug)]
pub struct CustomSection {
    /// Unique section identifier.
    pub id: SectionId,
    /// Display name (e.g. "Work", "Gaming").
    pub name: String,
    /// Emoji icon for the section header.
    pub icon_emoji: String,
    /// Sort position among sections.
    pub sort_order: u32,
    /// Whether the section is expanded in the sidebar.
    pub is_expanded: bool,
    /// How the conversations below are ordered.
    pub sort_mode: SectionSortMode,
    /// Conversations assigned to this section, already ordered by `sort_mode`.
    pub conversations: Vec<SectionConversation>,
    /// Aggregate unread count across all conversations in this section.
    pub unread_count: u32,
}

/// A conversation belonging to a section, enriched with the metadata the
/// sidebar needs to render and order it.
///
/// The enrichment is resolved once per `list_sections_with_conversations` call
/// from a single unified-conversation query; callers must not re-query per row.
#[derive(Clone, Debug)]
pub struct SectionConversation {
    /// The conversation's unique identifier.
    pub conversation_id: String,
    /// The type of conversation (channel, DM, anon, etc.).
    pub conversation_type: ConversationType,
    /// Resolved display name (empty when the conversation no longer exists).
    pub display_name: String,
    /// Unread messages in this conversation.
    pub unread_count: u32,
    /// Last message timestamp, used by `SectionSortMode::Recent`.
    pub last_message_at: Option<i64>,
    /// Local-only pin flag; pinned conversations sort to the top of the folder.
    pub is_pinned: bool,
}
