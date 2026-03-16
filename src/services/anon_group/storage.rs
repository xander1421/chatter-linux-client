//! Message storage and retrieval for anonymous groups.

use chrono::Utc;

use crate::domain::anon_group::{
    AnonGroupId, AnonGroupMessage as DomainAnonGroupMessage, AnonGroupMessageId,
};
use crate::domain::{AttachmentId, UserId};
use crate::services::AttachmentService;
use crate::services::gateway::shared;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
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
        let message = DomainAnonGroupMessage {
            id: AnonGroupMessageId::new(),
            group_id: *group_id,
            sender_id: *sender_id,
            content_type,
            content_json: content_json.to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
            is_pinned: false,
            is_read: false,
        };

        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored message {} in group {} with sent_at={}",
            message.id,
            group_id,
            sent_at
        );
        Ok(message.id)
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
        let message = DomainAnonGroupMessage::new_text(*group_id, *sender_id, text, sent_at);
        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored text message {} in group {} with sent_at={}",
            message.id,
            group_id,
            sent_at
        );
        Ok(message.id)
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
        // Determine content type and build JSON
        let content_type = shared::determine_content_type(!text.is_empty(), !attachment_ids.is_empty());
        let content_json = shared::build_content_json(text, attachment_ids);
        let attachments_blob = shared::serialize_attachment_ids(attachment_ids);

        let message = DomainAnonGroupMessage {
            id: AnonGroupMessageId::new(),
            group_id: *group_id,
            sender_id: *sender_id,
            content_type,
            content_json,
            attachments_blob,
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
            is_pinned: false,
            is_read: false,
        };

        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored message {} in group {} with sent_at={}, attachments={}",
            message.id,
            group_id,
            sent_at,
            attachment_ids.len()
        );
        Ok(message.id)
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
        let attachments_blob = shared::serialize_attachment_ids(attachment_ids);

        let message = DomainAnonGroupMessage {
            id: AnonGroupMessageId::new(),
            group_id: *group_id,
            sender_id: *sender_id,
            content_type,
            content_json: content_json.to_string(),
            attachments_blob,
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
            is_pinned: false,
            is_read: false,
        };

        self.anon_group_repo.insert_message(&message)?;
        Ok(message.id)
    }

    /// Get a message by group_id and sent_at timestamp (any sender).
    pub fn get_message_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_message_by_timestamp(group_id, sent_timestamp)?)
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
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self
            .anon_group_repo
            .get_message_by_sender_timestamp(group_id, sender_id, timestamp_ns)?)
    }

    /// Delete a message by sender timestamp (for delete-for-everyone).
    pub fn delete_message_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<bool, AnonGroupError> {
        Ok(self
            .anon_group_repo
            .mark_message_deleted_by_timestamp(group_id, sender_id, timestamp_ns)?)
    }

    /// Get messages for a group with pagination.
    ///
    /// Returns messages ordered by created_at DESC (newest first).
    pub fn get_messages(
        &self,
        group_id: &AnonGroupId,
        before: Option<&chrono::DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_messages(group_id, before, limit)?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonGroupMessageId,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_message(id)?)
    }

    /// Get the most recent message in a group (for sidebar preview).
    pub fn get_latest_message(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_latest_message(group_id)?)
    }

    /// Get the message count for a group.
    pub fn get_message_count(&self, group_id: &AnonGroupId) -> Result<i64, AnonGroupError> {
        Ok(self.anon_group_repo.get_message_count(group_id)?)
    }

    /// Set pin state for a message by group_id and sent_at timestamp.
    pub fn set_message_pinned(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
        pinned: bool,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.set_message_pinned_by_timestamp(group_id, sent_timestamp, pinned)?)
    }

    /// List all pinned messages for a group.
    pub fn list_pinned(&self, group_id: &AnonGroupId) -> Result<Vec<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.list_pinned(group_id)?)
    }

    /// Mark all messages as read for a group.
    pub fn mark_as_read(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        self.anon_group_repo.mark_as_read(group_id)?;
        Ok(())
    }

    /// Delete a message (soft delete).
    pub fn delete_message(&self, id: &AnonGroupMessageId) -> Result<(), AnonGroupError> {
        self.anon_group_repo.delete_message(id)?;
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
        Ok(self.anon_group_repo.update_message_text_by_sender(group_id, sender_id, sent_timestamp, new_text)?)
    }

    /// Add a reaction to a message in the DB.
    pub fn add_reaction(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.add_reaction(group_id, sent_timestamp, user_id, emoji)?)
    }

    /// Remove a reaction from a message in the DB.
    pub fn remove_reaction(
        &self,
        group_id: &AnonGroupId,
        sent_timestamp: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.remove_reaction(group_id, sent_timestamp, user_id, emoji)?)
    }

    /// Get all reaction counts for a group's messages.
    ///
    /// Returns a map of sent_at -> Vec<(emoji, count)>.
    pub fn get_all_reaction_counts(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, AnonGroupError> {
        Ok(self.anon_group_repo.get_all_reaction_counts(group_id)?)
    }
}
