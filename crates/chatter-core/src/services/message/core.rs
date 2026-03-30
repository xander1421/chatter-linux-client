//! Core message operations.
//!
//! Send, edit, delete, reply, forward, and ephemeral message operations.

use std::path::Path;

use prost::Message as ProstMessage;

use crate::domain::ephemeral::MessageTTL;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId};
use crate::domain::message::{Message, MessageContent};
use crate::mailbox::group::GroupFanout;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::gateway::builders::build_data_message_content_with_channel;
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    /// Send a message to a channel.
    ///
    /// Encrypts with sender keys if channel requires encryption. When
    /// `group_fanout` is provided and channel is encrypted, the message
    /// will be delivered to member mailboxes IN ADDITION to the channel-based
    /// delivery. This provides traffic analysis resistance per NDSS '21 paper.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Get encryption settings from channel config
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            channel_id,
            trimmed_text.as_bytes(),
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server
        tracing::debug!(
            channel_id = %channel_id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Sending message to server API"
        );
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send.clone(),
                thread_id.map(|id| id.to_string()).as_deref(),
                false, // ephemeral (Phase 34)
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %proto_message.id,
            "[SEND_MESSAGE] Server API success, message_id={}", proto_message.id
        );

        // Also send via group mailboxes if available (NDSS '21 paper)
        // Per Signal Protocol: Use pairwise sealed sender encryption for each member.
        // Sender identity is revealed via SSDecVer, NOT from plaintext metadata.
        if let Some(fanout) = group_fanout {
            if require_encryption {
                // Build a proper Content proto with DataMessage including attachments
                let sent_at = TimestampNanos::now().as_u64();
                let content_proto = build_data_message_content_with_channel(
                    trimmed_text,
                    attachment_pointers.clone(),
                    &channel_id.to_string(),
                    sent_at,
                );
                let plaintext = content_proto.encode_to_vec();

                // Use pairwise encryption (Double Ratchet sealed sender) per NDSS '21
                self.send_pairwise_to_group_mailboxes(fanout, channel_id, &plaintext, auth_client)
                    .await?;
            }
        }

        // Convert to domain message and preserve attachment IDs
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = attachment_ids;

        // IMPORTANT: Check if server returned a different channel_id
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_MESSAGE] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            // Fix: Use the input channel_id, not the server's
            message.conversation_id = *channel_id;
        }

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally
        tracing::info!(
            channel_id = %message.conversation_id,
            message_id = %message.id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Storing message locally"
        );
        self.message_repo.insert(&message)?;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %message.id,
            "[SEND_MESSAGE] Message stored successfully"
        );

        Ok(message)
    }

    /// Send a message to a channel (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    ///
    /// # Arguments
    /// * `attachment_ids` - IDs for server API and local storage
    /// * `attachment_pointers` - Full AttachmentPointer protos for E2E encrypted group delivery
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_message(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send a message with file attachments (high-level API).
    ///
    /// This method handles the complete flow:
    /// 1. Upload each file to CDN with encryption
    /// 2. Build AttachmentPointer protos
    /// 3. Send message via server API
    /// 4. Send via group mailboxes (if encrypted channel)
    ///
    /// # Arguments
    /// * `group_fanout` - Group member mailbox info for E2E encrypted delivery
    /// * `channel_id` - Target channel
    /// * `text` - Message text
    /// * `file_paths` - Local file paths to upload as attachments
    /// * `thread_id` - Optional thread root for thread replies
    /// * `mentioned_integrations` - IDs of mentioned integrations (bots)
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_files(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        file_paths: Vec<&Path>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Upload attachments with encryption
        let attachment_service = AttachmentService::new(self.attachment_repo.clone(), self.grpc_pool.clone());
        let mut attachment_ids = Vec::with_capacity(file_paths.len());
        let mut attachment_pointers = Vec::with_capacity(file_paths.len());

        for file_path in &file_paths {
            tracing::info!(
                path = %file_path.display(),
                "Encrypting and uploading attachment to CDN"
            );

            match attachment_service.upload_to_cdn_encrypted(file_path).await {
                Ok(result) => {
                    tracing::info!(
                        cdn_key = %result.cdn_key,
                        attachment_id = %result.attachment_id,
                        filename = %result.file_name,
                        "Attachment uploaded to CDN"
                    );
                    attachment_ids.push(result.attachment_id);
                    attachment_pointers.push(result.to_attachment_pointer());
                }
                Err(e) => {
                    tracing::error!(
                        path = %file_path.display(),
                        error = %e,
                        "Failed to upload attachment to CDN"
                    );
                    return Err(MessageError::AttachmentError(e.to_string()));
                }
            }
        }

        // Send message with uploaded attachments
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send a message with pre-uploaded attachment pointers.
    ///
    /// Like `send_with_files` but skips the upload step.
    pub async fn send_with_pointers(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send control content to a group channel (typing indicators, reactions, etc.)
    ///
    /// Per NDSS '21: Uses pairwise sealed sender encryption for each member.
    /// Sender identity is cryptographically bound inside the ciphertext.
    pub async fn send_control_content(
        &self,
        _message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        use prost::Message as ProstMessage;

        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Serialize content for encryption
        let plaintext = content.encode_to_vec();

        // Get encryption settings from channel config
        let (_is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        // Send via group mailboxes if available and encrypted
        // Per Signal Protocol: Use pairwise sealed sender encryption for each member.
        if let Some(fanout) = group_fanout {
            if require_encryption {
                // Use pairwise encryption (Double Ratchet sealed sender) per NDSS '21
                self.send_pairwise_to_group_mailboxes(fanout, channel_id, &plaintext, auth_client)
                    .await?;
            }
        }

        // For control messages in encrypted channels, we've already sent via mailboxes
        // The server doesn't need to see control messages for encrypted channels
        if !require_encryption {
            tracing::debug!(
                channel_id = %channel_id,
                "Control message for unencrypted channel (no mailbox delivery)"
            );
        }

        Ok(())
    }

    /// Send control content to a channel (simplified API - no client params).
    pub async fn send_control(
        &self,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_control_content(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            content,
            group_fanout,
        )
        .await
    }

    /// Send an ephemeral (self-destructing) message to a channel.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_ephemeral_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        text: &str,
        ttl: MessageTTL,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Convert TTL to prost Duration
        let ephemeral_ttl = Some(prost_types::Duration {
            seconds: ttl.as_secs() as i64,
            nanos: 0,
        });

        // Call server with ephemeral flag
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content,
                thread_id.map(|id| id.to_string()).as_deref(),
                true,          // ephemeral = true
                ephemeral_ttl, // TTL duration
                None,          // crypto_options
                Vec::new(),    // mentioned_integrations (not typically used with ephemeral)
                false,         // targets_ai_bot
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and preserve attachment IDs
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = attachment_ids;

        // IMPORTANT: Ensure channel_id matches (server might return different format)
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_EPHEMERAL] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            message.conversation_id = *channel_id;
        }

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Edit a message in a channel.
    pub async fn edit_message(
        &self,
        channel_id: &ChannelId,
        message_id: &MessageId,
        new_text: &str,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Validate new content
        let trimmed_text = new_text.trim();
        if trimmed_text.is_empty() {
            return Err(MessageError::InvalidContent(
                "message content cannot be empty".to_string(),
            ));
        }

        // Get existing message from local DB
        let existing = self
            .message_repo
            .get(message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*message_id))?;

        // Verify sender matches current user (only if sender_id is known)
        if let Some(sender_id) = existing.sender_id {
            if sender_id != current_user {
                return Err(MessageError::NotAuthor);
            }
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Call server
        let mut message_client = self.message_client();
        let proto_message = message_client
            .edit_message(&message_id.to_string(), &channel_id.to_string(), content)
            .await?;

        // Convert to domain message (use server response for consistency)
        let mut updated_message = proto_to_domain_message(&proto_message, current_user)?;

        // Mark edited locally (in case server didn't set it)
        updated_message.mark_edited();

        // Update local storage
        self.message_repo.update(&updated_message)?;

        Ok(updated_message)
    }

    /// Delete a message from a channel.
    pub async fn delete_message(
        &self,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Get existing message to verify ownership
        let existing = self
            .message_repo
            .get(message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*message_id))?;

        // Verify sender matches current user (only if sender_id is known)
        if let Some(sender_id) = existing.sender_id {
            if sender_id != current_user {
                return Err(MessageError::NotAuthor);
            }
        }

        // Call server
        let mut message_client = self.message_client();
        message_client
            .delete_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Soft delete locally - keep for history/sync
        let mut message = existing;
        message.mark_deleted();
        self.message_repo.update(&message)?;

        Ok(())
    }

    /// Send a reply (quote) to an existing message.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_reply(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        reply_to_id: &MessageId,
        attachment_ids: Vec<AttachmentId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate reply_to exists locally (informational - server validates)
        if self.message_repo.get(reply_to_id)?.is_none() {
            // Log but don't fail - server will validate
            // The message might exist on server but not locally
        }

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "reply must have text or attachments".to_string(),
            ));
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Get encryption settings from channel config
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            channel_id,
            trimmed_text.as_bytes(),
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server - note: reply_to is stored locally, not sent to server
        // Server's parent_id is for threads, not quotes
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send,
                None,  // parent_id is for threads, not replies
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and set reply_to + attachments
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.reply_to = Some(*reply_to_id);
        message.attachments = attachment_ids;

        // Populate reply quote context so reply bubbles survive reload
        if let Ok(Some(original)) = self.message_repo.get(reply_to_id) {
            message.reply_to_author_id = original.sender_id;
            message.reply_to_text = original.as_text().map(|t| t.to_string());
        }

        // IMPORTANT: Ensure channel_id matches (server might return different format)
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_REPLY] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            message.conversation_id = *channel_id;
        }

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally with reply_to set
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Helper to encrypt content based on channel type.
    pub(crate) async fn prepare_encrypted_content(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &ChannelId,
        plaintext: &[u8],
        original_content: chat_v1::message::Content,
        is_dm: bool,
        require_encryption: bool,
    ) -> (chat_v1::message::Content, Option<crate::proto::content_v1::SenderKeyDistribution>) {
        // DM: Use sealed sender (pairwise encryption)
        // Multi-device: encrypt for ALL recipient devices, send primary ciphertext
        // Server handles device routing via channel subscription
        if is_dm {
            if let Ok(recipient_id) = channel_id.to_string().parse::<uuid::Uuid>() {
                // Encrypt for ALL recipient devices (Signal multi-device)
                match self.signal_service.encrypt_message_all_devices(
                    auth_client,
                    &recipient_id.to_string(),
                    plaintext
                ).await {
                    Ok(device_ciphertexts) => {
                        // For channel DMs, server handles device addressing
                        // Use primary device (1) ciphertext, fallback to first available
                        let primary_ciphertext = device_ciphertexts.get(&1)
                            .or_else(|| device_ciphertexts.values().next())
                            .cloned();

                        if let Some(ciphertext) = primary_ciphertext {
                            tracing::debug!(
                                recipient = %recipient_id,
                                device_count = device_ciphertexts.len(),
                                "Channel DM encrypted for devices"
                            );

                            let encrypted_content = chat_v1::message::Content {
                                r#type: Some(chat_v1::message::content::Type::SealedSender(
                                    chat_v1::SealedSenderMessage {
                                        sealed_body: ciphertext,
                                        ..Default::default()
                                    }
                                )),
                            };
                            return (encrypted_content, None);
                        }
                        // No ciphertexts available - fall through to cleartext
                        tracing::warn!("DM encryption produced no ciphertexts");
                    },
                    Err(e) => {
                        tracing::warn!("DM encryption failed: {}, sending cleartext", e);
                    }
                }
            }
            return (original_content, None);
        }

        // Encrypted channel: Use sender keys (group encryption)
        if require_encryption {
            match self.signal_service.encrypt_for_group(channel_id, plaintext).await {
                Ok((group_encrypted, skdm_opt)) => {
                    use prost::Message;
                    // Serialize GroupEncrypted proto and pack into EncryptedData
                    let group_bytes = group_encrypted.encode_to_vec();

                    let encrypted_content = chat_v1::message::Content {
                        r#type: Some(chat_v1::message::content::Type::Encrypted(
                            chat_v1::EncryptedData {
                                version: chat_v1::CryptoVersion::SignalProtocol as i32,
                                ciphertext: group_bytes,
                                // Mark as sender key message in associated_data
                                associated_data: b"sender_key".to_vec(),
                                // Other fields empty for sender key
                                nonce: vec![],
                                auth_tag: vec![],
                                signature: vec![],
                                device_id: String::new(),
                            }
                        )),
                    };
                    return (encrypted_content, skdm_opt);
                },
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Group encryption failed, sending cleartext"
                    );
                    return (original_content, None);
                }
            }
        }

        // Regular channel: No encryption
        (original_content, None)
    }
}

// Re-export proto conversion functions from conversion module
pub use super::conversion::{proto_to_domain_message, proto_to_domain_message_with_sender, proto_content_to_domain};
