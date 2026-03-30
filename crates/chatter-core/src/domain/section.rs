//! Custom sidebar section domain types.
//!
//! Pure domain structs for user-defined sidebar sections that group conversations.

use crate::domain::ids::SectionId;
use crate::domain::message::ConversationType;

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
    /// Conversations assigned to this section.
    pub conversations: Vec<SectionConversation>,
    /// Aggregate unread count across all conversations in this section.
    pub unread_count: u32,
}

/// A conversation belonging to a section.
#[derive(Clone, Debug)]
pub struct SectionConversation {
    /// The conversation's unique identifier.
    pub conversation_id: String,
    /// The type of conversation (channel, DM, anon, etc.).
    pub conversation_type: ConversationType,
}
