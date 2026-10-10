use crate::domain::UserId;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::proto::content_v1::{
    self, AnonDmMessage as ProtoAnonDmMessage, AnonDmPresence, AnonDmProfileUpdate, Content,
    anon_dm_presence,
};
use crate::services::gateway::builders::{
    build_data_message_content_with_quote, build_delivery_receipt_content, build_edit_content,
    build_pin_content, build_read_receipt_content, build_typing_content,
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
    ) -> Result<(MessageId, u64), AnonDmError> {
        self.send_with_attachments(conversation_id, text, vec![])
            .await
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
    ) -> Result<(MessageId, u64), AnonDmError> {
        self.send_with_attachments_and_quote(conversation_id, text, attachments, None)
            .await
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
    ) -> Result<(MessageId, u64), AnonDmError> {
        // Look up the original message to build preview text + thumbnail.
        // Falls back to "📷 Photo" / "🎬 Video" / etc. for media-only replies
        // so the receiver sees a non-empty reply bubble on reload.
        let original = self
            .get_message_by_sender_timestamp(conversation_id, reply_to_sender, reply_to_timestamp)
            .ok()
            .flatten();
        let (preview_text, quote_attachments) = original
            .as_ref()
            .map(|msg| {
                let attachments: Vec<_> = msg
                    .attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect();
                let preview =
                    crate::services::gateway::shared::message_reply_preview(msg, &attachments);
                let qa = crate::services::gateway::shared::build_quoted_attachments(&attachments);
                (preview, qa)
            })
            .unwrap_or_default();

        let quote = crate::services::gateway::shared::build_quote_with_attachments(
            &reply_to_sender.to_string(),
            reply_to_timestamp,
            &preview_text,
            quote_attachments,
        );

        self.send_with_attachments_and_quote(conversation_id, text, attachments, Some(quote))
            .await
    }

    /// Internal: Send a message with optional attachments and optional quote.
    async fn send_with_attachments_and_quote(
        &self,
        conversation_id: &AnonDmId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
        quote: Option<crate::proto::content_v1::Quote>,
    ) -> Result<(MessageId, u64), AnonDmError> {
        use crate::services::gateway::shared;

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
        let attachment_ids = shared::extract_attachment_ids(&attachments);

        // Create DataMessage content with attachments and optional quote
        let sent_at = TimestampNanos::now().as_u64();
        let inner_content =
            build_data_message_content_with_quote(text, attachments, quote.clone(), sent_at);

        // Store message locally
        let channel_id: ChannelId = (*conversation_id).into();
        let message_id = MessageId::new();
        let created_at = TimestampNanos::from_nanos(sent_at)
            .to_datetime()
            .unwrap_or_else(chrono::Utc::now);

        let content = if attachment_ids.is_empty() {
            MessageContent::Text(text.to_string())
        } else {
            MessageContent::Mixed(crate::domain::message::MixedContent::new(
                text.to_string(),
                attachment_ids.clone(),
            ))
        };

        // Extract reply context from quote so reply bubbles survive reload
        let reply_to = quote.as_ref().map(|q| MessageId::from_timestamp(q.id));
        let reply_to_author_id = quote.as_ref().and_then(|q| {
            if q.author_aci.is_empty() {
                None
            } else {
                q.author_aci.parse().ok()
            }
        });
        let reply_to_text = quote
            .as_ref()
            .map(|q| q.text.clone())
            .filter(|t| !t.is_empty());
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = quote
            .as_ref()
            .and_then(|q| q.attachments.first())
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

        let message = Message {
            id: message_id,
            conversation_id: channel_id,
            conversation_type: ConversationType::AnonDm,
            sender_id: Some(current_user),
            device_id: None,
            content,
            encrypted_content: None,
            thread_id: None,
            reply_to,
            reply_to_author_id,
            reply_to_text,
            reply_to_thumbnail,
            reply_to_thumbnail_mime,
            attachments: attachment_ids.clone(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at,
            created_at,
            updated_at: created_at,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: "sending".into(),
            failure_reason: None,
        };
        self.message_repo.insert_outgoing(&message)?;
        crate::services::gateway::submission::bind_original_row(message_id.to_string())?;
        tracing::info!(
            conversation_id = %conversation_id,
            message_id = %message_id,
            sent_at,
            text_len = text.len(),
            "[ANON_DM_SEND] inserted own outgoing message into local DB"
        );

        // NOTE: Do NOT emit AnonDmMessageReceived here.
        // The UI send handler (send_message.rs) already appends the message to
        // the Slint model via append_message_to_ui(). Emitting the domain event
        // here caused a duplicate bubble because the domain_handler/anon_dm.rs
        // would also push the same message to the UI model.

        // NDSS '21 decorrelation: if this message carries attachment pointers,
        // delay the anonymous mailbox send so the server can't trivially
        // correlate the authenticated CDN upload with this anonymous write.
        // The message is already visible in the UI (optimistic insert above).
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        // Send to peer
        self.send_message(conversation_id, inner_content).await?;

        // Mark as "sent" for delivery status indicator
        let channel_id_for_status: ChannelId = (*conversation_id).into();
        let _ = self.message_repo.update_delivery_status_by_timestamps(
            &channel_id_for_status,
            &ConversationType::AnonDm,
            &[sent_at],
            "sent",
        );

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
        presence: crate::domain::user::PresenceStatus,
    ) -> Result<(), AnonDmError> {
        use crate::domain::user::PresenceStatus;
        // Silently ignore if blocked
        if self
            .check_conversation_not_blocked(conversation_id)
            .is_err()
        {
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

        // Map domain PresenceStatus to wire-level anon_dm_presence::Status.
        let status = match presence {
            PresenceStatus::Online => anon_dm_presence::Status::Online,
            PresenceStatus::Away => anon_dm_presence::Status::Away,
            PresenceStatus::DoNotDisturb => anon_dm_presence::Status::DoNotDisturb,
            PresenceStatus::Offline => anon_dm_presence::Status::Offline,
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

        match self
            .send_to_anon_mailbox(&peer_id, &peer_mailbox_pk, content)
            .await
        {
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
        if self
            .check_conversation_not_blocked(conversation_id)
            .is_err()
        {
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

        // Name and avatar travel only under our profile key, which
        // `get_my_profile_key` withholds from anon peers unless our visibility
        // is "everyone": a stranger never reads our real name otherwise.
        let profile_key = self.get_my_profile_key();
        let (encrypted_display_name, encrypted_avatar, pk_bytes) = if let Some(pk) = &profile_key {
            let enc_name = display_name.as_deref().and_then(|name| {
                crate::crypto::profile_crypto::encrypt_display_name(pk, name).ok()
            });
            let enc_avatar = self.get_encrypted_avatar(pk);
            (enc_name, enc_avatar, Some(pk.to_vec()))
        } else {
            (None, None, None)
        };

        // Build profile update message
        let profile_update = AnonDmProfileUpdate {
            conversation_id: conversation_id.as_bytes().to_vec(),
            encrypted_display_name,
            avatar_url,
            bio,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
            encrypted_avatar,
            profile_key: pk_bytes,
        };

        let content = Content {
            ephemeral_reply_to: None,
            timestamp: profile_update.timestamp,
            padding: Self::generate_padding(512), // Larger padding for profile updates
            content: Some(content_v1::content::Content::AnonDmProfileUpdate(
                profile_update,
            )),
        };

        let peer_id = dm.peer_id;

        // Send via mailbox
        let message = self
            .send_to_anon_mailbox(&peer_id, &peer_mailbox_pk, content)
            .await;

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
        if self
            .check_conversation_not_blocked(conversation_id)
            .is_err()
        {
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
        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content)
            .await
    }

    /// Send a reaction to a message.
    ///
    /// Reactions are ephemeral; blocked, missing and unready targets fail explicitly.
    pub async fn send_reaction(
        &self,
        conversation_id: &AnonDmId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> Result<(), AnonDmError> {
        let actor = self.current_user_id()?;
        let action_ts = self.message_repo.allocate_reaction_clock(&actor)?;
        self.send_reaction_at(
            conversation_id,
            emoji,
            target_author_aci,
            target_sent_timestamp,
            remove,
            action_ts,
        )
        .await
    }

    /// Send an already allocated action without restamping it.
    pub async fn send_reaction_at(
        &self,
        conversation_id: &AnonDmId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
        action_ts: u64,
    ) -> Result<(), AnonDmError> {
        dispatch_reaction_at(
            conversation_id,
            || {
                self.check_conversation_not_blocked(conversation_id)?;
                self.anon_dm_repo
                    .get_conversation(conversation_id)
                    .map_err(AnonDmError::from)
            },
            emoji,
            target_author_aci,
            target_sent_timestamp,
            remove,
            action_ts,
            |dm, content| async move {
                // Readiness was checked by the reaction-only adapter.
                let mailbox = dm
                    .peer_mailbox_pk
                    .ok_or(AnonDmError::ConversationNotReady(*conversation_id))?;
                self.send_ephemeral_message(&dm.peer_id, &mailbox, conversation_id, content)
                    .await
            },
        )
        .await
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

        let dm_message = ProtoAnonDmMessage {
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

        match self
            .send_to_anon_mailbox(peer_id, peer_mailbox_pk, content)
            .await
        {
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
        if self
            .check_conversation_not_blocked(conversation_id)
            .is_err()
        {
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

        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content)
            .await
    }

    /// Send a delivery receipt to the peer.
    pub async fn send_delivery_receipt(
        &self,
        conversation_id: &AnonDmId,
        message_timestamps: Vec<u64>,
    ) -> Result<(), AnonDmError> {
        if message_timestamps.is_empty() {
            return Ok(());
        }

        if self
            .check_conversation_not_blocked(conversation_id)
            .is_err()
        {
            return Ok(());
        }

        let dm = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.is_ready() => dm,
            _ => return Ok(()),
        };

        let peer_mailbox_pk = dm.peer_mailbox_pk.unwrap();
        let content = build_delivery_receipt_content(message_timestamps);

        self.send_ephemeral_message(&dm.peer_id, &peer_mailbox_pk, conversation_id, content)
            .await
    }

    /// Send a delete-for-everyone message.
    ///
    /// This sends a delete notification to the peer so they also delete the message.
    pub async fn send_delete_for_everyone(
        &self,
        conversation_id: &AnonDmId,
        target_sent_timestamp: u64,
    ) -> Result<(), AnonDmError> {
        let author = self.current_user_id()?;
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            target_sent_timestamp,
            Some(&author.to_string()),
            None,
        );
        self.send_message(conversation_id, content).await
    }

    pub async fn send_targeted_delete_for_everyone(
        &self,
        conversation_id: &AnonDmId,
        author: &UserId,
        timestamp: u64,
    ) -> Result<(), AnonDmError> {
        let target = self.get_message_by_sender_timestamp(conversation_id, author, timestamp)?;
        let content = match target.as_ref() {
            Some(record) => {
                crate::services::gateway::builders::build_stored_dm_delete_content(record)?
            }
            None => crate::services::gateway::builders::build_targeted_delete_content(
                timestamp,
                Some(&author.to_string()),
                None,
            ),
        };
        self.send_message(conversation_id, content).await?;
        Ok(())
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
        let channel_id: ChannelId = (*conversation_id).into();
        self.message_repo.update_text_by_timestamp(
            &channel_id,
            &ConversationType::AnonDm,
            target_sent_timestamp,
            new_text,
        )?;

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
        let dm_message = ProtoAnonDmMessage {
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

/// Actual reaction adapter; only the mailbox effect is injected in regressions.
async fn dispatch_reaction_at<G, S, F>(
    id: &AnonDmId,
    gate: G,
    emoji: &str,
    author: &str,
    target: u64,
    remove: bool,
    clock: u64,
    send: S,
) -> Result<(), AnonDmError>
where
    G: FnOnce() -> Result<Option<crate::domain::anon_dm::AnonDm>, AnonDmError>,
    S: FnOnce(crate::domain::anon_dm::AnonDm, Content) -> F,
    F: std::future::Future<Output = Result<(), AnonDmError>>,
{
    let dm = gate()?.ok_or(AnonDmError::ConversationNotFound(*id))?;
    if !dm.is_ready() {
        return Err(AnonDmError::ConversationNotReady(*id));
    }
    let content = crate::services::gateway::builders::build_reaction_content_with_channel_at(
        emoji, remove, author, target, None, clock,
    );
    send(dm, content).await
}

#[cfg(test)]
#[path = "../anonymous_reaction_test_fixture.rs"]
mod reaction_test_fixture;

#[cfg(test)]
mod reaction_outcome_tests {
    use super::*;
    use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
    use crate::services::conversation::ConversationError;
    use reaction_test_fixture::{Fixture, assert_encoded_action};
    use std::cell::Cell;

    #[tokio::test]
    async fn actual_dm_reaction_adapter_gates_add_remove_and_retains_clock() {
        for remove in [false, true] {
            for mode in [
                "blocked",
                "missing",
                "unready",
                "transport_failed",
                "delivered",
            ] {
                let fixture = Fixture::new(ConversationType::AnonDm, remove);
                let mut dm = AnonDm::new_initiated(UserId::new());
                let id = dm.id;
                if mode != "unready" {
                    dm.status = AnonDmStatus::Active;
                    dm.peer_mailbox_pk = Some([7; 32]);
                }
                let attempts = Cell::new(0);
                let author = fixture.author.to_string();
                let dispatch = dispatch_reaction_at(
                    &id,
                    || match mode {
                        "blocked" => Err(AnonDmError::ConversationBlocked(id)),
                        "missing" => Ok(None),
                        _ => Ok(Some(dm)),
                    },
                    "A",
                    &author,
                    100,
                    remove,
                    20,
                    |_, content| {
                        attempts.set(attempts.get() + 1);
                        assert_encoded_action(&content, remove, 20, &fixture.author);
                        async move {
                            tokio::task::yield_now().await;
                            if mode == "transport_failed" {
                                Err(AnonDmError::MessageError("injected mailbox failure".into()))
                            } else {
                                Ok(())
                            }
                        }
                    },
                );
                fixture
                    .finish(
                        async { dispatch.await.map_err(ConversationError::from) },
                        remove,
                        20,
                        mode == "delivered",
                    )
                    .await;
                assert_eq!(
                    attempts.get(),
                    usize::from(matches!(mode, "transport_failed" | "delivered"))
                );
            }
        }
    }
}
