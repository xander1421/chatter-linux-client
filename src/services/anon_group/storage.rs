//! Message storage and retrieval for anonymous groups.

use chrono::Utc;

use crate::domain::anon_group::{
    AnonGroupId, AnonGroupMessage as DomainAnonGroupMessage, AnonGroupMessageId,
};
use crate::domain::{AttachmentId, UserId};
use crate::services::AttachmentService;
use crate::storage::serialization::db_serialize;

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
        // Determine content type: 1=text, 2=attachment only, 3=mixed
        let has_text = !text.is_empty();
        let has_attachments = !attachment_ids.is_empty();

        let (content_type, content_json) = match (has_text, has_attachments) {
            (true, false) => (1, serde_json::json!({ "text": text }).to_string()),
            (false, true) => (
                2,
                serde_json::json!({ "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() })
                    .to_string(),
            ),
            (true, true) => (
                3,
                serde_json::json!({ "text": text, "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() })
                    .to_string(),
            ),
            (false, false) => (0, "{}".to_string()),
        };

        // Serialize attachment IDs to blob
        let attachment_ids_vec: Vec<_> = attachment_ids.to_vec();
        let attachments_blob =
            db_serialize(&attachment_ids_vec).unwrap_or_else(|_| vec![8, 0, 0, 0, 0, 0, 0, 0]);

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

    /// Create attachment records from pointers.
    ///
    /// Returns the list of created attachment IDs for storing in the message.
    pub fn create_attachments_from_pointers(
        &self,
        pointers: &[crate::proto::content_v1::AttachmentPointer],
    ) -> Vec<AttachmentId> {
        let attachment_service = AttachmentService::new(self.attachment_repo.clone());
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
}
