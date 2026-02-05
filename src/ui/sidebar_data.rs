//! Sidebar data structures for Slint UI bindings.
//!
//! Provides Rust structs that map to Slint struct definitions for
//! sidebar components like channel list items.

use crate::domain::channel::{Channel, ChannelType};

// ============================================================================
// Channel List Item Data
// ============================================================================

/// Channel list item data for Slint model binding.
///
/// Maps to the properties of `ChannelListItem` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct ChannelListItemData {
    /// Channel ID for identification.
    pub channel_id: String,
    /// Display name of the channel.
    pub name: String,
    /// Channel type: "public", "private", or "dm".
    pub channel_type: String,
    /// Number of unread messages.
    pub unread_count: u32,
}

impl ChannelListItemData {
    /// Create a new channel list item data entry.
    #[must_use]
    pub fn new(
        channel_id: impl Into<String>,
        name: impl Into<String>,
        channel_type: impl Into<String>,
        unread_count: u32,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            name: name.into(),
            channel_type: channel_type.into(),
            unread_count,
        }
    }

    /// Create a public channel item.
    #[must_use]
    pub fn public(channel_id: impl Into<String>, name: impl Into<String>, unread_count: u32) -> Self {
        Self::new(channel_id, name, "public", unread_count)
    }

    /// Create a private channel item.
    #[must_use]
    pub fn private(channel_id: impl Into<String>, name: impl Into<String>, unread_count: u32) -> Self {
        Self::new(channel_id, name, "private", unread_count)
    }

    /// Create a DM channel item.
    #[must_use]
    pub fn dm(channel_id: impl Into<String>, name: impl Into<String>, unread_count: u32) -> Self {
        Self::new(channel_id, name, "dm", unread_count)
    }
}

impl From<&Channel> for ChannelListItemData {
    fn from(channel: &Channel) -> Self {
        let channel_type = match channel.channel_type {
            ChannelType::Public => "public",
            ChannelType::Private => "private",
            ChannelType::DirectMessage => "dm",
        };

        Self {
            channel_id: channel.id.to_string(),
            name: channel.name.clone(),
            channel_type: channel_type.to_string(),
            unread_count: channel.unread_count,
        }
    }
}

impl From<Channel> for ChannelListItemData {
    fn from(channel: Channel) -> Self {
        Self::from(&channel)
    }
}

// ============================================================================
// DM List Item Data
// ============================================================================

/// DM list item data for Slint model binding.
///
/// Maps to the properties of `DMListItem` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct DMListItemData {
    /// User ID for identification.
    pub user_id: String,
    /// Display name of the user.
    pub display_name: String,
    /// Avatar URL (empty if none).
    pub avatar_url: String,
    /// Initials for avatar fallback.
    pub initials: String,
    /// Presence status: "online", "away", "dnd", "offline".
    pub presence_status: String,
    /// Last message preview text.
    pub last_message: String,
    /// Timestamp string (e.g., "2h", "Mon").
    pub timestamp: String,
    /// Number of unread messages.
    pub unread_count: u32,
}

impl DMListItemData {
    /// Create a new DM list item data entry.
    #[must_use]
    pub fn new(
        user_id: impl Into<String>,
        display_name: impl Into<String>,
        initials: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            display_name: display_name.into(),
            initials: initials.into(),
            avatar_url: String::new(),
            presence_status: "offline".to_string(),
            last_message: String::new(),
            timestamp: String::new(),
            unread_count: 0,
        }
    }

    /// Set avatar URL.
    #[must_use]
    pub fn with_avatar(mut self, url: impl Into<String>) -> Self {
        self.avatar_url = url.into();
        self
    }

    /// Set presence status.
    #[must_use]
    pub fn with_presence(mut self, status: impl Into<String>) -> Self {
        self.presence_status = status.into();
        self
    }

    /// Set last message preview.
    #[must_use]
    pub fn with_last_message(mut self, message: impl Into<String>) -> Self {
        self.last_message = message.into();
        self
    }

    /// Set timestamp.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    /// Set unread count.
    #[must_use]
    pub fn with_unread(mut self, count: u32) -> Self {
        self.unread_count = count;
        self
    }
}

/// Convert a presence status string to a hex color for Slint.
///
/// Returns color values matching Colors.online/away/dnd/offline in Slint.
#[must_use]
pub fn presence_color(status: &str) -> &'static str {
    match status.to_lowercase().as_str() {
        "online" => "#10b981",
        "away" => "#f59e0b",
        "dnd" => "#ef4444",
        _ => "#6b7280",
    }
}

