//! Polymorphic conversation operations.
//!
//! `ConversationHandle` wraps `ActiveConversation` + `ChatterClient` to provide
//! a single API for messaging operations across all conversation types
//! (Channel, DM, Anon DM, Anon Group).
//!
//! # Usage
//!
//! ```ignore
//! let handle = ConversationHandle::current(&ui_ctx);
//! handle.send_reaction("👍", &author, timestamp, false).await?;
//! ```
//!
//! This eliminates the 4-way match blocks in the chat UI layer.

use std::sync::Arc;

use crate::client::ChatterClient;
use crate::domain::ids::{ChannelId, UserId};
use crate::state::ActiveConversation;

/// Unified error type for conversation operations.
pub type ConversationResult<T = ()> = Result<T, ConversationError>;

#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("No active conversation")]
    NoConversation,
    #[error("Anon DM error: {0}")]
    AnonDm(#[from] crate::services::AnonDmError),
    #[error("Anon group error: {0}")]
    AnonGroup(#[from] crate::services::AnonGroupError),
    #[error("Message error: {0}")]
    Message(#[from] crate::services::MessageError),
    #[error("{0}")]
    Other(String),
}

/// Polymorphic conversation operations.
///
/// Wraps `ActiveConversation` + `ChatterClient` to provide a single API
/// for all conversation types. The correct service is dispatched
/// internally based on the conversation variant.
#[derive(Clone)]
pub struct ConversationHandle {
    conversation: ActiveConversation,
    client: Arc<ChatterClient>,
}

impl ConversationHandle {
    /// Create a handle for a specific conversation.
    pub fn new(conversation: ActiveConversation, client: Arc<ChatterClient>) -> Self {
        Self { conversation, client }
    }

    /// Create for the current active conversation.
    pub fn current(client: &Arc<ChatterClient>) -> Self {
        Self {
            conversation: client.chat_state().active_conversation(),
            client: client.clone(),
        }
    }

    /// Get the underlying conversation variant.
    pub fn conversation(&self) -> &ActiveConversation {
        &self.conversation
    }

    // ========================================================================
    // Ephemeral operations (reaction, typing, pin, edit, delete)
    // ========================================================================

    /// Send a reaction.
    pub async fn send_reaction(
        &self,
        emoji: &str,
        target_author: &str,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .send_reaction(id, emoji, target_author, target_sent_timestamp, remove)
                    .await?;
                // Persist own reaction
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    if remove {
                        let _ = self.client.anon_dms().remove_reaction(id, target_sent_timestamp, &user_id, emoji);
                    } else {
                        let _ = self.client.anon_dms().add_reaction(id, target_sent_timestamp, &user_id, emoji);
                    }
                }
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .send_reaction(id, emoji, target_author, target_sent_timestamp, remove)
                    .await?;
                // Persist own reaction
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.anon_groups().add_reaction(id, target_sent_timestamp, &user_id, emoji);
                }
                Ok(())
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_reaction_content(
                    emoji, remove, &author_aci, target_sent_timestamp,
                );
                self.send_dm_content(content, peer_id).await?;
                // Persist own reaction to DB
                let _ = self.client.messages().update_reaction(&msg.id, emoji, !remove);
                Ok(())
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_reaction_content_with_channel(
                    emoji, remove, &author_aci, target_sent_timestamp,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                // Persist own reaction to DB
                let _ = self.client.messages().update_reaction(&msg.id, emoji, !remove);
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_reaction_content_with_channel(
                    emoji, remove, &author_aci, target_sent_timestamp,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                // Persist own reaction to DB
                let _ = self.client.messages().update_reaction(&msg.id, emoji, !remove);
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send a typing indicator.
    pub async fn send_typing(&self, started: bool) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms().send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups().send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let content = crate::services::build_typing_content(started);
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let content = crate::services::build_typing_content_with_channel(
                    started, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::build_typing_content_with_channel(
                    started, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send a pin/unpin and persist locally.
    pub async fn send_pin(
        &self,
        target_author: &str,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> ConversationResult {
        let new_pinned = !unpin;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .send_pin(id, target_author, target_sent_timestamp, unpin)
                    .await?;
                let _ = self.client.anon_dms().set_message_pinned(id, target_sent_timestamp, new_pinned);
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .send_pin(id, target_author, target_sent_timestamp, unpin)
                    .await?;
                let _ = self.client.anon_groups().set_message_pinned(id, target_sent_timestamp, new_pinned);
                Ok(())
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content(
                    &author_aci, target_sent_timestamp, unpin,
                );
                self.send_dm_content(content, peer_id).await?;
                let _ = self.client.messages().set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci, target_sent_timestamp, unpin,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                let _ = self.client.messages().set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci, target_sent_timestamp, unpin,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                let _ = self.client.messages().set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send an edit and persist locally.
    pub async fn send_edit(
        &self,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                // anon_dms().send_edit() persists internally
                self.client.anon_dms()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                // anon_groups().send_edit() persists internally
                self.client.anon_groups()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let content = crate::services::gateway::builders::build_edit_content(
                    target_sent_timestamp, new_text,
                );
                self.send_dm_content(content, peer_id).await?;
                // Persist edit locally
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp, new_text, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp, new_text, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send a delete-for-everyone and persist locally.
    pub async fn send_delete(&self, target_sent_timestamp: u64) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                // Persist locally
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.anon_dms().delete_message_by_timestamp(id, &user_id, target_sent_timestamp);
                }
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                // Persist locally
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.anon_groups().delete_message_by_timestamp(id, &user_id, target_sent_timestamp);
                }
                Ok(())
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let content = crate::services::build_delete_for_everyone_content(
                    target_sent_timestamp,
                );
                self.send_dm_content(content, peer_id).await?;
                // Persist deletion locally (sender doesn't get their own delete back)
                self.delete_local_by_timestamp(target_sent_timestamp);
                Ok(())
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let content = crate::services::build_delete_for_everyone_content_with_channel(
                    target_sent_timestamp, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                self.delete_local_by_timestamp(target_sent_timestamp);
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::build_delete_for_everyone_content_with_channel(
                    target_sent_timestamp, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await?;
                self.delete_local_by_timestamp(target_sent_timestamp);
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Delete-for-me: delete locally + sync to own devices.
    pub async fn delete_for_me(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                    .map(|_| ())
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                    .map(|_| ())
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &peer_id.to_string(), &author_aci, target_sent_timestamp,
                );
                self.send_dm_content(content, peer_id).await?;
                self.client.messages().delete_message_local(&msg.id)
                    .map_err(ConversationError::Message)
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &channel_id.to_string(), &author_aci, target_sent_timestamp,
                );
                self.send_channel_content(content, channel_id).await?;
                self.client.messages().delete_message_local(&msg.id)
                    .map_err(ConversationError::Message)
            }
            ActiveConversation::Channel(channel_id) => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &channel_id.to_string(), &author_aci, target_sent_timestamp,
                );
                self.send_channel_content(content, channel_id).await?;
                self.client.messages().delete_message_local(&msg.id)
                    .map_err(ConversationError::Message)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Get message text by sender + timestamp across any conversation type.
    pub fn get_message_text(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Option<String>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.as_text()),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.as_text()),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::MailboxDm { .. } | ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                Ok(msg.as_text().map(|s| s.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Get attachment IDs for a message by sender + timestamp across any conversation type.
    pub fn get_message_attachment_ids(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Vec<crate::domain::AttachmentId>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.attachment_ids()),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.attachment_ids()),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::MailboxDm { .. } | ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                Ok(msg.attachments)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Check if a message is owned by the current user.
    pub fn is_own_message(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        let current_user_id = self.client.auth_state().get().current_user_id;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(Some(msg.sender_id) == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(Some(msg.sender_id) == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::MailboxDm { .. } | ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(&target_author.to_string(), target_sent_timestamp)?;
                Ok(msg.sender_id == current_user_id)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }


    // ========================================================================
    // Receipts
    // ========================================================================

    pub async fn send_read_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() { return Ok(()); }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => {
                self.client.anon_dms()
                    .send_read_receipt(dm_id, timestamps.to_vec())
                    .await
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            // Groups don't track per-user read state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let content = crate::services::gateway::build_read_receipt_content(timestamps.to_vec());
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let content = crate::services::gateway::build_read_receipt_content_with_channel(
                    timestamps.to_vec(), Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::gateway::build_read_receipt_content_with_channel(
                    timestamps.to_vec(), Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    pub async fn send_delivery_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() { return Ok(()); }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => {
                self.client.anon_dms()
                    .send_delivery_receipt(dm_id, timestamps.to_vec())
                    .await
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            // Groups don't track per-user delivery state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::MailboxDm { peer_id, .. } => {
                let content = crate::services::gateway::build_delivery_receipt_content(timestamps.to_vec());
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::ServerDm { channel_id, .. } => {
                let content = crate::services::gateway::build_delivery_receipt_content_with_channel(
                    timestamps.to_vec(), Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::gateway::build_delivery_receipt_content_with_channel(
                    timestamps.to_vec(), Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Mark all messages in this conversation as read.
    ///
    /// For DM/Channel, loads recent messages from the peer and sends read receipts.
    /// For AnonDm, marks locally + sends anon read receipt.
    /// For AnonGroup, no-op (groups don't track read state per-user).
    pub async fn mark_as_read(&self) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                if let Err(e) = self.client.anon_dms().mark_as_read(id) {
                    tracing::warn!("Failed to mark anon DM as read: {}", e);
                }
                // Send read receipts for peer messages
                let current_user = self.client.auth_state().get().current_user_id;
                if let Ok(messages) = self.client.anon_dms().get_messages(id, None, 50) {
                    let timestamps: Vec<u64> = messages.iter()
                        .filter(|m| current_user.map(|uid| m.sender_id != uid).unwrap_or(true))
                        .map(|m| m.sent_at)
                        .collect();
                    if !timestamps.is_empty() {
                        let _ = self.client.anon_dms().send_read_receipt(id, timestamps).await;
                    }
                }
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                if let Err(e) = self.client.anon_groups().mark_as_read(id) {
                    tracing::warn!("Failed to mark anon group as read: {}", e);
                }
                Ok(())
            }
            ActiveConversation::MailboxDm { channel_id, .. }
            | ActiveConversation::ServerDm { channel_id, .. } => {
                let my_user_id = self.client.auth_state().get().current_user_id;
                if let Ok(messages) = self.client.messages().get_messages_local(channel_id, None, 50) {
                    let timestamps: Vec<u64> = messages.iter()
                        .filter(|m| m.sender_id.as_ref() != my_user_id.as_ref())
                        .map(|m| m.sent_at)
                        .collect();
                    self.send_read_receipt(&timestamps).await?;
                }
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let my_user_id = self.client.auth_state().get().current_user_id.map(|id| id.to_string());
                if let Ok(messages) = self.client.messages().get_messages_local(channel_id, None, 50) {
                    let timestamps: Vec<u64> = messages.iter()
                        .filter(|m| m.sender_id.as_ref().map(|id| id.to_string()).as_ref() != my_user_id.as_ref())
                        .map(|m| m.sent_at)
                        .collect();
                    self.send_read_receipt(&timestamps).await?;
                }
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Look up a message by sender + timestamp (for standard DM/Channel paths).
    /// Persist an edit to local DB (for sender's own edits).
    fn persist_edit(&self, sent_timestamp: u64, new_text: &str) {
        if let Some(user_id) = self.client.auth_state().get().current_user_id {
            if let Ok(Some(msg)) = self.client.messages().get_by_sender_timestamp(&user_id, sent_timestamp) {
                let new_content = crate::domain::message::MessageContent::Text(new_text.to_string());
                let _ = self.client.messages().update_message_body(&msg.id, &new_content);
            }
        }
    }

    /// Delete a message from local DB by timestamp (for sender's own deletes).
    fn delete_local_by_timestamp(&self, sent_timestamp: u64) {
        if let Some(user_id) = self.client.auth_state().get().current_user_id {
            if let Ok(Some(msg)) = self.client.messages().get_by_sender_timestamp(&user_id, sent_timestamp) {
                let _ = self.client.messages().delete_message_local(&msg.id);
            }
        }
    }

    fn lookup_message(
        &self,
        sender_id_str: &str,
        sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        let sender_id: UserId = sender_id_str.parse()
            .map_err(|_| ConversationError::Other("Invalid sender ID".into()))?;
        self.client.messages()
            .get_by_sender_timestamp(&sender_id, sent_timestamp)
            .map_err(|e| ConversationError::Message(e))?
            .ok_or_else(|| ConversationError::Other("Message not found".into()))
    }

    /// Send content via DM (Signal Protocol + mailbox).
    async fn send_dm_content(
        &self,
        content: crate::proto::content_v1::Content,
        peer_id: &UserId,
    ) -> ConversationResult {
        self.client.messages()
            .send_dm(peer_id, content, &**self.client.mailbox_manager())
            .await
            .map_err(ConversationError::Message)
    }

    /// Send control content via channel (sender keys).
    async fn send_channel_content(
        &self,
        content: crate::proto::content_v1::Content,
        channel_id: &ChannelId,
    ) -> ConversationResult {
        self.client.messages()
            .send_control(channel_id, content, None)
            .await
            .map(|_| ())
            .map_err(ConversationError::Message)
    }
}
