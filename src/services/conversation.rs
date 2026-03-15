//! Polymorphic conversation operations.
//!
//! The `ConversationOps` trait provides a unified API for messaging operations
//! across all conversation types (Channel, DM, Anon DM, Anon Group).
//!
//! # Usage
//!
//! ```ignore
//! let ops = ConversationOps::from_active(conversation, ui_ctx);
//! ops.send_reaction("👍", &author, timestamp, false).await?;
//! ```
//!
//! This eliminates the 4-way match blocks in the chat UI layer.

use std::sync::Arc;

use crate::app::UIContext;
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
/// Wraps `ActiveConversation` + `UIContext` to provide a single API
/// for all conversation types. The correct service is dispatched
/// internally based on the conversation variant.
pub struct ConversationHandle {
    conversation: ActiveConversation,
    ui_ctx: Arc<UIContext>,
}

impl ConversationHandle {
    /// Create from the current active conversation.
    pub fn new(conversation: ActiveConversation, ui_ctx: Arc<UIContext>) -> Self {
        Self { conversation, ui_ctx }
    }

    /// Create from the current chat state.
    pub fn current(ui_ctx: &Arc<UIContext>) -> Self {
        Self {
            conversation: ui_ctx.chat_state.active_conversation(),
            ui_ctx: ui_ctx.clone(),
        }
    }

    /// Get the underlying conversation variant.
    pub fn conversation(&self) -> &ActiveConversation {
        &self.conversation
    }

    /// Get the UIContext reference.
    pub fn ui_ctx(&self) -> &Arc<UIContext> {
        &self.ui_ctx
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
                self.ui_ctx.anon_dm_service
                    .send_reaction(id, emoji, target_author, target_sent_timestamp, remove)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.ui_ctx.anon_group_service
                    .send_reaction(id, emoji, target_author, target_sent_timestamp, remove)
                    .await?;
                Ok(())
            }
            ActiveConversation::Dm { peer_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_reaction_content(
                    emoji, remove, &author_aci, target_sent_timestamp,
                );
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_reaction_content_with_channel(
                    emoji, remove, &author_aci, target_sent_timestamp,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send a typing indicator.
    pub async fn send_typing(&self, started: bool) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.ui_ctx.anon_dm_service.send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.ui_ctx.anon_group_service.send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::Dm { peer_id, .. } => {
                let content = crate::services::build_typing_content(started);
                self.send_dm_content(content, peer_id).await
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

    /// Send a pin/unpin.
    pub async fn send_pin(
        &self,
        target_author: &str,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.ui_ctx.anon_dm_service
                    .send_pin(id, target_author, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.ui_ctx.anon_group_service
                    .send_pin(id, target_author, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::Dm { peer_id, .. } => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content(
                    &author_aci, target_sent_timestamp, unpin,
                );
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci, target_sent_timestamp, unpin,
                    Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send an edit.
    pub async fn send_edit(
        &self,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.ui_ctx.anon_dm_service
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.ui_ctx.anon_group_service
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::Dm { peer_id, .. } => {
                let content = crate::services::gateway::builders::build_edit_content(
                    target_sent_timestamp, new_text,
                );
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp, new_text, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Send a delete-for-everyone.
    pub async fn send_delete(&self, target_sent_timestamp: u64) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.ui_ctx.anon_dm_service
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.ui_ctx.anon_group_service
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                Ok(())
            }
            ActiveConversation::Dm { peer_id, .. } => {
                let content = crate::services::build_delete_for_everyone_content(
                    target_sent_timestamp,
                );
                self.send_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                let content = crate::services::build_delete_for_everyone_content_with_channel(
                    target_sent_timestamp, Some(&channel_id.to_string()),
                );
                self.send_channel_content(content, channel_id).await
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Look up a message by sender + timestamp (for standard DM/Channel paths).
    fn lookup_message(
        &self,
        sender_id_str: &str,
        sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        let sender_id: UserId = sender_id_str.parse()
            .map_err(|_| ConversationError::Other("Invalid sender ID".into()))?;
        self.ui_ctx.message_service
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
        self.ui_ctx.message_service
            .send_dm(peer_id, content, &*self.ui_ctx.mailbox_manager)
            .await
            .map_err(ConversationError::Message)
    }

    /// Send control content via channel (sender keys).
    async fn send_channel_content(
        &self,
        content: crate::proto::content_v1::Content,
        channel_id: &ChannelId,
    ) -> ConversationResult {
        self.ui_ctx.message_service
            .send_control(channel_id, content, None)
            .await
            .map(|_| ())
            .map_err(|e| ConversationError::Other(format!("Channel send failed: {}", e)))
    }
}
