//! Unified conversation domain entity.
//!
//! Represents a consolidated view of all conversation types (Channel, DM, Anon DM, Anon Group).
//! Used for the unified sidebar and search.

use crate::domain::ids::UserId;
use crate::domain::message::ConversationType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Represents a conversation in the unified list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnifiedConversation {
    /// Unique identifier (ChannelId, AnonDmId, or AnonGroupId as string).
    pub id: String,
    /// Type of conversation.
    pub conversation_type: ConversationType,
    /// Display name (channel name, peer display name, or group name).
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Owner of the conversation (if applicable).
    pub owner_id: Option<UserId>,
    /// Our role in the conversation (if applicable).
    pub my_role: String,
    /// Current status (Active, Pending, etc.).
    pub status: i32,
    /// Whether the conversation is archived.
    pub archived: bool,
    /// When the last message was sent/received.
    pub last_message_at: Option<DateTime<Utc>>,
    /// Unread message count.
    pub unread_count: u32,
    /// When the conversation was created.
    pub created_at: DateTime<Utc>,
    /// When the conversation was last updated.
    pub updated_at: DateTime<Utc>,
}
