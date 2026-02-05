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
}
