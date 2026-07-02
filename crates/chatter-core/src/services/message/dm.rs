//! DM (Direct Message) operations.
//!
//! Signal Protocol encryption for DM messages, including bootstrap,
//! session management, and pending message queue.

use std::path::Path;
use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::crypto::encrypt_dm_bytes;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, Message};
use crate::mailbox::dm::DmConversationManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;
use crate::services::mailbox_manager::MailboxManager;
use crate::util::time::TimestampNanos;

use super::{DmSendResult, MessageError, MessageService, SendDmContentParams, SendDmMsgParams};

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

        let peer_ciphertexts = self
            .signal_service
            .encrypt_message_all_devices(auth_client, &peer_id_str, &plaintext)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Multi-device encrypt: {}", e)))?;

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

    /// Send a text message to a DM peer.
    pub async fn send_dm_text(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        text: &str,
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
            mailbox_manager,
            grpc_pool: Some(grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await?;

        // Store the message locally
        let our_user_id = self.settings_repo.current_user_id()?;
        if let Ok(Some(dm_channel)) = self.channel_repo.get_dm_channel_by_peer(&our_user_id, peer_id) {
            let message = Message::new_text(dm_channel.id, ConversationType::MailboxDm, Some(our_user_id), text.to_string(), sent_at);
            if let Err(e) = self.message_repo.insert(&message) {
                tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store sent text DM");
            }
        }

        Ok(())
    }

    /// Send a DM with text and/or attachments.
    pub async fn send_dm_message(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
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
            mailbox_manager,
            sent_at,
        } = params;

        // Use caller-provided timestamp for proto AND storage
        let content = crate::services::build_data_message_content(text, attachment_pointers, sent_at);

        // Send the message via Signal Protocol - pass attachment_ids for storage
        let mut auth_client = self.auth_client();
        let send_result = self.send_dm_content(SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id,
            content,
            mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        }).await;

        // Build the message for storage
        let current_user_id = self.settings_repo.current_user_id()?;
        let mut message = Message::new_text(*dm_channel_id, ConversationType::MailboxDm, Some(current_user_id), text.to_string(), sent_at);
        message.attachments = attachment_ids;

        // Handle send result - save to DB for both success and bootstrap cases
        match send_result {
            Ok(()) => {
                // Message sent successfully - save to DB
                if let Err(e) = self.message_repo.insert(&message) {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to store sent DM with attachments"
                    );
                }
                Ok(message)
            }
            Err(MessageError::BootstrapInitiated) => {
                // Bootstrap initiated - message queued, but STILL save to DB
                // so it persists when switching conversations
                if let Err(e) = self.message_repo.insert(&message) {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to store queued DM during bootstrap"
                    );
                }
                tracing::info!(
                    peer_id = %peer_id,
                    sent_at = sent_at,
                    "Saved queued message to DB during bootstrap"
                );
                Err(MessageError::BootstrapInitiated)
            }
            Err(e) => Err(e),
        }
    }

    /// Send a DM with file attachments (high-level API).
    ///
    /// This method handles the complete flow:
    /// 1. Upload each file to CDN with encryption
    /// 2. Build AttachmentPointer protos
    /// 3. Send message via Signal Protocol with two-layer encryption
    /// 4. Store message locally
    ///
    /// # Arguments
    /// * `peer_id` - The peer's user ID
    /// * `dm_channel_id` - The DM channel ID for local storage
    /// * `text` - Message text
    /// * `file_paths` - Local file paths to upload as attachments
    /// * `mailbox_manager` - For sending to peer's mailbox
    pub async fn send_dm_with_reply(
        &self,
        peer_id: &UserId,
        dm_channel_id: &ChannelId,
        text: &str,
        file_paths: Vec<&Path>,
        quote_author_id: &UserId,
        quote_timestamp: u64,
        quote_text: &str,
        mailbox_manager: &MailboxManager,
    ) -> Result<DmSendResult, MessageError> {
        // Upload attachments with encryption
        let attachment_service = AttachmentService::new(self.attachment_repo.clone(), self.grpc_pool.clone());
        let mut attachment_ids = Vec::with_capacity(file_paths.len());
        let mut attachment_pointers = Vec::with_capacity(file_paths.len());
        let mut gif_attachment_ids = Vec::new();
        
        for path in file_paths {
            match attachment_service.upload_to_cdn_encrypted(path).await {
                Ok(result) => {
                    attachment_ids.push(result.attachment_id);
                    
                    // Track GIF attachments for animation
                    if result.content_type.starts_with("image/gif") {
                        gif_attachment_ids.push(result.attachment_id);
                    }
                    
                    // Build AttachmentPointer proto with all required fields
                    let pointer = crate::proto::content_v1::AttachmentPointer {
                        cdn_key: result.cdn_key,
                        encryption_key: result.encryption_key.to_vec(),
                        digest: result.digest.to_vec(),
                        plaintext_size: result.plaintext_size,
                        content_type: result.content_type,
                        file_name: result.file_name,
                        thumbnail: result.thumbnail,
                        thumbnail_width: 0,
                        thumbnail_height: 0,
                        blur_hash: String::new(),
                        width: 0,
                        height: 0,
                        duration_ms: result.duration_ms,
                        voice_note: false,
                        incremental_mac: result.incremental_mac,
                        chunk_size: result.chunk_size,
                        ciphertext_size: result.ciphertext_size,
                        plaintext_hash: String::new(),
                        waveform: result.waveform,
                    };
                    attachment_pointers.push(pointer);
                }
                Err(e) => {
                    tracing::error!("Failed to upload attachment: {}", e);
                    return Err(MessageError::AttachmentError(e.to_string()));
                }
            }
        }
        
        // Resolve quoted message locally so we can populate both wire + local
        // reply metadata from a single source of truth.
        let original = self
            .message_repo
            .get_by_sender_timestamp(quote_author_id, quote_timestamp)
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
        let (quote_thumb, quote_mime) = quote_attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

        // Build reply content with Quote proto
        let sent_at = TimestampNanos::now().as_u64();
        let content = crate::services::build_reply_content(
            text,
            attachment_pointers,
            &quote_author_id.to_string(),
            quote_timestamp,
            quote_text,
            quote_attachments,
            sent_at,
        );

        // Send the message
        self.send_dm(peer_id, content, mailbox_manager).await?;

        // Store the message locally
        let our_user_id = self.settings_repo.current_user_id()?;
        let mut message = Message::new_text(*dm_channel_id, ConversationType::MailboxDm, Some(our_user_id), text.to_string(), sent_at);
        message.attachments = attachment_ids.clone();

        message.reply_to = Some(
            original
                .as_ref()
                .map(|m| m.id)
                .unwrap_or_else(|| crate::domain::ids::MessageId::from_timestamp(quote_timestamp)),
        );
        message.reply_to_author_id = Some(*quote_author_id);
        message.reply_to_text = {
            if !quote_text.is_empty() {
                Some(crate::services::gateway::shared::truncate_preview(quote_text))
            } else {
                original.as_ref().map(|m| {
                    crate::services::gateway::shared::message_reply_preview(m, &original_attachments)
                })
            }
            .filter(|t| !t.is_empty())
        };
        message.reply_to_thumbnail = quote_thumb;
        message.reply_to_thumbnail_mime = quote_mime;

        // Store in database
        if let Err(e) = self.message_repo.insert(&message) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to store sent reply DM"
            );
        }
        
        Ok(DmSendResult {
            message: Some(message),
            attachment_ids,
            gif_attachment_ids,
            sent_at,
            bootstrap_initiated: false,
        })
    }
    
    pub async fn send_dm_with_files(
        &self,
        peer_id: &UserId,
        dm_channel_id: &ChannelId,
        text: &str,
        file_paths: Vec<&Path>,
        mailbox_manager: &MailboxManager,
    ) -> Result<DmSendResult, MessageError> {
        // Upload attachments with encryption
        let attachment_service = AttachmentService::new(self.attachment_repo.clone(), self.grpc_pool.clone());
        let mut attachment_ids = Vec::with_capacity(file_paths.len());
        let mut attachment_pointers = Vec::with_capacity(file_paths.len());
        let mut gif_attachment_ids = Vec::new();

        for file_path in &file_paths {
            // Detect GIF by extension or content type
            let is_gif = file_path
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("gif"))
                .unwrap_or(false);

            tracing::info!(
                path = %file_path.display(),
                is_gif = is_gif,
                "Encrypting and uploading DM attachment to CDN"
            );

            match attachment_service.upload_to_cdn_encrypted(file_path).await {
                Ok(result) => {
                    tracing::info!(
                        cdn_key = %result.cdn_key,
                        attachment_id = %result.attachment_id,
                        filename = %result.file_name,
                        "DM attachment uploaded to CDN"
                    );
                    if is_gif {
                        gif_attachment_ids.push(result.attachment_id);
                    }
                    attachment_ids.push(result.attachment_id);
                    attachment_pointers.push(result.to_attachment_pointer());
                }
                Err(e) => {
                    tracing::error!(
                        path = %file_path.display(),
                        error = %e,
                        "Failed to upload DM attachment to CDN"
                    );
                    return Err(MessageError::AttachmentError(e.to_string()));
                }
            }
        }

        // Generate timestamp for proto AND storage
        let sent_at = TimestampNanos::now().as_u64();

        tracing::info!(
            peer_id = %peer_id,
            text_len = text.len(),
            attachment_count = attachment_pointers.len(),
            sent_at = sent_at,
            "Sending DM with files via send_dm_msg"
        );

        // Clone IDs before passing to send (needed for result in all cases)
        let result_attachment_ids = attachment_ids.clone();
        let result_gif_ids = gif_attachment_ids.clone();

        // Send message with uploaded attachments
        match self
            .send_dm_msg(SendDmMsgParams {
                peer_id,
                text,
                attachment_pointers,
                attachment_ids,
                dm_channel_id,
                mailbox_manager,
                sent_at,
            })
            .await
        {
            Ok(message) => Ok(DmSendResult {
                message: Some(message),
                attachment_ids: result_attachment_ids,
                gif_attachment_ids: result_gif_ids,
                sent_at,
                bootstrap_initiated: false,
            }),
            Err(MessageError::BootstrapInitiated) => {
                // First DM - bootstrap was initiated, message was queued
                // Return success with bootstrap flag so UI can show the message
                tracing::info!(
                    peer_id = %peer_id,
                    attachment_count = result_attachment_ids.len(),
                    "DM bootstrap initiated - message queued, attachments uploaded"
                );
                Ok(DmSendResult {
                    message: None,
                    attachment_ids: result_attachment_ids,
                    gif_attachment_ids: result_gif_ids,
                    sent_at,
                    bootstrap_initiated: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Send a DM with pre-uploaded attachment pointers.
    ///
    /// Like `send_dm_with_files` but skips the upload step — caller provides
    /// already-uploaded `AttachmentPointer`s and their `AttachmentId`s.
    /// Used when attachments are pre-uploaded via `upload_pending_attachments()`.
    pub async fn send_dm_with_pointers(
        &self,
        peer_id: &UserId,
        dm_channel_id: &ChannelId,
        text: &str,
        attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
        attachment_ids: Vec<crate::domain::AttachmentId>,
        mailbox_manager: &MailboxManager,
    ) -> Result<DmSendResult, MessageError> {
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();

        let result_attachment_ids = attachment_ids.clone();

        match self
            .send_dm_msg(SendDmMsgParams {
                peer_id,
                text,
                attachment_pointers,
                attachment_ids,
                dm_channel_id,
                mailbox_manager,
                sent_at,
            })
            .await
        {
            Ok(message) => Ok(DmSendResult {
                message: Some(message),
                attachment_ids: result_attachment_ids,
                gif_attachment_ids: vec![],
                sent_at,
                bootstrap_initiated: false,
            }),
            Err(MessageError::BootstrapInitiated) => {
                Ok(DmSendResult {
                    message: None,
                    attachment_ids: result_attachment_ids,
                    gif_attachment_ids: vec![],
                    sent_at,
                    bootstrap_initiated: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Send pending messages after bootstrap completes.
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
                MessageError::EncryptionFailed(format!("Multi-device session establishment failed: {}", e))
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
