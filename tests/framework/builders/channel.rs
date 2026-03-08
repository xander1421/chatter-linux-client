//! Channel builder for creating test channels.

use crate::framework::fixtures::{TestChannel, unique_channel_name};

/// Builder for creating test channels.
pub struct ChannelBuilder {
    name: Option<String>,
    channel_type: i32,
    description: Option<String>,
    creator_id: Option<String>,
    member_ids: Vec<String>,
}

impl ChannelBuilder {
    /// Create a public channel builder.
    pub fn public(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            channel_type: 1, // Public
            description: None,
            creator_id: None,
            member_ids: Vec::new(),
        }
    }

    /// Create a private channel builder.
    pub fn private(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            channel_type: 2, // Private
            description: None,
            creator_id: None,
            member_ids: Vec::new(),
        }
    }

    /// Create a DM channel builder.
    pub fn dm() -> Self {
        Self {
            name: None,
            channel_type: 3, // Direct Message
            description: None,
            creator_id: None,
            member_ids: Vec::new(),
        }
    }

    /// Create a group DM channel builder.
    pub fn group_dm(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            channel_type: 4, // Group DM
            description: None,
            creator_id: None,
            member_ids: Vec::new(),
        }
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the creator.
    pub fn created_by(mut self, user_id: impl Into<String>) -> Self {
        let id = user_id.into();
        self.creator_id = Some(id.clone());
        if !self.member_ids.contains(&id) {
            self.member_ids.push(id);
        }
        self
    }

    /// Add a member.
    pub fn with_member(mut self, user_id: impl Into<String>) -> Self {
        let id = user_id.into();
        if !self.member_ids.contains(&id) {
            self.member_ids.push(id);
        }
        self
    }

    /// Add multiple members.
    pub fn with_members(mut self, user_ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        for id in user_ids {
            let id = id.into();
            if !self.member_ids.contains(&id) {
                self.member_ids.push(id);
            }
        }
        self
    }

    /// Build the channel.
    pub fn build(self) -> TestChannel {
        TestChannel {
            id: uuid::Uuid::new_v4().to_string(),
            name: self.name.unwrap_or_else(unique_channel_name),
            channel_type: self.channel_type,
            description: self.description,
            creator_id: self.creator_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            member_ids: self.member_ids,
            created_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Build a DM channel between two users.
    pub fn build_dm_between(user1: impl Into<String>, user2: impl Into<String>) -> TestChannel {
        Self::dm()
            .with_member(user1)
            .with_member(user2)
            .build()
    }
}
