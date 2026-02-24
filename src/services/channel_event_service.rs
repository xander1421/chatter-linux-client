//! Channel event service for handling member events and system messages.
//!
//! Centralizes system message creation for member join/leave events,
//! separating business logic from UI handlers.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::channel::Membership;
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, SystemMessage};
use crate::error::StorageError;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::message_repo::MessageRepository;
use crate::storage::repositories::user_repo::UserRepository;

#[derive(Debug, thiserror::Error)]
pub enum ChannelEventError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Service for handling channel events (member join/leave) and creating system messages.
pub struct ChannelEventService {
    message_repo: Arc<MessageRepository>,
    channel_repo: Arc<ChannelRepository>,
    user_repo: Arc<UserRepository>,
}

impl ChannelEventService {
    /// Create a new ChannelEventService.
    pub fn new(
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        user_repo: Arc<UserRepository>,
    ) -> Self {
        Self {
            message_repo,
            channel_repo,
            user_repo,
        }
    }

    /// Handle a member joining a channel.
    ///
    /// Creates a system message and syncs membership to local storage.
    /// Returns (message, display_name) for UI updates.
    pub fn handle_member_joined(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        membership: &Membership,
    ) -> Result<(Message, String), ChannelEventError> {
        // Get display name
        let display_name = self.get_display_name(user_id);

        // Create system message
        let message = self.create_system_message(
            channel_id,
            SystemMessage::UserJoined(user_id.clone()),
        );

        // Persist message
        self.message_repo.insert(&message)?;

        // Sync membership (insert or ignore if already exists)
        self.channel_repo.insert_or_ignore_membership(membership)?;

        Ok((message, display_name))
    }

    /// Handle a member leaving/being removed from a channel.
    ///
    /// Creates a system message and removes membership from local storage.
    /// Returns (message, display_name) for UI updates.
    pub fn handle_member_removed(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(Message, String), ChannelEventError> {
        let display_name = self.get_display_name(user_id);

        let message = self.create_system_message(
            channel_id,
            SystemMessage::UserLeft(user_id.clone()),
        );

        self.message_repo.insert(&message)?;

        // Remove membership from local storage
        self.channel_repo.delete_membership(channel_id, user_id)?;

        Ok((message, display_name))
    }

    /// Get display name for a user, falling back to user ID string.
    fn get_display_name(&self, user_id: &UserId) -> String {
        match self.user_repo.get(user_id) {
            Ok(Some(user)) => user.username,
            Ok(None) => user_id.to_string(),
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to get user for display name, falling back to ID"
                );
                user_id.to_string()
            }
        }
    }

    /// Create a system message for the given channel.
    fn create_system_message(&self, channel_id: &ChannelId, content: SystemMessage) -> Message {
        let now = Utc::now();
        Message {
            id: MessageId::new(),
            channel_id: channel_id.clone(),
            sender_id: None, // System messages have no sender
            content: MessageContent::System(content),
            thread_id: None,
            reply_to: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            sent_at: 0, // System messages don't use sender timestamp
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
        }
    }
}
