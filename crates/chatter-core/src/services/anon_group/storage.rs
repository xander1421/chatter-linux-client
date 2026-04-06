//! Message storage and retrieval for anonymous groups.

use chrono::Utc;

use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent, SystemMessage};
use crate::domain::{AttachmentId, UserId};
use crate::services::AttachmentService;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Store a system message (join, leave, kick) in a group.
    pub fn store_system_message(
        &self,
        group_id: &AnonGroupId,
        sender_id: Option<UserId>,
        system_content: SystemMessage,
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        let mut message = Message::new_text(channel_id, ConversationType::AnonGroup, sender_id, String::new(), sent_at);
        message.content = MessageContent::System(system_content);

        let msg_id_str = message.id.to_string();
        self.message_service.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored system message {} in group {} at {}",
            msg_id_str,
            group_id,
            sent_at
        );
        Ok(AnonGroupMessageId::from_string(msg_id_str))
    }

    /// Store a message received from a group member.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        let content = Self::content_from_type_and_json(content_type, content_json);
        let mut message = Message::new_text(channel_id, ConversationType::AnonGroup, Some(*sender_id), String::new(), sent_at);
        message.content = content;

        let msg_id_str = message.id.to_string();
        self.message_service.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored message {} in group {} with sent_at={}",
            msg_id_str,
            group_id,
            sent_at
        );
        Ok(AnonGroupMessageId::from_string(msg_id_str))
    }

    /// Store a text message in a group.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_text_message(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        let message = Message::new_text(channel_id, ConversationType::AnonGroup, Some(*sender_id), text.to_string(), sent_at);

        let msg_id_str = message.id.to_string();
        self.message_service.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored text message {} in group {} with sent_at={}",
            msg_id_str,
            group_id,
            sent_at
        );
        Ok(AnonGroupMessageId::from_string(msg_id_str))
    }

    /// Store a message with text and/or attachments.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message_with_attachments(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[AttachmentId],
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        let content = Self::build_message_content(text, attachment_ids);

        let mut message = Message::new_text(channel_id, ConversationType::AnonGroup, Some(*sender_id), String::new(), sent_at);
        message.content = content;
        message.attachments = attachment_ids.to_vec();

        let msg_id_str = message.id.to_string();
        self.message_service.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored message {} in group {} with sent_at={}, attachments={}",
            msg_id_str,
            group_id,
            sent_at,
            attachment_ids.len()
        );
        Ok(AnonGroupMessageId::from_string(msg_id_str))
    }

    /// Store a message with explicit content_json (used for replies with quote metadata).
    pub fn store_message_with_content_json(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        attachment_ids: &[AttachmentId],
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        let content = Self::content_from_type_and_json(content_type, content_json);

        let mut message = Message::new_text(channel_id, ConversationType::AnonGroup, Some(*sender_id), String::new(), sent_at);
        message.content = content;
        message.attachments = attachment_ids.to_vec();

        let msg_id_str = message.id.to_string();
        self.message_service.message_repo.insert(&message)?;
        Ok(AnonGroupMessageId::from_string(msg_id_str))
    }

    /// Get a message by group_id and sent_at timestamp (any sender).
    pub fn get_message_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
    ) -> Result<Option<Message>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.get_by_conversation_timestamp(&channel_id, sent_timestamp)?)
    }

    /// Create attachment records from pointers.
    ///
    /// Returns the list of created attachment IDs for storing in the message.
    pub fn create_attachments_from_pointers(
        &self,
        pointers: &[crate::proto::content_v1::AttachmentPointer],
    ) -> Vec<AttachmentId> {
        let attachment_service = AttachmentService::new_local(self.attachment_repo.clone());
        attachment_service.create_from_pointers(pointers)
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, delete, pin, etc.
    /// Messages are identified by (sender_id, sent_at) tuple per Signal Protocol.
    pub fn get_message_by_sender_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self
            .message_service
            .message_repo
            .get_by_conversation_sender_timestamp(&channel_id, sender_id, timestamp_ns)?)
    }

    /// Delete a message by sender timestamp (for delete-for-everyone).
    pub fn delete_message_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<bool, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self
            .message_service
            .message_repo
            .mark_deleted_by_sender_timestamp(&channel_id, sender_id, timestamp_ns)?)
    }

    /// Get messages for a group with pagination.
    ///
    /// Returns messages ordered by sent_at DESC (newest first).
    pub fn get_messages(
        &self,
        group_id: &AnonGroupId,
        before: Option<&chrono::DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<Message>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        // Convert DateTime cursor to sent_at nanos for the unified repo
        let before_sent_at = before.and_then(|dt| {
            dt.timestamp_nanos_opt().map(|ns| ns as u64)
        });
        Ok(self.message_service.message_repo.list_for_conversation(
            &channel_id,
            &ConversationType::AnonGroup,
            before_sent_at,
            limit as u32,
        )?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonGroupMessageId,
    ) -> Result<Option<Message>, AnonGroupError> {
        let msg_id: MessageId = id.as_str().parse()
            .map_err(|_| AnonGroupError::InvalidData(format!("invalid message id: {}", id)))?;
        Ok(self.message_service.message_repo.get(&msg_id)?)
    }

    /// Get the most recent message in a group (for sidebar preview).
    pub fn get_latest_message(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<Message>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.get_latest_for_conversation(
            &channel_id,
            &ConversationType::AnonGroup,
        )?)
    }

    /// Get the message count for a group.
    pub fn get_message_count(&self, group_id: &AnonGroupId) -> Result<i64, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.count_in_conversation(
            &channel_id,
            &ConversationType::AnonGroup,
        )?)
    }

    /// Set pin state for a message by group_id and sent_at timestamp.
    pub fn set_message_pinned(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
        pinned: bool,
    ) -> Result<bool, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.set_pinned_by_timestamp(&channel_id, &ConversationType::AnonGroup, sent_timestamp, pinned)?)
    }

    /// List all pinned messages for a group.
    pub fn list_pinned(&self, group_id: &AnonGroupId) -> Result<Vec<Message>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.list_pinned_for_conversation(
            &channel_id,
            &ConversationType::AnonGroup,
        )?)
    }

    /// Mark all messages as read for a group.
    pub fn mark_as_read(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        self.message_service.message_repo.mark_conversation_read(&channel_id, &ConversationType::AnonGroup)?;
        Ok(())
    }

    /// Delete a message (soft delete).
    pub fn delete_message(&self, id: &AnonGroupMessageId) -> Result<(), AnonGroupError> {
        let msg_id: MessageId = id.as_str().parse()
            .map_err(|_| AnonGroupError::InvalidData(format!("invalid message id: {}", id)))?;
        self.message_service.message_repo.delete(&msg_id)?;
        Ok(())
    }

    /// Update message text (for handling edits from peer).
    ///
    /// Only updates if sender_id matches the message author (security).
    /// Returns `Ok(true)` if a message was updated, `Ok(false)` if message not found
    /// or sender doesn't match author.
    pub fn update_message_text_by_sender(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        // Verify sender matches message author before updating
        if let Some(msg) = self.message_service.message_repo
            .get_by_conversation_sender_timestamp(&channel_id, sender_id, sent_timestamp)?
        {
            if msg.sender_id.as_ref() != Some(sender_id) {
                return Ok(false);
            }
            Ok(self.message_service.message_repo
                .update_text_by_timestamp(&channel_id, &ConversationType::AnonGroup, sent_timestamp, new_text)?)
        } else {
            Ok(false)
        }
    }

    /// Add a reaction to a message in the DB.
    ///
    /// Uses (sender_id, sent_at_ns) to identify the target message (Signal-style).
    pub fn add_reaction(
        &self,
        _group_id: &AnonGroupId,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.message_service.message_repo
            .add_reaction(sender_id, sent_at_ns, reactor_id, emoji)?)
    }

    /// Remove a reaction from a message in the DB.
    ///
    /// Uses (sender_id, sent_at_ns) to identify the target message (Signal-style).
    pub fn remove_reaction(
        &self,
        _group_id: &AnonGroupId,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.message_service.message_repo
            .remove_reaction(sender_id, sent_at_ns, reactor_id, emoji)?)
    }

    /// Get all reaction counts for a group's messages.
    ///
    /// Returns a map of sent_at -> Vec<(emoji, count)>.
    pub fn get_all_reaction_counts(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, AnonGroupError> {
        let channel_id: ChannelId = (*group_id).into();
        Ok(self.message_service.message_repo.get_reaction_counts_for_conversation(&channel_id, &ConversationType::AnonGroup)?)
    }

    // ========================================================================
    // Helper methods for content conversion
    // ========================================================================

    /// Build MessageContent from text and attachment IDs.
    fn build_message_content(text: &str, attachment_ids: &[AttachmentId]) -> MessageContent {
        match (!text.is_empty(), !attachment_ids.is_empty()) {
            (true, true) => MessageContent::Mixed(MixedContent::new(
                text.to_string(),
                attachment_ids.to_vec(),
            )),
            (true, false) => MessageContent::Text(text.to_string()),
            (false, true) => MessageContent::Mixed(MixedContent::attachments_only(
                attachment_ids.to_vec(),
            )),
            (false, false) => MessageContent::Text(String::new()),
        }
    }

    /// Convert a content_type integer and content_json string to MessageContent.
    ///
    /// This bridges the old anon_group content format to the unified Message format.
    fn content_from_type_and_json(content_type: i32, content_json: &str) -> MessageContent {
        match content_type {
            // Text
            1 => {
                let text = serde_json::from_str::<serde_json::Value>(content_json)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_default();
                MessageContent::Text(text)
            }
            // Mixed (text + attachments) or reply with quote metadata
            3 => {
                // For mixed/reply content, store as Mixed with the text extracted
                let text = serde_json::from_str::<serde_json::Value>(content_json)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from));
                MessageContent::Mixed(MixedContent {
                    text,
                    attachment_ids: Vec::new(),
                })
            }
            // Attachment only or unknown - store as Mixed with no text
            _ => {
                let text = serde_json::from_str::<serde_json::Value>(content_json)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from));
                if let Some(t) = text {
                    MessageContent::Text(t)
                } else {
                    MessageContent::Text(String::new())
                }
            }
        }
    }
}
