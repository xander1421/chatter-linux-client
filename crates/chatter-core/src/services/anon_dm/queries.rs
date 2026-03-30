use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMessageId};
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message};
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
        before_sent_at: Option<u64>,
        limit: i32,
    ) -> Result<Vec<Message>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.list_for_conversation(
            &channel_id,
            &ConversationType::AnonDm,
            before_sent_at,
            limit as u32,
        )?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonDmMessageId,
    ) -> Result<Option<Message>, AnonDmError> {
        let message_id: MessageId = id.as_str().parse()
            .map_err(|_| AnonDmError::InvalidData(format!("invalid message ID: {}", id)))?;
        Ok(self.message_repo.get(&message_id)?)
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
    ) -> Result<Option<Message>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.get_by_conversation_timestamp(&channel_id, timestamp_ns)?)
    }

    pub fn get_message_by_sender_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.get_by_conversation_sender_timestamp(&channel_id, sender_id, timestamp_ns)?)
    }

    /// Get the most recent message in a conversation.
    pub fn get_latest_message(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<Message>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.get_latest_for_conversation(&channel_id, &ConversationType::AnonDm)?)
    }

    /// Get the message count for a conversation.
    pub fn get_message_count(&self, conversation_id: &AnonDmId) -> Result<i64, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.count_in_conversation(&channel_id, &ConversationType::AnonDm)?)
    }

    /// Delete a message.
    pub fn delete_message(&self, id: &AnonDmMessageId) -> Result<(), AnonDmError> {
        let message_id: MessageId = id.as_str().parse()
            .map_err(|_| AnonDmError::InvalidData(format!("invalid message ID: {}", id)))?;
        self.message_repo.delete(&message_id)?;
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
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.mark_deleted_by_sender_timestamp(
            &channel_id,
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
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.set_pinned_by_timestamp(&channel_id, &ConversationType::AnonDm, sent_timestamp, pinned)?)
    }

    /// List all pinned messages for a conversation.
    pub fn list_pinned(&self, conversation_id: &AnonDmId) -> Result<Vec<Message>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.list_pinned_for_conversation(&channel_id, &ConversationType::AnonDm)?)
    }

    /// Add a reaction to an anon DM message in the DB.
    ///
    /// Uses (sender_id, sent_at_ns) to identify the target message (Signal-style).
    pub fn add_reaction(
        &self,
        _conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonDmError> {
        Ok(self.message_repo.add_reaction(sender_id, sent_at_ns, reactor_id, emoji)?)
    }

    /// Remove a reaction from an anon DM message in the DB.
    ///
    /// Uses (sender_id, sent_at_ns) to identify the target message (Signal-style).
    pub fn remove_reaction(
        &self,
        _conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, AnonDmError> {
        Ok(self.message_repo.remove_reaction(sender_id, sent_at_ns, reactor_id, emoji)?)
    }

    /// Get all reaction counts for an anon DM conversation's messages.
    ///
    /// Returns a map of sent_at -> Vec<(emoji, count)>.
    pub fn get_all_reaction_counts(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.get_reaction_counts_for_conversation(&channel_id, &ConversationType::AnonDm)?)
    }
}
