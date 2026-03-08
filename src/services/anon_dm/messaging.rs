use crate::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use crate::domain::UserId;
use crate::proto::content_v1::{self, AnonDmMessage, Content};
use crate::services::gateway::builders::{
    build_typing_content, build_reaction_content, build_read_receipt_content,
    build_delete_for_everyone_content, build_pin_content, build_edit_content,
};
use crate::util::time::TimestampNanos;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Send a text message in an active conversation.
    pub async fn send_text(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        self.send_with_attachments(conversation_id, text, vec![]).await
    }

    /// Send a message with optional attachments in an active conversation.
    ///
    /// Attachments should be pre-uploaded to CDN with encryption keys included.
    pub async fn send_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        use crate::domain::AttachmentId;
        use crate::proto::content_v1::DataMessage;

        let current_user = self.current_user_id()?;

        // Get conversation and verify it's active
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if !dm.is_ready() {
            return Err(AnonDmError::ConversationNotReady(*conversation_id));
        }

        // Extract attachment IDs from pointers before moving them
        let attachment_ids: Vec<AttachmentId> = attachments
            .iter()
            .filter_map(|ptr| {
                if ptr.cdn_key.is_empty() {
                    None
                } else {
                    Some(AttachmentId::from_cdn_key(&ptr.cdn_key))
                }
            })
            .collect();

        // Create DataMessage content with attachments
        let sent_at = TimestampNanos::now().as_u64();
        let data_message = DataMessage {
            body: text.to_string(),
            attachments,
            ..Default::default()
        };
        let inner_content = Content {
            content: Some(content_v1::content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: sent_at,
            padding: Vec::new(),
        };

        // Store message locally with attachment IDs
        let message_id = self.store_message_with_attachments(
            conversation_id,
            &current_user,
            text,
            &attachment_ids,
            sent_at,
        )?;

        // Send to peer
        self.send_message(conversation_id, inner_content).await?;

        Ok(message_id)
    }

    /// Send a typing indicator to the peer.
    ///
    /// Typing indicators are ephemeral - they're silently dropped if:
    /// - Conversation is blocked
    /// - Conversation is not ready (bootstrap not complete)
    pub async fn send_typing_indicator(
        &self,
        conversation_id: &AnonDmId,
        started: bool,
    ) -> Result<(), AnonDmError> {
        // Silently ignore if blocked
        if self.check_conversation_not_blocked(conversation_id).is_err() {
            return Ok(());
        }

        // Get conversation - silently ignore if not found or not ready
        let dm = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.is_ready() => dm,
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "Dropping typing indicator - conversation not ready"
                );
                return Ok(());
            }
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        let content = build_typing_content(started);
        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content).await
    }

    /// Send a reaction to a message.
    ///
    /// Reactions are ephemeral - they're silently dropped if conversation not ready.
    pub async fn send_reaction(
        &self,
        conversation_id: &AnonDmId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> Result<(), AnonDmError> {
        // Silently ignore if blocked
        if self.check_conversation_not_blocked(conversation_id).is_err() {
            return Ok(());
        }

        // Get conversation - silently ignore if not found or not ready
        let dm = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.is_ready() => dm,
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "Dropping reaction - conversation not ready"
                );
                return Ok(());
            }
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        let content = build_reaction_content(emoji, remove, target_author_aci, target_sent_timestamp);
        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content).await
    }

    /// Send an ephemeral message (typing, reaction, receipt) - no rate limiting, no queueing.
    pub(crate) async fn send_ephemeral_message(
        &self,
        peer_id: &UserId,
        peer_mailbox_pk: &[u8; 32],
        conversation_id: &AnonDmId,
        inner_content: Content,
    ) -> Result<(), AnonDmError> {
        let dm_message = AnonDmMessage {
            conversation_id: conversation_id.to_vec(),
            inner_content: Some(Box::new(inner_content)),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
                dm_message,
            ))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        self.send_to_anon_mailbox(peer_id, peer_mailbox_pk, content).await
    }

    /// Send a read receipt for messages.
    ///
    /// Read receipts are ephemeral - silently dropped if conversation not ready.
    pub async fn send_read_receipt(
        &self,
        conversation_id: &AnonDmId,
        message_timestamps: Vec<u64>,
    ) -> Result<(), AnonDmError> {
        if message_timestamps.is_empty() {
            return Ok(());
        }

        // Silently ignore if blocked
        if self.check_conversation_not_blocked(conversation_id).is_err() {
            return Ok(());
        }

        // Get conversation - silently ignore if not found or not ready
        let dm = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.is_ready() => dm,
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "Dropping read receipt - conversation not ready"
                );
                return Ok(());
            }
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        let content = build_read_receipt_content(message_timestamps);
        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content).await
    }

    /// Send a delete-for-everyone message.
    ///
    /// This sends a delete notification to the peer so they also delete the message.
    pub async fn send_delete_for_everyone(
        &self,
        conversation_id: &AnonDmId,
        target_sent_timestamp: u64,
    ) -> Result<(), AnonDmError> {
        let content = build_delete_for_everyone_content(target_sent_timestamp);
        self.send_message(conversation_id, content).await
    }

    /// Send a pin/unpin message.
    ///
    /// This sends a pin notification to the peer so they also pin/unpin the message.
    pub async fn send_pin(
        &self,
        conversation_id: &AnonDmId,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> Result<(), AnonDmError> {
        let content = build_pin_content(target_author_aci, target_sent_timestamp, unpin);
        self.send_message(conversation_id, content).await
    }

    /// Send an edit message to update a previously sent message.
    ///
    /// This sends an edit notification to the peer and updates the message locally.
    pub async fn send_edit(
        &self,
        conversation_id: &AnonDmId,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> Result<(), AnonDmError> {
        let content = build_edit_content(target_sent_timestamp, new_text);
        self.send_message(conversation_id, content).await?;

        // Update message locally
        self.anon_dm_repo.update_message_text(conversation_id, target_sent_timestamp, new_text)?;

        Ok(())
    }

    /// Send a message to the peer in an active conversation.
    ///
    /// # Errors
    ///
    /// - `ConversationBlocked` if the conversation is blocked
    /// - `RateLimitExceeded` if too many messages sent recently
    pub async fn send_message(
        &self,
        conversation_id: &AnonDmId,
        inner_content: Content,
    ) -> Result<(), AnonDmError> {
        // Check if conversation is blocked (cannot send to blocked conversations)
        self.check_conversation_not_blocked(conversation_id)?;

        // Check rate limit before sending
        self.check_rate_limit(conversation_id)?;

        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if !dm.is_ready() {
            return Err(AnonDmError::ConversationNotReady(*conversation_id));
        }

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap(); // is_ready() guarantees this

        // Wrap in AnonDmMessage
        let dm_message = AnonDmMessage {
            conversation_id: conversation_id.to_vec(),
            inner_content: Some(Box::new(inner_content)),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
                dm_message,
            ))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to peer's anonymous mailbox
        self.send_to_anon_mailbox(&dm.peer_id, &peer_mailbox_pk, content)
            .await?;

        tracing::info!(
            conversation_id = %conversation_id,
            peer_id = %dm.peer_id,
            peer_mailbox_pk = hex::encode(&peer_mailbox_pk[..8]),
            "[ANON_DM] Sent message to peer's mailbox"
        );

        Ok(())
    }
}
