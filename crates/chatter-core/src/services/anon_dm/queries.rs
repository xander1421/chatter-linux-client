use chrono::{DateTime, Utc};
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMessage as DomainAnonDmMessage, AnonDmMessageId};
use crate::domain::UserId;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Get a conversation by ID.
    pub fn get_conversation(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.get_conversation(conversation_id)?)
    }

    /// Get a conversation by peer ID.
    pub fn get_conversation_by_peer(&self, peer_id: &UserId) -> Result<Option<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.get_conversation_by_peer(peer_id)?)
    }

    /// List all active conversations.
    pub fn list_conversations(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_active_conversations()?)
    }

    /// List pending message requests (incoming only).
    pub fn list_message_requests(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_message_requests()?)
    }

    /// List outgoing pending requests.
    pub fn list_outgoing_requests(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_outgoing_requests()?)
    }

    /// Get total unread message count.
    pub fn get_total_unread_count(&self) -> Result<u32, AnonDmError> {
        Ok(self.anon_dm_repo.get_total_unread_count()?)
    }

    /// Get messages for a conversation with pagination.
    pub fn get_messages(
        &self,
        conversation_id: &AnonDmId,
        before: Option<&DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<DomainAnonDmMessage>, AnonDmError> {
        Ok(self
            .anon_dm_repo
            .get_messages(conversation_id, before, limit)?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonDmMessageId,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_message(id)?)
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, etc. where we identify
    /// messages by (sender_id, sent_at) tuple per Signal Protocol convention.
    /// Look up a message by conversation + timestamp (any sender).
    pub fn get_message_by_timestamp(
        &self,
        conversation_id: &AnonDmId,
        timestamp_ns: u64,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_message_by_timestamp(conversation_id, timestamp_ns)?)
    }

    pub fn get_message_by_sender_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_message_by_sender_timestamp(conversation_id, sender_id, timestamp_ns)?)
    }

    /// Get the most recent message in a conversation.
    pub fn get_latest_message(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_latest_message(conversation_id)?)
    }

    /// Get the message count for a conversation.
    pub fn get_message_count(&self, conversation_id: &AnonDmId) -> Result<i64, AnonDmError> {
        Ok(self.anon_dm_repo.get_message_count(conversation_id)?)
    }

    /// Delete a message.
    pub fn delete_message(&self, id: &AnonDmMessageId) -> Result<(), AnonDmError> {
        self.anon_dm_repo.delete_message(id)?;
        Ok(())
    }

    /// Delete a message by sender timestamp (for incoming delete-for-everyone).
    ///
    /// Uses (conversation_id, sender_id, sent_timestamp) as the Signal-style lookup key.
    pub fn delete_message_by_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.mark_message_deleted_by_timestamp(
            conversation_id,
            sender_id,
            sent_timestamp,
        )?)
    }

    /// Set pin state for a message by conversation_id and sent_at timestamp.
    pub fn set_message_pinned(
        &self,
        conversation_id: &AnonDmId,
        sent_timestamp: u64,
        pinned: bool,
    ) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.set_message_pinned_by_timestamp(conversation_id, sent_timestamp, pinned)?)
    }

    /// List all pinned messages for a conversation.
    pub fn list_pinned(&self, conversation_id: &AnonDmId) -> Result<Vec<crate::domain::anon_dm::AnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.list_pinned(conversation_id)?)
    }

    /// Add a reaction to an anon DM message in the DB.
    pub fn add_reaction(
        &self,
        conversation_id: &AnonDmId,
        sent_timestamp: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.add_reaction(conversation_id, sent_timestamp, user_id, emoji)?)
    }

    /// Remove a reaction from an anon DM message in the DB.
    pub fn remove_reaction(
        &self,
        conversation_id: &AnonDmId,
        sent_timestamp: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.remove_reaction(conversation_id, sent_timestamp, user_id, emoji)?)
    }

    /// Get all reaction counts for an anon DM conversation's messages.
    ///
    /// Returns a map of sent_at -> Vec<(emoji, count)>.
    pub fn get_all_reaction_counts(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, AnonDmError> {
        Ok(self.anon_dm_repo.get_all_reaction_counts(conversation_id)?)
    }
}
