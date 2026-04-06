//! Channel domain entity.
//!
//! Represents chat channels, membership, and permissions.

use crate::domain::ids::{ChannelId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Channel type (matches proto ChannelType).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ChannelType {
    #[default]
    Public,
    Private,
    DirectMessage,
    SavedMessages,
}

/// Message persistence policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MessagePersistence {
    #[default]
    Permanent,
    Ephemeral,
    Hybrid,
}

/// Channel configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub persistence: MessagePersistence,
    pub max_members: Option<u32>,
    pub default_ttl_seconds: Option<u32>,
    pub allow_reactions: bool,
    pub allow_threads: bool,
    pub allow_attachments: bool,
    pub require_encryption: bool,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            persistence: MessagePersistence::Permanent,
            max_members: None,
            default_ttl_seconds: None,
            allow_reactions: true,
            allow_threads: true,
            allow_attachments: true,
            require_encryption: true,
        }
    }
}

/// Represents a chat channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub channel_type: ChannelType,
    pub name: String,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub owner_id: UserId,
    pub config: ChannelConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    /// Unread message count (local state)
    pub unread_count: u32,
    /// Last message timestamp for sorting
    pub last_message_at: Option<DateTime<Utc>>,
}

impl Channel {
    /// Create a new channel with defaults.
    #[must_use]
    pub fn new(id: ChannelId, name: String, channel_type: ChannelType, owner_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id,
            channel_type,
            name,
            description: None,
            image_url: None,
            owner_id,
            config: ChannelConfig::default(),
            created_at: now,
            updated_at: now,
            archived: false,
            unread_count: 0,
            last_message_at: None,
        }
    }

    /// Check if this is a direct message channel.
    #[must_use]
    pub fn is_dm(&self) -> bool {
        matches!(self.channel_type, ChannelType::DirectMessage)
    }

    /// Check if this channel is private (Private or DirectMessage).
    #[must_use]
    pub fn is_private(&self) -> bool {
        matches!(
            self.channel_type,
            ChannelType::Private | ChannelType::DirectMessage
        )
    }

    /// Get the correct ConversationType for this channel.
    ///
    /// Encrypted channels use the anonymous group mailbox system (NDSS '21),
    /// so their messages are stored as AnonGroup. Non-encrypted channels
    /// use the server-side Channel path.
    #[must_use]
    pub fn conversation_type(&self) -> crate::domain::message::ConversationType {
        use crate::domain::message::ConversationType;
        match self.channel_type {
            ChannelType::DirectMessage => ConversationType::ServerDm,
            ChannelType::SavedMessages => ConversationType::SavedMessages,
            _ if self.config.require_encryption => ConversationType::AnonGroup,
            _ => ConversationType::Channel,
        }
    }
}

/// Channel member role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MemberRole {
    #[default]
    Member,
    Moderator,
    Admin,
    Owner,
}

/// Channel membership information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Membership {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    /// Last read message ID for tracking unread
    pub last_read_message_id: Option<crate::domain::ids::MessageId>,
    pub is_muted: bool,
    pub muted_until: Option<DateTime<Utc>>,
    pub notification_preference: NotificationPreference,
}

/// Channel member with profile information.
///
/// Combines membership data with user profile for display.
#[derive(Clone, Debug)]
pub struct MemberWithProfile {
    pub membership: Membership,
    pub display_name: String,
    pub avatar_url: Option<String>,
}

/// Notification preference for a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotificationPreference {
    #[default]
    All,
    Mentions,
    None,
}

/// Channel permissions (what actions a role can perform).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    pub can_send_messages: bool,
    pub can_edit_own_messages: bool,
    pub can_delete_own_messages: bool,
    pub can_delete_any_message: bool,
    pub can_pin_messages: bool,
    pub can_manage_members: bool,
    pub can_manage_roles: bool,
    pub can_edit_channel: bool,
    pub can_delete_channel: bool,
}

impl Permissions {
    /// Default permissions for a member.
    #[must_use]
    pub fn member() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: false,
            can_pin_messages: false,
            can_manage_members: false,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Permissions for a moderator.
    #[must_use]
    pub fn moderator() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: true,
            can_pin_messages: true,
            can_manage_members: true,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Full permissions for owner/admin.
    #[must_use]
    pub fn admin() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: true,
            can_pin_messages: true,
            can_manage_members: true,
            can_manage_roles: true,
            can_edit_channel: true,
            can_delete_channel: true,
        }
    }

    /// Permissions for DM participants — both users can delete any message.
    #[must_use]
    pub fn dm() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: true,
            can_pin_messages: true,
            can_manage_members: false,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Get permissions for a given role.
    #[must_use]
    pub fn for_role(role: MemberRole) -> Self {
        match role {
            MemberRole::Member => Self::member(),
            MemberRole::Moderator => Self::moderator(),
            MemberRole::Admin | MemberRole::Owner => Self::admin(),
        }
    }
}

/// Channel template for pre-configured channels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    pub channel_type: ChannelType,
    pub config: ChannelConfig,
    pub icon: Option<String>,
}
