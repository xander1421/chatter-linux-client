//! DM (Direct Message) operations.
//!
//! Signal Protocol encryption for DM messages, including bootstrap,
//! session management, and pending message queue.

use prost::Message as ProstMessage;

use crate::crypto::encrypt_dm_bytes;
use crate::domain::ids::UserId;
use crate::mailbox::dm::DmConversationManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::services::mailbox_manager::MailboxManager;

use super::{MessageError, MessageService, SendDmContentParams};

impl MessageService {
    /// Send a Content proto to a DM peer via two-layer encryption.
    ///
    /// Uses two encryption layers for privacy:
    /// 1. Signal Protocol (forward secrecy via Double Ratchet)
    /// 2. Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
    ///
    /// If profile sharing is enabled for this friendship, includes our
    /// profile key in DataMessage so the recipient can decrypt our display_name.
    pub async fn send_dm_content(
        &self,
        params: SendDmContentParams<'_>,
    ) -> Result<(), MessageError> {
        let SendDmContentParams {
            auth_client,
            peer_id,
            content,
            mailbox_manager,
            grpc_pool,
            attachment_ids,
            channel_id,
        } = params;

        tracing::info!(
            peer_id = %peer_id,
            has_grpc_pool = grpc_pool.is_some(),
            "MessageService: send_dm_content called"
        );

        // Inject profile key if profile sharing is enabled for this peer
        let content = self.inject_profile_key_if_sharing(peer_id, content)?;

        // Load conversation to get peer's mailbox and shared secret
        tracing::debug!(peer_id = %peer_id, "MessageService: loading conversation state");
        let conversation_manager = DmConversationManager::new(self.database.clone());
        let conversation = conversation_manager.load_conversation(peer_id).map_err(|e| {
            tracing::error!(peer_id = %peer_id, error = %e, "MessageService: failed to load conversation");
            MessageError::DmError(format!("Failed to load conversation: {}", e))
        })?;

        tracing::debug!(
            peer_id = %peer_id,
            conversation_exists = conversation.is_some(),
            has_our_mailbox = conversation.as_ref().map(|c| c.our_mailbox.is_some()).unwrap_or(false),
            has_peer_mailbox = conversation.as_ref().map(|c| c.peer_mailbox.is_some()).unwrap_or(false),
            has_shared_secret = conversation.as_ref().map(|c| c.conversation_key.as_ref().map(|k| k.shared_secret.is_some()).unwrap_or(false)).unwrap_or(false),
            is_ready = conversation.as_ref().map(|c| c.is_ready_to_send()).unwrap_or(false),
            "MessageService: conversation state loaded"
        );

        // Check if conversation exists and is ready
        let needs_bootstrap = match &conversation {
            None => true,
            Some(conv) => !conv.is_ready_to_send(),
        };

        if needs_bootstrap {
            // DIAGNOSTIC: a ServerDm (regular friend) may have no ready mailbox
            // conversation, so any mailbox-transport send (incl. call signaling)
            // bootstraps instead of delivering. Surfaces the transport mismatch.
            tracing::info!(
                peer_id = %peer_id,
                conversation_exists = conversation.is_some(),
                "[DM_TRANSPORT] mailbox send needs bootstrap — content queued, not delivered now"
            );
            return self.handle_dm_bootstrap(
                auth_client,
                peer_id,
                content,
                grpc_pool,
                attachment_ids,
                channel_id,
                conversation.as_ref(),
            ).await;
        }

        // Conversation is ready - proceed with sending
        let conversation = conversation.unwrap();

        let peer_mailbox = conversation
            .peer_mailbox
            .as_ref()
            .ok_or_else(|| MessageError::DmError("No peer mailbox address".into()))?;

        // Per-direction key: outbound mailbox-layer encryption uses our
        // send_key (XChaCha20-Poly1305). Peer's matching recv_key is
        // derived deterministically from the same shared root.
        let shared_secret = conversation
            .conversation_key
            .as_ref()
            .and_then(|k| k.send_key())
            .ok_or_else(|| MessageError::DmError("No shared secret".into()))?;

        // Serialize Content proto
        let plaintext = content.encode_to_vec();

        // Layer 1: Signal Protocol encryption for ALL peer devices (forward secrecy)
        let peer_id_str = peer_id.to_string();

        // Shield: no prekey-bundle fetch per send. That fetch is an identified
        // RPC naming the recipient at the moment we message them — the link
        // the mailbox exists to hide. The sessions already exist (the
        // bootstrap made them).
        let peer_ciphertexts = if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            self.signal_service
                .encrypt_message_session_devices(auth_client, &peer_id_str, &plaintext)
                .await
        } else {
            self.signal_service
                .encrypt_message_all_devices(auth_client, &peer_id_str, &plaintext)
                .await
        }
        .map_err(|e| MessageError::from_signal_send(e, "Multi-device encrypt"))?;

