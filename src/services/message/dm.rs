//! DM (Direct Message) operations.
//!
//! Signal Protocol encryption for DM messages, including bootstrap,
//! session management, and pending message queue.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::crypto::encrypt_dm_bytes;
use crate::domain::ids::UserId;
use crate::domain::message::Message;
use crate::mailbox::dm::DmConversationManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::mailbox_manager::MailboxManager;
use crate::storage::Database;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService, SendDmContentParams, SendDmMsgParams};

impl MessageService {
    /// Send a Content proto to a DM peer via two-layer encryption.
    ///
    /// Uses two encryption layers for privacy:
    /// 1. Signal Protocol (forward secrecy via Double Ratchet)
    /// 2. Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
    pub async fn send_dm_content(
        &self,
        params: SendDmContentParams<'_>,
    ) -> Result<(), MessageError> {
        let SendDmContentParams {
            auth_client,
            peer_id,
            content,
            database,
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

        // Load conversation to get peer's mailbox and shared secret
        tracing::debug!(peer_id = %peer_id, "MessageService: loading conversation state");
        let conversation_manager = DmConversationManager::new(database.clone());
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
            return self.handle_dm_bootstrap(
                auth_client,
                peer_id,
                content,
                database,
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

        let shared_secret = conversation
            .conversation_key
            .as_ref()
            .and_then(|k| k.get_shared_secret())
            .ok_or_else(|| MessageError::DmError("No shared secret".into()))?;

        // Serialize Content proto
        let plaintext = content.encode_to_vec();

        // Layer 1: Signal Protocol encryption (forward secrecy)
        let peer_device_id = 1u32; // Primary device
        let peer_id_str = peer_id.to_string();

        let signal_ciphertext = self
            .signal_service
            .encrypt_message(auth_client, &peer_id_str, peer_device_id, &plaintext)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Signal encrypt: {}", e)))?;

        // Layer 2: Mailbox encryption (sender anonymity)
        let mailbox_encrypted = encrypt_dm_bytes(&signal_ciphertext, shared_secret)
            .map_err(|e| MessageError::EncryptionFailed(format!("Mailbox encrypt: {}", e)))?;

        // Send to peer's mailbox
        mailbox_manager
            .send_to_mailbox(&peer_mailbox.public_key, mailbox_encrypted, EnvelopeType::SealedSender)
            .await
            .map_err(|e| MessageError::DmError(format!("Failed to send to mailbox: {}", e)))?;

        tracing::debug!(peer_id = %peer_id, "Sent DM via two-layer encryption");

        Ok(())
    }

    /// Send a Content proto to a DM peer (simplified API - no client params).
    pub async fn send_dm(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        database: Arc<Database>,
        mailbox_manager: &MailboxManager,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();

        self.send_dm_content(SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id,
            content,
            database,
            mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await
    }

    /// Send a text message to a DM peer.
    pub async fn send_dm_text(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        text: &str,
        database: Arc<Database>,
        mailbox_manager: &MailboxManager,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            text_len = text.len(),
            "MessageService: send_dm_text called"
        );
        let sent_at = TimestampNanos::now().as_u64();
        let content = crate::services::build_text_content(text, sent_at);
        self.send_dm_content(SendDmContentParams {
            auth_client,
            peer_id,
            content,
            database: database.clone(),
            mailbox_manager,
            grpc_pool: Some(grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await?;

        // Store the message locally
        let our_user_id = self.current_user_id()?;
        if let Ok(Some(dm_channel)) = self.channel_repo.get_dm_channel_by_peer(&our_user_id, peer_id) {
            let message = Message::new_text(dm_channel.id, Some(our_user_id), text.to_string(), sent_at);
            if let Err(e) = self.message_repo.insert(&message) {
                tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store sent text DM");
            }
        }

        Ok(())
    }

    /// Send a DM with text and/or attachments.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_dm_message(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
        database: Arc<Database>,
        mailbox_manager: &MailboxManager,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            text_len = text.len(),
            attachment_count = attachment_pointers.len(),
            "MessageService: send_dm_message called"
        );
        let sent_at = TimestampNanos::now().as_u64();
        let content = crate::services::build_data_message_content(text, attachment_pointers, sent_at);
        self.send_dm_content(SendDmContentParams {
            auth_client,
            peer_id,
            content,
            database,
            mailbox_manager,
            grpc_pool: Some(grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await
    }

    /// Send a DM message with attachments (simplified API - no client params).
    pub async fn send_dm_msg(
        &self,
        params: SendDmMsgParams<'_>,
    ) -> Result<Message, MessageError> {
        let SendDmMsgParams {
            peer_id,
            text,
            attachment_pointers,
            attachment_ids,
            dm_channel_id,
            database,
            mailbox_manager,
            sent_at,
        } = params;

        // Use caller-provided timestamp for proto AND storage
        let content = crate::services::build_data_message_content(text, attachment_pointers, sent_at);

        // Send the message via Signal Protocol - pass attachment_ids for storage
        let mut auth_client = self.auth_client();
        self.send_dm_content(SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id,
            content,
            database,
            mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        }).await?;

        // Store the message locally with attachments
        let current_user_id = self.current_user_id()?;
        let mut message = Message::new_text(*dm_channel_id, Some(current_user_id), text.to_string(), sent_at);
        message.attachments = attachment_ids;

        if let Err(e) = self.message_repo.insert(&message) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to store sent DM with attachments"
            );
        }

        Ok(message)
    }

    /// Send pending messages after bootstrap completes.
    pub async fn send_pending_messages(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        database: Arc<Database>,
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
                    database: database.clone(),
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

        Ok(sent_count)
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
                MessageError::EncryptionFailed(format!("Session establishment failed: {}", e))
            })?;

        tracing::info!(
            peer_id = %peer_id,
            device_id,
            "Successfully reset and established new session"
        );

        Ok(())
    }
}
