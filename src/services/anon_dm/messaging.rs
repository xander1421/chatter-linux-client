use crate::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use crate::domain::UserId;
use crate::proto::content_v1::{self, AnonDmMessage, AnonDmPresence, AnonDmProfileUpdate, Content, anon_dm_presence};
use crate::services::gateway::builders::{
    build_typing_content, build_reaction_content, build_read_receipt_content,
    build_delete_for_everyone_content, build_pin_content, build_edit_content,
};
use crate::util::time::TimestampNanos;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Send a text message in an active conversation.
    ///
    /// Returns the message ID and the sent_at timestamp (nanoseconds).
    pub async fn send_text(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
    ) -> Result<(AnonDmMessageId, u64), AnonDmError> {
        self.send_with_attachments(conversation_id, text, vec![]).await
    }

    /// Send a message with optional attachments in an active conversation.
    ///
    /// Attachments should be pre-uploaded to CDN with encryption keys included.
    ///
    /// Returns the message ID and the sent_at timestamp (nanoseconds).
    /// The sent_at timestamp should be used for UI display and receipt matching.
    pub async fn send_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
    ) -> Result<(AnonDmMessageId, u64), AnonDmError> {
        self.send_with_attachments_and_quote(conversation_id, text, attachments, None).await
    }

    /// Send a reply message with optional attachments in an active conversation.
    ///
    /// The quote contains the reply target info (author_id, timestamp, preview text).
    pub async fn send_reply(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
        reply_to_sender: &UserId,
        reply_to_timestamp: u64,
    ) -> Result<(AnonDmMessageId, u64), AnonDmError> {
        // Look up the original message to get preview text
        let preview_text = self.get_message_by_sender_timestamp(
            conversation_id,
            reply_to_sender,
            reply_to_timestamp,
        )
        .ok()
        .flatten()
        .map(|msg| {
            let text = msg.as_text().unwrap_or_default();
            if text.chars().count() > 100 {
                format!("{}...", text.chars().take(97).collect::<String>())
            } else {
                text
            }
        })
        .unwrap_or_default();

        let quote = crate::proto::content_v1::Quote {
            id: reply_to_timestamp,
            author_aci: reply_to_sender.to_string(),
            text: preview_text,
            attachments: vec![],
            r#type: 0, // NORMAL
            author_aci_binary: vec![],
        };

        self.send_with_attachments_and_quote(conversation_id, text, attachments, Some(quote)).await
    }

    /// Internal: Send a message with optional attachments and optional quote.
    async fn send_with_attachments_and_quote(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
        quote: Option<crate::proto::content_v1::Quote>,
    ) -> Result<(AnonDmMessageId, u64), AnonDmError> {
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

        // Create DataMessage content with attachments and optional quote
        let sent_at = TimestampNanos::now().as_u64();
        let data_message = DataMessage {
            body: text.to_string(),
            attachments,
            quote,
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

        Ok((message_id, sent_at))
    }

    /// Send a presence update to the peer.
    ///
    /// Presence updates are ephemeral - they're silently dropped if:
    /// - Conversation is blocked
    /// - Conversation is not ready (bootstrap not complete)
    pub async fn send_presence_update(
        &self,
        conversation_id: &AnonDmId,
        presence: crate::proto::UserPresence,
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
                    "Dropping presence update - conversation not ready"
                );
                return Ok(());
            }
        };

        // Map UserPresence to anon_dm_presence::Status
        let status = match presence {
            crate::proto::UserPresence::Online => anon_dm_presence::Status::Online,
            crate::proto::UserPresence::Away => anon_dm_presence::Status::Away,
            crate::proto::UserPresence::DoNotDisturb => anon_dm_presence::Status::DoNotDisturb,
            crate::proto::UserPresence::Offline => anon_dm_presence::Status::Offline,
            _ => anon_dm_presence::Status::Offline,
        };

        // Create AnonDmPresence message
        let presence_msg = AnonDmPresence {
            conversation_id: conversation_id.to_vec(),
            status: status as i32,
            timestamp: TimestampNanos::now().as_u64(),
        };

        // Send via mailbox
        let content = Content {
            content: Some(content_v1::content::Content::AnonDmPresence(presence_msg)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        let peer_id = dm.peer_id;
        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        
        match self.send_to_anon_mailbox(&peer_id, &peer_mailbox_pk, content).await {
            Ok(()) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    status = ?status,
                    "[ANON_DM_PRESENCE] Sent presence update"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "[ANON_DM_PRESENCE] Failed to send presence update (ephemeral, ignoring)"
                );
                Ok(()) // Still return Ok for ephemeral messages
            }
        }
    }

    /// Send a profile update to the peer.
    ///
    /// Profile updates are ephemeral - silently dropped if conversation not ready.
    pub async fn send_profile_update(
        &self,
        conversation_id: &AnonDmId,
        display_name: Option<String>,
        avatar_url: Option<String>,
        bio: Option<String>,
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
                    "Dropping profile update - conversation not ready"
                );
                return Ok(());
            }
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        
        // Build profile update message
        let profile_update = AnonDmProfileUpdate {
            conversation_id: conversation_id.as_bytes().to_vec(),
            display_name,
            avatar_url,
            bio,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        };

        let content = Content {
            ephemeral_reply_to: None,
            timestamp: profile_update.timestamp,
            padding: Self::generate_padding(512), // Larger padding for profile updates
            content: Some(content_v1::content::Content::AnonDmProfileUpdate(profile_update)),
        };

        let peer_id = dm.peer_id;
        
        // Send via mailbox
        let message = self.send_to_anon_mailbox(&peer_id, &peer_mailbox_pk, content).await;

        match message {
            Ok(()) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    "[ANON_DM_PROFILE] Sent profile update"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "[ANON_DM_PROFILE] Failed to send profile update (ephemeral, ignoring)"
                );
                Ok(()) // Still return Ok for ephemeral messages
            }
        }
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
        // Log what type of ephemeral message we're sending
        let content_type = match &inner_content.content {
            Some(content_v1::content::Content::Receipt(_)) => "Receipt",
            Some(content_v1::content::Content::Typing(_)) => "Typing",
            Some(content_v1::content::Content::Reaction(_)) => "Reaction",
            _ => "Unknown",
        };

        tracing::info!(
            conversation_id = %conversation_id,
            peer_id = %peer_id,
            peer_mailbox_pk = hex::encode(&peer_mailbox_pk[..8]),
            content_type = content_type,
            "[ANON_DM_EPHEMERAL] Sending ephemeral message"
        );

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

        match self.send_to_anon_mailbox(peer_id, peer_mailbox_pk, content).await {
            Ok(()) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    peer_mailbox_pk = hex::encode(&peer_mailbox_pk[..8]),
                    content_type = content_type,
                    "[ANON_DM_EPHEMERAL] Successfully sent ephemeral message"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    conversation_id = %conversation_id,
                    peer_mailbox_pk = hex::encode(&peer_mailbox_pk[..8]),
                    content_type = content_type,
                    error = %e,
                    "[ANON_DM_EPHEMERAL] Failed to send ephemeral message"
                );
                Err(e)
            }
        }
    }

    /// Send a read receipt for messages.
    ///
    /// Read receipts are ephemeral - silently dropped if conversation not ready.
    pub async fn send_read_receipt(
        &self,
        conversation_id: &AnonDmId,
        message_timestamps: Vec<u64>,
    ) -> Result<(), AnonDmError> {
        tracing::info!(
            conversation_id = %conversation_id,
            timestamp_count = message_timestamps.len(),
            timestamps = ?message_timestamps,
            "[ANON_DM_SEND_RECEIPT] Attempting to send read receipt"
        );

        if message_timestamps.is_empty() {
            tracing::debug!("[ANON_DM_SEND_RECEIPT] No timestamps - skipping");
            return Ok(());
        }

        // Silently ignore if blocked
        if self.check_conversation_not_blocked(conversation_id).is_err() {
            tracing::debug!(
                conversation_id = %conversation_id,
                "[ANON_DM_SEND_RECEIPT] Conversation blocked - skipping"
            );
            return Ok(());
        }

        // Get conversation - silently ignore if not found or not ready
        let dm = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.is_ready() => dm,
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "[ANON_DM_SEND_RECEIPT] Dropping read receipt - conversation not ready"
                );
                return Ok(());
            }
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        let content = build_read_receipt_content(message_timestamps.clone());

        tracing::info!(
            conversation_id = %conversation_id,
            peer_id = %dm.peer_id,
            timestamp_count = message_timestamps.len(),
            "[ANON_DM_SEND_RECEIPT] Sending read receipt to peer"
        );

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