        tracing::info!(
            peer_id = %peer_id,
            device_count = peer_ciphertexts.len(),
            "Encrypted DM for all peer devices"
        );

        // Layer 2: Mailbox encryption for each device (sender anonymity)
        // All ciphertexts go to same mailbox - server fans out to devices
        for (device_id, signal_ciphertext) in peer_ciphertexts {
            let mailbox_encrypted = encrypt_dm_bytes(&signal_ciphertext, &shared_secret)
                .map_err(|e| MessageError::EncryptionFailed(format!("Mailbox encrypt: {}", e)))?;

            self.send_to_mailbox_retried(
                mailbox_manager, &peer_mailbox.public_key, mailbox_encrypted, peer_id, device_id,
            ).await?;

            tracing::debug!(
                peer_id = %peer_id,
                device_id,
                "Sent DM to device via two-layer encryption"
            );
        }

        Ok(())
    }

    /// Send a Content proto to a DM peer (simplified API - no client params).
    pub async fn send_dm(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        mailbox_manager: &MailboxManager,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();

        self.send_dm_content(SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id,
            content,
            mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await
    }

    /// Send pending messages after bootstrap completes.
    ///
    /// Every queued message is attempted; if any could not be sent the whole
    /// flush is an error (the queue is already drained, so a swallowed failure
    /// was a message lost without a word — the caller turns this into
    /// `MessageSendFailed`). `Ok(n)` means all `n` went out.
    pub async fn send_pending_messages(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        mailbox_manager: &MailboxManager,
    ) -> Result<usize, MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            "PENDING_MESSAGES: Draining Sesame queue for peer"
        );

        // Drain pending messages from Sesame queue
        let pending = self
            .sesame_manager
            .drain_pending_messages(peer_id)
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "PENDING_MESSAGES: Failed to drain pending messages"
                );
                MessageError::Storage(e)
            })?;

        if pending.is_empty() {
            tracing::info!(
                peer_id = %peer_id,
                "PENDING_MESSAGES: Queue empty - no messages to send"
            );
            return Ok(0);
        }

        let pending_count = pending.len();
        tracing::info!(
            peer_id = %peer_id,
            count = pending_count,
            "PENDING_MESSAGES: Found {} message(s) to send",
            pending_count
        );

        let mut sent_count = 0;
        let mut last_error = None;

        for msg in pending {
            // Deserialize the content
            let content = match crate::proto::content_v1::Content::decode(msg.content.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to deserialize pending message content"
                    );
                    last_error = Some(format!("undecodable queued message: {e}"));
                    continue;
                }
            };

            let original_sent_at = content.timestamp;

            tracing::debug!(
                peer_id = %peer_id,
                msg_index = sent_count + 1,
                timestamp = content.timestamp,
                has_channel_id = msg.channel_id.is_some(),
                attachment_count = msg.attachment_ids.len(),
                "PENDING_MESSAGES: Sending message via send_dm_content"
            );

            match self
                .send_dm_content(SendDmContentParams {
                    auth_client,
                    peer_id,
                    content,
                    mailbox_manager,
                    grpc_pool: None,
                    attachment_ids: Some(msg.attachment_ids.clone()),
                    channel_id: msg.channel_id,
                })
                .await
            {
                Ok(()) => {
                    sent_count += 1;
                    tracing::info!(
                        peer_id = %peer_id,
                        sent = sent_count,
                        timestamp = original_sent_at,
                        channel_id = ?msg.channel_id,
                        "PENDING_MESSAGES: Successfully sent message #{}", sent_count
                    );
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "PENDING_MESSAGES: FAILED to send message"
                    );
                    last_error = Some(e.to_string());
                }
            }
        }

        tracing::info!(
            peer_id = %peer_id,
            sent = sent_count,
            total = pending_count,
            "PENDING_MESSAGES: Complete - sent {}/{} messages",
            sent_count,
            pending_count
        );

        match last_error {
            None => Ok(sent_count),
            Some(e) => Err(MessageError::DmError(format!(
                "{} of {} queued message(s) failed to send: {e}",
                pending_count - sent_count,
                pending_count
            ))),
        }
    }

    /// Reset session with a peer and attempt to re-establish.
    pub async fn reset_and_establish_session(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        device_id: u32,
    ) -> Result<(), MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            device_id,
            "Resetting session with peer"
        );

        // Archive all sessions in Sesame
        if let Err(e) = self.sesame_manager.archive_all_sessions(peer_id, device_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to archive Sesame sessions (continuing anyway)"
            );
        }

        // Mark peer as stale
        if let Err(e) = self.sesame_manager.mark_user_stale(peer_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to mark peer as stale (continuing anyway)"
            );
        }

        // Establish a fresh session by fetching their prekey bundle
        self.signal_service
            .ensure_session(auth_client, &peer_id.to_string(), device_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to establish new session after reset"
                );
                MessageError::from_signal_send(e, "Session establishment failed")
            })?;

        tracing::info!(
            peer_id = %peer_id,
            device_id,
            "Successfully reset and established new session"
        );

        Ok(())
    }

    /// Reset sessions with ALL devices of a peer and re-establish.
    ///
    /// Used for convergence failure recovery in multi-device scenarios.
    /// Archives existing sessions and establishes fresh ones for all known devices.
    pub async fn reset_and_establish_sessions_all_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            "Resetting sessions for all peer devices"
        );

        // Mark peer as stale (affects all devices)
        if let Err(e) = self.sesame_manager.mark_user_stale(peer_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to mark peer as stale (continuing anyway)"
            );
        }

        // Establish fresh sessions for ALL devices
        let device_ids = self.signal_service
            .ensure_sessions_all_devices(auth_client, &peer_id.to_string())
            .await
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to establish sessions for peer devices"
                );
                MessageError::from_signal_send(e, "Multi-device session establishment failed")
            })?;

        tracing::info!(
            peer_id = %peer_id,
            device_count = device_ids.len(),
            "Reset sessions for all peer devices"
        );

        Ok(device_ids)
    }

    /// Send to a mailbox with retry for transient network failures.
    ///
    /// Retries up to 3 times with exponential backoff (500ms, 1s, 2s)
    /// for retryable errors (timeout, unavailable, rate limit, server error).
    /// Non-retryable errors (auth, not found, invalid request) fail immediately.
    async fn send_to_mailbox_retried(
        &self,
        mailbox_manager: &MailboxManager,
        public_key: &[u8],
        encrypted_envelope: Vec<u8>,
        peer_id: &UserId,
        device_id: u32,
    ) -> Result<(), MessageError> {
        use crate::services::mailbox_manager::MailboxError;

        const MAX_ATTEMPTS: u32 = 4; // 1 initial + 3 retries

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            match mailbox_manager
                .send_to_mailbox(
                    public_key,
                    encrypted_envelope.clone(),
                    EnvelopeType::SealedSender,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(MailboxError::Network(net_err))
                    if net_err.is_retryable() && attempt + 1 < MAX_ATTEMPTS =>
                {
                    tracing::warn!(
                        peer_id = %peer_id,
                        device_id,
                        attempt = attempt + 1,
                        max_attempts = MAX_ATTEMPTS,
                        error = %net_err,
                        "Mailbox send failed, retrying"
                    );
                }
                Err(e) => {
                    return Err(MessageError::DmError(format!(
                        "Failed to send to mailbox: {}",
                        e
                    )));
                }
            }
        }

        unreachable!("retry loop always returns")
    }

    /// Inject our profile key into the Content if profile sharing is enabled.
    ///
    /// If this is a DM to a friend, auto-enables profile sharing on first message
    /// and injects our profile key into any DataMessage or EditMessage
    /// so the recipient can decrypt our display_name.
    pub(crate) fn inject_profile_key_if_sharing(
        &self,
        peer_id: &UserId,
        mut content: crate::proto::content_v1::Content,
    ) -> Result<crate::proto::content_v1::Content, MessageError> {
        use crate::domain::user::FriendshipStatus;
        use crate::proto::content_v1::content;
        use crate::storage::repositories::settings_repo::keys;

        // Get current user ID
        let our_user_id = self.settings_repo.current_user_id()?;

        // Check if this is a friendship
        let friendship = self
            .user_repo
            .get_friendship(&our_user_id, peer_id)
            .ok()
            .flatten();

        // Only share profile key with accepted friends
        let is_friend = friendship
            .as_ref()
            .map(|f| matches!(f.status, FriendshipStatus::Accepted))
            .unwrap_or(false);

        if !is_friend {
            tracing::trace!(
                peer_id = %peer_id,
                "Not a friend, skipping profile key injection"
            );
            return Ok(content);
        }

        // Check if profile sharing is enabled for this friendship
        let is_sharing = self
            .user_repo
            .is_profile_sharing(&our_user_id, peer_id)
            .unwrap_or(false);

        // Auto-enable profile sharing on first message to a friend
        if !is_sharing {
            if let Err(e) = self.user_repo.set_profile_sharing(&our_user_id, peer_id, true) {
                tracing::warn!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to auto-enable profile sharing"
                );
                return Ok(content);
            }
            tracing::info!(
                peer_id = %peer_id,
                "Auto-enabled profile sharing for friendship"
            );
        }

        // Get our profile key from settings
        let profile_key = match self.settings_repo.get_bytes(keys::PROFILE_KEY)? {
            Some(key) if key.len() == 32 => key,
            Some(key) => {
                tracing::warn!(
                    key_len = key.len(),
                    "Profile key has unexpected length, skipping injection"
                );
                return Ok(content);
            }
            None => {
                tracing::trace!("No profile key in settings, skipping injection");
                return Ok(content);
            }
        };

        // Inject profile key into DataMessage or EditMessage
        match &mut content.content {
            Some(content::Content::DataMessage(dm)) => {
                dm.profile_key = Some(profile_key);
                tracing::debug!(
                    peer_id = %peer_id,
                    "Injected profile key into DataMessage"
                );
            }
            Some(content::Content::Edit(edit)) => {
                if let Some(dm) = edit.data_message.as_mut() {
                    dm.profile_key = Some(profile_key);
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Injected profile key into EditMessage"
                    );
                }
            }
            Some(content::Content::Attachment(_)) => {
                // AttachmentMessage doesn't have profile_key field,
                // but the attachment is usually accompanied by a DataMessage
                tracing::trace!(
                    peer_id = %peer_id,
                    "AttachmentMessage - profile key not applicable"
                );
            }
            _ => {
                // Other message types (typing, receipts, reactions, etc.)
                // don't need profile key
                tracing::trace!(
                    peer_id = %peer_id,
                    "Non-data message type, profile key not applicable"
                );
            }
        }

        Ok(content)
    }
}

