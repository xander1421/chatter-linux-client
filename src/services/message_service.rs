//! Message service.
//!
//! Send, edit, delete, forward, and quote messages.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};

use crate::domain::ephemeral::MessageTTL;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, MixedContent, ReactionCount};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;

use crate::domain::channel::ChannelType;
use crate::network::grpc::auth_client::AuthClient;
use crate::services::signal_service::SignalService;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::message_repo::MessageRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Message service errors.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(ChannelId),

    /// Message to edit/delete was not found.
    #[error("message not found: {0}")]
    MessageNotFound(MessageId),

    /// User is not the message author (can't edit/delete).
    #[error("not the message author")]
    NotAuthor,

    /// Content validation failed.
    #[error("invalid content: {0}")]
    InvalidContent(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),
}

/// Message service for all message operations.
///
/// Orchestrates message sending, editing, and deletion by coordinating
/// network client and local storage.
pub struct MessageService {
    settings_repo: Arc<SettingsRepository>,
    message_repo: Arc<MessageRepository>,
    channel_repo: Arc<ChannelRepository>,
    signal_service: Arc<SignalService>,
}

impl MessageService {
    /// Create a new MessageService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        signal_service: Arc<SignalService>,
    ) -> Self {
        Self {
            settings_repo,
            message_repo,
            channel_repo,
            signal_service,
        }
    }

    /// Get encryption settings for a channel.
    ///
    /// Looks up channel locally first, fetches from server if not found.
    /// Returns (is_dm, require_encryption) tuple.
    async fn get_channel_encryption_settings(
        &self,
        channel_id: &ChannelId,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
    ) -> Result<(bool, bool), MessageError> {
        // Try local lookup first
        if let Ok(Some(channel)) = self.channel_repo.get(channel_id) {
            let is_dm = channel.channel_type == ChannelType::DirectMessage;
            let require_encryption = channel.config.require_encryption;
            return Ok((is_dm, require_encryption));
        }

        // Not found locally - fetch from server
        tracing::debug!(
            channel_id = %channel_id,
            "Channel not in local repo, fetching from server"
        );

        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await
            .map_err(MessageError::Network)?;

        // Convert and cache locally
        let channel = crate::services::channel_service::proto_to_domain_channel(&proto_channel)
            .map_err(|e| MessageError::InvalidContent(format!("Invalid channel data: {}", e)))?;

        // Store in local repo for future lookups
        if let Err(e) = self.channel_repo.insert(&channel) {
            tracing::warn!(error = %e, "Failed to cache channel locally");
        }

        let is_dm = channel.channel_type == ChannelType::DirectMessage;
        let require_encryption = channel.config.require_encryption;
        Ok((is_dm, require_encryption))
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, MessageError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| MessageError::InvalidContent("invalid user ID in settings".to_string()))
    }

    /// Send a message to a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `auth_client` - Mutable reference to auth client
    /// * `channel_client` - Mutable reference to channel client for encryption settings
    /// * `channel_id` - Target channel ID
    /// * `text` - Message text content
    /// * `attachment_ids` - Optional attachment IDs
    /// * `thread_id` - Optional parent message ID for threading
    /// * `mentioned_integrations` - AI integrations mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

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
            self.distribute_sender_key(message_client, channel_id, &skdm).await?;
        }

        // Call server
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send,
                thread_id.map(|id| id.to_string()).as_deref(),
                false, // ephemeral (Phase 34)
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and preserve attachment IDs
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = attachment_ids;

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Send encrypted control content to a channel (typing, reactions, etc.).
    ///
    /// Uses sender key encryption for E2E privacy. The content is sent as an
    /// ephemeral encrypted message that is not stored by the server.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Message client for sending
    /// * `auth_client` - Auth client for key fetching
    /// * `channel_client` - Channel client for encryption settings
    /// * `channel_id` - Target channel
    /// * `content` - The Content proto to send (TypingMessage, Reaction, etc.)
    pub async fn send_control_content(
        &self,
        message_client: &mut MessageClient,
        _auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        use prost::Message as ProstMessage;

        // Check logged in
        let _ = self.current_user_id()?;

        // Serialize the Content proto
        let plaintext = content.encode_to_vec();

        // Get encryption settings
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        // Skip if not an encrypted channel
        if is_dm || !require_encryption {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping control content - channel not encrypted"
            );
            return Ok(());
        }

        // Encrypt with sender key
        match self.signal_service.encrypt_for_group(channel_id, &plaintext).await {
            Ok((group_encrypted, skdm_opt)) => {
                // Distribute SKDM if new sender key was created
                if let Some(skdm) = skdm_opt {
                    self.distribute_sender_key(message_client, channel_id, &skdm).await?;
                }

                // Serialize GroupEncrypted and wrap in EncryptedData
                let group_bytes = group_encrypted.encode_to_vec();
                let encrypted_content = chat_v1::message::Content {
                    r#type: Some(chat_v1::message::content::Type::Encrypted(
                        chat_v1::EncryptedData {
                            version: chat_v1::CryptoVersion::SignalProtocol as i32,
                            ciphertext: group_bytes,
                            associated_data: b"sender_key_control".to_vec(),
                            nonce: vec![],
                            auth_tag: vec![],
                            signature: vec![],
                            device_id: String::new(),
                        }
                    )),
                };

                // Send as ephemeral message (not stored)
                let _ = message_client
                    .send_message(
                        &channel_id.to_string(),
                        encrypted_content,
                        None,  // no thread
                        true,  // ephemeral (control message)
                        None,  // no ttl (immediate delivery)
                        None,  // no crypto options
                        vec![], // no mentions
                        false,  // not targeting AI
                        vec![], // no attachments
                    )
                    .await?;

                tracing::debug!(
                    channel_id = %channel_id,
                    "Sent encrypted control content"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to encrypt control content"
                );
                Err(MessageError::EncryptionFailed(e.to_string()))
            }
        }
    }

    /// Send an ephemeral (self-destructing) message to a channel.
    ///
    /// The message will be marked for deletion after the TTL expires,
    /// starting from when the recipient reads it.
    ///
    /// Note: The caller is responsible for also calling `EphemeralService::track()`
    /// after this returns successfully to set up local ephemeral tracking.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Target channel ID
    /// * `text` - Message text content
    /// * `ttl` - Time-to-live for the message
    /// * `attachment_ids` - Optional attachment IDs
    /// * `thread_id` - Optional parent message ID for threading
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
        let sender_id = self.current_user_id()?;

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

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Edit a message in a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel containing the message
    /// * `message_id` - ID of the message to edit
    /// * `new_text` - New message text content
    ///
    /// # Errors
    ///
    /// Returns error if not logged in, message not found, user is not author,
    /// or content is empty.
    pub async fn edit_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
        new_text: &str,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let current_user = self.current_user_id()?;

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
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel containing the message
    /// * `message_id` - ID of the message to delete
    ///
    /// # Errors
    ///
    /// Returns error if not logged in, message not found, or user is not author.
    pub async fn delete_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let current_user = self.current_user_id()?;

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

        // Call server
        message_client
            .delete_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Soft delete locally - keep for history/sync
        let mut message = existing;
        message.mark_deleted();
        self.message_repo.update(&message)?;

        Ok(())
    }

    /// Pin a message in a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel containing the message
    /// * `message_id` - ID of the message to pin
    ///
    /// # Returns
    ///
    /// The updated message with is_pinned = true.
    pub async fn pin_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let proto_message = message_client
            .pin_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Parse sender ID from proto
        let sender_id = if proto_message.author_id.is_empty() {
            None
        } else {
            proto_message.author_id.parse().ok()
        };

        // Convert to domain message
        let message = proto_to_domain_message_with_sender(&proto_message, sender_id)?;

        // Update local storage
        match self.message_repo.get(&message.id)? {
            Some(_) => self.message_repo.update(&message)?,
            None => self.message_repo.insert(&message)?,
        }

        Ok(message)
    }

    /// Unpin a message from a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel containing the message
    /// * `message_id` - ID of the message to unpin
    pub async fn unpin_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        message_client
            .unpin_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Update local storage - set is_pinned = false
        if let Some(mut existing) = self.message_repo.get(message_id)? {
            existing.is_pinned = false;
            existing.updated_at = Utc::now();
            self.message_repo.update(&existing)?;
        }

        Ok(())
    }

    /// List pinned messages in a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel to get pinned messages from
    /// * `limit` - Maximum number of pinned messages to fetch
    ///
    /// # Returns
    ///
    /// Vector of pinned messages.
    pub async fn list_pinned_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = message_client
            .list_pinned_messages(&channel_id.to_string(), limit as i32)
            .await?;

        // Convert proto messages to domain messages and store locally
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            let message = proto_to_domain_message_with_sender(proto, sender_id)?;

            // Upsert to local storage
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }

    /// Send a reply (quote) to an existing message.
    ///
    /// Creates a new message that references another message. The reply_to
    /// relationship is stored locally for display purposes - the server
    /// treats this as a regular message.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `auth_client` - Mutable reference to auth client
    /// * `channel_client` - Mutable reference to channel client for encryption settings
    /// * `channel_id` - Target channel ID
    /// * `reply_to_id` - ID of the message being replied to
    /// * `text` - Reply text content
    /// * `attachment_ids` - Optional attachment IDs
    /// * `mentioned_integrations` - AI integrations mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_reply(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        reply_to_id: &MessageId,
        attachment_ids: Vec<AttachmentId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

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
            self.distribute_sender_key(message_client, channel_id, &skdm).await?;
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

        // Store locally with reply_to set
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Forward a message to another channel.
    ///
    /// Creates a new message in the target channel with the forwarded content.
    /// The forwarded message includes attribution text indicating it was forwarded.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `auth_client` - Mutable reference to auth client
    /// * `channel_client` - Mutable reference to channel client for encryption settings
    /// * `source_message_id` - ID of the message to forward
    /// * `target_channel_id` - Channel to forward the message to
    /// * `comment` - Optional comment to prepend to the forwarded content
    ///
    /// # Returns
    ///
    /// The newly created forwarded message.
    pub async fn forward_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        source_message_id: &MessageId,
        target_channel_id: &ChannelId,
        comment: Option<&str>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Get source message from local storage
        let source = self
            .message_repo
            .get(source_message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*source_message_id))?;

        // Extract text content from source message
        let original_text = source.as_text().unwrap_or("[No text content]");

        // Build forwarded content with attribution
        let forwarded_text = match comment {
            Some(c) if !c.trim().is_empty() => {
                format!("{}\n\n---\nForwarded:\n{}", c.trim(), original_text)
            }
            _ => format!("Forwarded:\n{}", original_text),
        };

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(forwarded_text.clone())),
        };

        // Get encryption settings from channel config
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(target_channel_id, channel_client)
            .await?;

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            target_channel_id,
            forwarded_text.as_bytes(),
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(message_client, target_channel_id, &skdm).await?;
        }

        // Copy attachment IDs from source if any
        let attachment_ids: Vec<String> = source
            .attachments
            .iter()
            .map(|id| id.to_string())
            .collect();

        // Call server
        let proto_message = message_client
            .send_message(
                &target_channel_id.to_string(),
                content_to_send,
                None,  // parent_id
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                Vec::new(), // mentioned_integrations
                false, // targets_ai_bot
                attachment_ids,
            )
            .await?;

        // Convert to domain message and copy source attachments
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = source.attachments.clone();

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Send a reply within a thread.
    ///
    /// Creates a new message in an existing thread. The parent_id is set to
    /// the thread root message ID.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `auth_client` - Mutable reference to auth client
    /// * `channel_client` - Mutable reference to channel client for encryption settings
    /// * `channel_id` - Channel containing the thread
    /// * `thread_root_id` - ID of the thread root message
    /// * `text` - Reply text content
    /// * `attachment_ids` - Optional attachment IDs
    /// * `mentioned_integrations` - AI integrations mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_thread_reply(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        thread_root_id: &MessageId,
        attachment_ids: Vec<AttachmentId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "thread reply must have text or attachments".to_string(),
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
            self.distribute_sender_key(message_client, channel_id, &skdm).await?;
        }

        // Call server - parent_id is thread_root_id for thread replies
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send,
                Some(&thread_root_id.to_string()), // parent_id = thread_root_id
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and preserve attachment IDs
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = attachment_ids;

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Helper to encrypt content based on channel type.
    ///
    /// # Encryption modes:
    /// - DM: Uses Signal Protocol sealed sender (pairwise sessions)
    /// - Encrypted channel: Uses Signal Protocol sender keys (group encryption)
    /// - Regular channel: No encryption
    ///
    /// # Arguments
    /// * `auth_client` - Auth client for fetching prekey bundles
    /// * `channel_id` - The channel/DM ID
    /// * `plaintext` - The plaintext message bytes
    /// * `original_content` - The original unencrypted content
    /// * `is_dm` - Whether this is a DM
    /// * `require_encryption` - Whether the channel requires sender key encryption
    ///
    /// # Returns
    /// A tuple of (content, optional_skdm). The SKDM is returned when a new
    /// sender key was created and must be distributed to channel members.
    async fn prepare_encrypted_content(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &ChannelId,
        plaintext: &[u8],
        original_content: chat_v1::message::Content,
        is_dm: bool,
        require_encryption: bool,
    ) -> (chat_v1::message::Content, Option<crate::proto::content_v1::SenderKeyDistribution>) {
        // DM: Use sealed sender (pairwise encryption)
        if is_dm {
            if let Ok(recipient_id) = channel_id.to_string().parse::<uuid::Uuid>() {
                match self.signal_service.encrypt_message(
                    auth_client,
                    &recipient_id.to_string(),
                    1, // Default to primary device
                    plaintext
                ).await {
                    Ok(ciphertext) => {
                        let encrypted_content = chat_v1::message::Content {
                            r#type: Some(chat_v1::message::content::Type::SealedSender(
                                chat_v1::SealedSenderMessage {
                                    sealed_body: ciphertext,
                                    ..Default::default()
                                }
                            )),
                        };
                        return (encrypted_content, None);
                    },
                    Err(e) => {
                        tracing::warn!("DM encryption failed: {}, sending cleartext", e);
                        return (original_content, None);
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
                    // This allows using existing proto types while carrying sender key ciphertext
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

    /// Distribute a sender key distribution message to channel members.
    ///
    /// Sends the SKDM as a special message in the channel so all members
    /// can receive it and decrypt future messages from this sender.
    async fn distribute_sender_key(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use prost::Message;

        // Serialize SKDM and send as encrypted message with special marker
        let skdm_bytes = skdm.encode_to_vec();

        let skdm_content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Encrypted(
                chat_v1::EncryptedData {
                    version: chat_v1::CryptoVersion::SignalProtocol as i32,
                    ciphertext: skdm_bytes,
                    // Mark as SKDM distribution message
                    associated_data: b"sender_key_distribution".to_vec(),
                    nonce: vec![],
                    auth_tag: vec![],
                    signature: vec![],
                    device_id: String::new(),
                }
            )),
        };

        // Send SKDM to channel - all members will receive and process it
        message_client
            .send_message(
                &channel_id.to_string(),
                skdm_content,
                None,  // parent_id
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                vec![], // mentioned_integrations
                false,  // targets_ai_bot
                vec![], // attachment_ids
            )
            .await?;

        tracing::info!(
            channel_id = %channel_id,
            "Distributed sender key to channel members"
        );

        Ok(())
    }

    /// Send a message to the SavedMessages channel (Note to Self).
    ///
    /// Follows Signal's approach:
    /// - If only one device (this device): store locally only
    /// - If multiple devices: send encrypted sync message to all devices
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Auth client for device listing and prekey fetching
    /// * `message_client` - Message client for sending sync messages
    /// * `text` - Message text content
    /// * `attachment_ids` - Optional attachment IDs
    pub async fn send_saved_message(
        &self,
        auth_client: &mut AuthClient,
        _message_client: &mut MessageClient, // Reserved for multi-device sync
        text: &str,
        attachment_ids: Vec<AttachmentId>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Validate content
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Get current device ID
        let current_device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .unwrap_or(1);

        // List all our devices to determine if we need to sync
        let devices = auth_client.list_devices().await?;
        let device_count = devices.devices.len();

        tracing::debug!(
            "SavedMessages: device_count={}, current_device_id={}",
            device_count,
            current_device_id
        );

        // Create the message locally first
        let message_id = MessageId::new();
        let now = Utc::now();
        let channel_id = ChannelId::from(sender_id); // SavedMessages channel_id = user_id

        let message = Message {
            id: message_id,
            channel_id,
            sender_id: Some(sender_id),
            content: MessageContent::Text(trimmed_text.to_string()),
            thread_id: None,
            reply_to: None,
            attachments: attachment_ids,
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: vec![],
            targets_ai_bot: false,
        };

        // Store locally
        self.message_repo.insert(&message)?;

        if device_count <= 1 {
            // Only this device - we're done (local-only storage)
            tracing::info!(
                "SavedMessages: stored locally only (single device), message_id={}",
                message_id
            );
        } else {
            // Multiple devices - need to sync to other devices
            // Signal uses SentTranscript sync messages for this
            //
            // TODO: Implement proper multi-device sync:
            // 1. Create SentTranscript proto message
            // 2. Encrypt for each linked device using Signal Protocol
            // 3. Send via device-specific queue (requires server support)
            //
            // For now, message is stored locally only. Other devices won't see it
            // until proper sync is implemented.
            let other_device_ids: Vec<u32> = devices
                .devices
                .iter()
                .filter(|d| d.device_id != current_device_id)
                .map(|d| d.device_id)
                .collect();

            tracing::warn!(
                "SavedMessages: multi-device sync not yet implemented. \
                 Message stored locally only. Other devices {:?} won't see message_id={}",
                other_device_ids,
                message_id
            );
        }

        Ok(message)
    }

    /// Get messages from a thread.
    ///
    /// Fetches messages from a specific thread.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel containing the thread
    /// * `thread_root_id` - ID of the thread root message
    /// * `limit` - Maximum number of messages to fetch
    /// * `before` - Optional message ID to fetch messages before (cursor)
    ///
    /// # Returns
    ///
    /// Vector of thread messages in reverse chronological order (newest first).
    pub async fn get_thread_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        thread_root_id: &MessageId,
        limit: u32,
        before: Option<&MessageId>,
    ) -> Result<Vec<Message>, MessageError> {
        use crate::proto::chat_v1::MessageSortDirection;

        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server with thread_root_id filter
        let response = message_client
            .get_messages(
                &channel_id.to_string(),
                limit as i32,
                Some(&thread_root_id.to_string()), // thread_root_id filter
                None, // page_token
                MessageSortDirection::SortNewestFirst,
                before.map(|id| id.to_string()).as_deref(),
                None, // reference_time
            )
            .await?;

        // Convert proto messages to domain messages and store locally
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            let message = proto_to_domain_message_with_sender(proto, sender_id)?;

            // Upsert to local storage (insert or update)
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }

    /// Get messages from a channel.
    ///
    /// Fetches messages from the server and stores them locally.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Channel to fetch messages from
    /// * `limit` - Maximum number of messages to fetch
    /// * `before` - Optional message ID to fetch messages before (cursor)
    ///
    /// # Returns
    ///
    /// Vector of messages in reverse chronological order (newest first).
    pub async fn get_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        limit: u32,
        before: Option<&MessageId>,
    ) -> Result<Vec<Message>, MessageError> {
        use crate::proto::chat_v1::MessageSortDirection;

        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = message_client
            .get_messages(
                &channel_id.to_string(),
                limit as i32,
                None, // thread_root_id
                None, // page_token
                MessageSortDirection::SortNewestFirst,
                before.map(|id| id.to_string()).as_deref(),
                None, // reference_time
            )
            .await?;

        // Convert proto messages to domain messages and store locally
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            let message = proto_to_domain_message_with_sender(proto, sender_id)?;

            // Upsert to local storage (insert or update)
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }
}