// ============================================================================
// User Profile Bar Data
// ============================================================================

/// User profile bar data for Slint model binding.
///
/// Maps to the properties of `UserProfileBar` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct UserProfileBarData {
    /// Display name.
    pub display_name: String,
    /// Avatar URL (empty if none).
    pub avatar_url: String,
    /// Initials for avatar fallback.
    pub initials: String,
    /// Presence status: "online", "away", "dnd", "offline".
    pub presence_status: String,
    /// Custom status text.
    pub custom_status: String,
}

impl UserProfileBarData {
    /// Create a new user profile bar data entry.
    #[must_use]
    pub fn new(display_name: impl Into<String>, initials: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            initials: initials.into(),
            avatar_url: String::new(),
            presence_status: "online".to_string(),
            custom_status: String::new(),
        }
    }

    /// Set avatar URL.
    #[must_use]
    pub fn with_avatar(mut self, url: impl Into<String>) -> Self {
        self.avatar_url = url.into();
        self
    }

    /// Set presence status.
    #[must_use]
    pub fn with_presence(mut self, status: impl Into<String>) -> Self {
        self.presence_status = status.into();
        self
    }

    /// Set custom status text.
    #[must_use]
    pub fn with_custom_status(mut self, status: impl Into<String>) -> Self {
        self.custom_status = status.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{ChannelId, UserId};

    #[test]
    fn test_channel_list_item_data_new() {
        let item = ChannelListItemData::new("chan123", "general", "public", 5);

        assert_eq!(item.channel_id, "chan123");
        assert_eq!(item.name, "general");
        assert_eq!(item.channel_type, "public");
        assert_eq!(item.unread_count, 5);
    }

    #[test]
    fn test_channel_list_item_factories() {
        let public = ChannelListItemData::public("c1", "announcements", 0);
        assert_eq!(public.channel_type, "public");

        let private = ChannelListItemData::private("c2", "secret", 3);
        assert_eq!(private.channel_type, "private");

        let dm = ChannelListItemData::dm("c3", "Alice", 1);
        assert_eq!(dm.channel_type, "dm");
    }

    #[test]
    fn test_from_channel() {
        let channel_id = ChannelId::new();
        let owner_id = UserId::new();
        let mut channel = Channel::new(channel_id, "test-channel".to_string(), ChannelType::Private, owner_id);
        channel.unread_count = 7;

        let item: ChannelListItemData = (&channel).into();

        assert_eq!(item.channel_id, channel_id.to_string());
        assert_eq!(item.name, "test-channel");
        assert_eq!(item.channel_type, "private");
        assert_eq!(item.unread_count, 7);
    }

    #[test]
    fn test_from_dm_channel() {
        let channel_id = ChannelId::new();
        let owner_id = UserId::new();
        let channel = Channel::new(channel_id, "Bob".to_string(), ChannelType::DirectMessage, owner_id);

        let item: ChannelListItemData = channel.into();

        assert_eq!(item.channel_type, "dm");
    }

    #[test]
    fn test_dm_list_item_data() {
        let item = DMListItemData::new("user123", "Alice", "AL")
            .with_presence("online")
            .with_last_message("Hey there!")
            .with_timestamp("2h")
            .with_unread(5);

        assert_eq!(item.user_id, "user123");
        assert_eq!(item.display_name, "Alice");
        assert_eq!(item.initials, "AL");
        assert_eq!(item.presence_status, "online");
        assert_eq!(item.last_message, "Hey there!");
        assert_eq!(item.timestamp, "2h");
        assert_eq!(item.unread_count, 5);
    }

    #[test]
    fn test_presence_color() {
        assert_eq!(presence_color("online"), "#10b981");
        assert_eq!(presence_color("away"), "#f59e0b");
        assert_eq!(presence_color("dnd"), "#ef4444");
        assert_eq!(presence_color("offline"), "#6b7280");
        assert_eq!(presence_color("unknown"), "#6b7280");
        assert_eq!(presence_color("ONLINE"), "#10b981"); // case insensitive
    }

    #[test]
    fn test_user_profile_bar_data() {
        let profile = UserProfileBarData::new("Bob Smith", "BS")
            .with_presence("away")
            .with_custom_status("In a meeting");

        assert_eq!(profile.display_name, "Bob Smith");
        assert_eq!(profile.initials, "BS");
        assert_eq!(profile.presence_status, "away");
        assert_eq!(profile.custom_status, "In a meeting");
    }
}