/// Reply metadata assembled from a quoted message, feeding both the outgoing
/// wire `Quote` and the local echo's stored reply fields. Shared across DM
/// transports so reply assembly lives in one place.
pub(crate) struct OutboundReplyParts {
    pub quote_attachments: Vec<crate::proto::content_v1::QuotedAttachment>,
    pub reply_to: crate::domain::ids::MessageId,
    pub reply_to_text: Option<String>,
    pub reply_to_thumbnail: Option<Vec<u8>>,
    pub reply_to_thumbnail_mime: Option<String>,
}

impl MessageService {
    /// Resolve a quoted message into wire + local reply metadata from a single
    /// source of truth. Used by every DM reply send (mailbox + ServerDm) so the
    /// quote-assembly logic isn't duplicated per transport.
    pub(crate) fn assemble_reply_parts(
        &self,
        quote_author: &crate::domain::ids::UserId,
        quote_timestamp: u64,
        quote_preview: &str,
    ) -> OutboundReplyParts {
        let original = self
            .message_repo
            .get_by_sender_timestamp(quote_author, quote_timestamp)
            .ok()
            .flatten();
        let original_attachments: Vec<_> = original
            .as_ref()
            .map(|m| {
                m.attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect()
            })
            .unwrap_or_default();
        let quote_attachments =
            crate::services::gateway::shared::build_quoted_attachments(&original_attachments);
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = quote_attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));
        let reply_to = original
            .as_ref()
            .map(|m| m.id)
            .unwrap_or_else(|| crate::domain::ids::MessageId::from_timestamp(quote_timestamp));
        let reply_to_text = if !quote_preview.is_empty() {
            Some(crate::services::gateway::shared::truncate_preview(quote_preview))
        } else {
            original.as_ref().map(|m| {
                crate::services::gateway::shared::message_reply_preview(m, &original_attachments)
            })
        }
        .filter(|t| !t.is_empty());

        OutboundReplyParts {
            quote_attachments,
            reply_to,
            reply_to_text,
            reply_to_thumbnail,
            reply_to_thumbnail_mime,
        }
    }
}