/// Convert a proto Message to a domain Message.
fn proto_to_domain_message(
    proto: &chat_v1::Message,
    sender_id: UserId,
) -> Result<Message, MessageError> {
    // Parse message ID
    let id: MessageId = proto
        .id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid message ID".to_string()))?;

    // Parse channel ID
    let channel_id: ChannelId = proto
        .channel_id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid channel ID".to_string()))?;

    // Parse thread ID (parent_id in proto)
    let thread_id: Option<MessageId> = if proto.parent_id.is_empty() {
        None
    } else {
        Some(
            proto
                .parent_id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid parent ID".to_string()))?,
        )
    };

    // Parse mentioned integrations
    let mentioned_integrations: Vec<IntegrationId> = proto
        .mentioned_integrations
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Convert proto content to domain content
    let content = proto_content_to_domain(proto.content.as_ref())?;

    // Extract attachment IDs from content
    let attachments = extract_attachment_ids_from_content(proto.content.as_ref());

    // Convert reaction counts
    let reaction_counts: Vec<ReactionCount> = proto
        .reaction_counts
        .iter()
        .map(|rc| ReactionCount::new(rc.emoji.clone(), rc.count as u32))
        .collect();

    // Convert timestamp (proto uses millis)
    let created_at = millis_to_datetime(proto.timestamp);
    let updated_at = if proto.updated_at > 0 {
        millis_to_datetime(proto.updated_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        Some(millis_to_datetime(proto.edited_at))
    } else {
        None
    };

    Ok(Message {
        id,
        channel_id,
        sender_id: Some(sender_id),
        content,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        attachments,
        is_pinned: proto.pinned,
        is_edited: proto.edited,
        is_deleted: false,
        created_at,
        updated_at,
        edited_at,
        reaction_counts,
        current_user_reactions: proto.current_user_reactions.clone(),
        mentioned_integrations,
        targets_ai_bot: proto.targets_ai_bot,
    })
}

/// Convert a proto Message to a domain Message with optional sender.
///
/// Used when fetching messages where sender_id comes from the proto (not known upfront).
fn proto_to_domain_message_with_sender(
    proto: &chat_v1::Message,
    sender_id: Option<UserId>,
) -> Result<Message, MessageError> {
    // Parse message ID
    let id: MessageId = proto
        .id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid message ID".to_string()))?;

    // Parse channel ID
    let channel_id: ChannelId = proto
        .channel_id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid channel ID".to_string()))?;

    // Parse thread ID (parent_id in proto)
    let thread_id: Option<MessageId> = if proto.parent_id.is_empty() {
        None
    } else {
        Some(
            proto
                .parent_id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid parent ID".to_string()))?,
        )
    };

    // Parse mentioned integrations
    let mentioned_integrations: Vec<IntegrationId> = proto
        .mentioned_integrations
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Convert proto content to domain content
    let content = proto_content_to_domain(proto.content.as_ref())?;

    // Extract attachment IDs from content
    let attachments = extract_attachment_ids_from_content(proto.content.as_ref());

    // Convert reaction counts
    let reaction_counts: Vec<ReactionCount> = proto
        .reaction_counts
        .iter()
        .map(|rc| ReactionCount::new(rc.emoji.clone(), rc.count as u32))
        .collect();

    // Convert timestamp (proto uses millis)
    let created_at = millis_to_datetime(proto.timestamp);
    let updated_at = if proto.updated_at > 0 {
        millis_to_datetime(proto.updated_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        Some(millis_to_datetime(proto.edited_at))
    } else {
        None
    };

    Ok(Message {
        id,
        channel_id,
        sender_id, // May be None for sealed sender messages
        content,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        attachments,
        is_pinned: proto.pinned,
        is_edited: proto.edited,
        is_deleted: false,
        created_at,
        updated_at,
        edited_at,
        reaction_counts,
        current_user_reactions: proto.current_user_reactions.clone(),
        mentioned_integrations,
        targets_ai_bot: proto.targets_ai_bot,
    })
}

/// Extract attachment IDs from proto message content.
///
/// Attachments can be in Content::File (single file) or Content::Mixed (multiple files).
fn extract_attachment_ids_from_content(
    proto_content: Option<&chat_v1::message::Content>,
) -> Vec<AttachmentId> {
    let Some(content) = proto_content else {
        return Vec::new();
    };

    let Some(ref content_type) = content.r#type else {
        return Vec::new();
    };

    match content_type {
        chat_v1::message::content::Type::File(file_meta) => {
            // Single file attachment
            if let Ok(id) = file_meta.id.parse() {
                vec![id]
            } else {
                Vec::new()
            }
        }
        chat_v1::message::content::Type::Mixed(mixed) => {
            // Multiple file attachments
            mixed
                .files
                .iter()
                .filter_map(|f| f.id.parse().ok())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Convert proto content to domain MessageContent.
fn proto_content_to_domain(
    proto_content: Option<&chat_v1::message::Content>,
) -> Result<MessageContent, MessageError> {
    let content = proto_content.ok_or_else(|| {
        MessageError::InvalidContent("message has no content".to_string())
    })?;

    match &content.r#type {
        Some(chat_v1::message::content::Type::Text(text)) => {
            Ok(MessageContent::Text(text.clone()))
        }
        Some(chat_v1::message::content::Type::Encrypted(_)) => {
            // Encrypted content - store as placeholder text for now
            // Full decryption is handled by crypto layer
            Ok(MessageContent::Text("[Encrypted message]".to_string()))
        }
        Some(chat_v1::message::content::Type::File(file_meta)) => {
            // File-only message - use MixedContent with no text
            let attachment_id: AttachmentId = file_meta
                .id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid file ID".to_string()))?;
            Ok(MessageContent::Mixed(MixedContent::attachments_only(vec![attachment_id])))
        }
        None => {
            // No content type - default to empty text
            Ok(MessageContent::Text(String::new()))
        }
        _ => {
            // Other types (reactions, system, call, AI response) - handle as text placeholder
            Ok(MessageContent::Text("[Special message]".to_string()))
        }
    }
}

/// Convert milliseconds timestamp to DateTime<Utc>.
fn millis_to_datetime(millis: u64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis as i64)
        .single()
        .unwrap_or_else(Utc::now)
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_error_display() {
        let err = MessageError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = MessageError::InvalidContent("empty".to_string());
        assert!(err.to_string().contains("empty"));

        let channel_id = ChannelId::new();
        let err = MessageError::ChannelNotFound(channel_id);
        assert!(err.to_string().contains("channel not found"));

        let message_id = MessageId::new();
        let err = MessageError::MessageNotFound(message_id);
        assert!(err.to_string().contains("message not found"));

        let err = MessageError::NotAuthor;
        assert!(err.to_string().contains("not the message author"));
    }

    #[test]
    fn test_proto_content_to_domain_text() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let proto_content = Content {
            r#type: Some(Type::Text("Hello world".to_string())),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert_eq!(text, "Hello world"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_none() {
        let result = proto_content_to_domain(None);
        assert!(result.is_err());
    }

    #[test]
    fn test_millis_to_datetime() {
        let millis = 1706140800000u64; // 2024-01-25 00:00:00 UTC
        let dt = millis_to_datetime(millis);
        assert_eq!(dt.timestamp_millis(), millis as i64);
    }
}
