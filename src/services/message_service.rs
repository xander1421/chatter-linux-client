//! Message service.
//!
//! Send, edit, delete, forward, and quote messages.

use std::sync::Arc;

use chrono::Utc;

use crate::util::time::TimestampNanos;

use crate::crypto::encrypt_dm_bytes;
use crate::domain::ephemeral::MessageTTL;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, MixedContent, ReactionCount};
use crate::error::{NetworkError, StorageError};
use crate::mailbox::dm::{
    release_bootstrap_lock, try_acquire_bootstrap_lock, DmBootstrap, DmConversationManager,
};
use crate::mailbox::group::{GroupFanout, GroupMailboxManager};
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::message_client::MessageClient;
use crate::network::grpc::MailboxClient;
use crate::proto::chat_v1::{self, SealedSenderMessage};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::mailbox_manager::MailboxManager;
use crate::storage::Database;

use crate::crypto::signal::{SesameManager, SqliteSesameStore};
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

    /// DM conversation error.
    #[error("dm error: {0}")]
    DmError(String),

    /// DM bootstrap was initiated - message will be sent after key exchange completes.
    #[error("DM bootstrap initiated - secure channel being established")]
    BootstrapInitiated,
}

/// Parameters for sending DM content.
pub struct SendDmContentParams<'a> {
    /// Auth client for key fetching.
    pub auth_client: &'a mut AuthClient,
    /// The peer's user ID.
    pub peer_id: &'a UserId,
    /// The Content proto to send.
    pub content: crate::proto::content_v1::Content,
    /// Database for conversation state.
    pub database: Arc<Database>,
    /// For sending to the peer's mailbox.
    pub mailbox_manager: &'a MailboxManager,
    /// Optional gRPC pool for bootstrap (needed for first DM).
    pub grpc_pool: Option<&'a Arc<GrpcClientPool>>,
    /// Optional attachment IDs for local storage when message is queued during bootstrap.
    pub attachment_ids: Option<Vec<AttachmentId>>,
    /// Optional channel ID for local storage.
    pub channel_id: Option<ChannelId>,
}

/// Parameters for sending a DM message with attachments.
pub struct SendDmMsgParams<'a> {
    /// The peer's user ID.
    pub peer_id: &'a UserId,
    /// Message text.
    pub text: &'a str,
    /// Attachment pointers for the proto message.
    pub attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
    /// Local attachment IDs to store with the message.
    pub attachment_ids: Vec<AttachmentId>,
    /// The DM channel ID for local storage.
    pub dm_channel_id: &'a ChannelId,
    /// Database for conversation state.
    pub database: Arc<Database>,
    /// For sending to the peer's mailbox.
    pub mailbox_manager: &'a MailboxManager,
    /// Timestamp in nanoseconds (caller generates this for consistent UI/storage).
    pub sent_at: u64,
}

/// Message service for all message operations.
///
/// Orchestrates message sending, editing, and deletion by coordinating
/// network client and local storage.
pub struct MessageService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    message_repo: Arc<MessageRepository>,
    channel_repo: Arc<ChannelRepository>,
    signal_service: Arc<SignalService>,
    /// Group mailbox manager for NDSS '21 per-group mailboxes.
    group_mailbox_manager: Arc<GroupMailboxManager>,
    /// Sesame session manager for multi-session handling and message queueing.
    sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
}

impl MessageService {
    /// Create a new MessageService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        signal_service: Arc<SignalService>,
        group_mailbox_manager: Arc<GroupMailboxManager>,
        sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            message_repo,
            channel_repo,
            signal_service,
            group_mailbox_manager,
            sesame_manager,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Internal client helpers
    // ─────────────────────────────────────────────────────────────────────────────

    fn message_client(&self) -> MessageClient {
        MessageClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    fn channel_client(&self) -> crate::network::grpc::channel_client::ChannelClient {
        crate::network::grpc::channel_client::ChannelClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        )
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
        let channel = crate::services::channel::proto_to_domain_channel(&proto_channel)
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
    /// * `group_fanout` - Optional GroupFanout for NDSS '21 mailbox delivery
    /// * `channel_id` - Target channel ID
    /// * `text` - Message text content
    /// * `attachment_ids` - Optional attachment IDs
    /// * `thread_id` - Optional parent message ID for threading
    /// * `mentioned_integrations` - AI integrations mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    ///
    /// # Group Mailbox Delivery
    ///
    /// If `group_fanout` is provided and the channel has mailboxes configured,
    /// the message will be delivered to member mailboxes IN ADDITION to the
    /// channel-based delivery. This provides traffic analysis resistance per
    /// NDSS '21 paper.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        group_fanout: Option<&GroupFanout>,
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
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server
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

        // Also send via group mailboxes if available (NDSS '21 paper)
        if let Some(fanout) = group_fanout {
            if require_encryption {
                // Extract ciphertext from content for mailbox delivery
                if let Some(chat_v1::message::content::Type::Encrypted(ref encrypted)) = content_to_send.r#type {
                    self.send_via_group_mailboxes(fanout, channel_id, &encrypted.ciphertext, &sender_id)
                        .await?;
                }
            }
        }

        // Convert to domain message and preserve attachment IDs
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = attachment_ids;

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Send a message to a channel (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
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
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send control content to a group channel (typing indicators, reactions, etc.)
    ///
    /// Uses sender key encryption for E2E privacy. When `group_fanout` is provided,
    /// also sends via member mailboxes for NDSS '21 traffic analysis resistance.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Message client for sending
    /// * `auth_client` - Auth client for key fetching and SKDM distribution
    /// * `channel_client` - Channel client for encryption settings
    /// * `channel_id` - Target channel
    /// * `content` - The Content proto to send (TypingMessage, Reaction, etc.)
    /// * `group_fanout` - Optional GroupFanout for mailbox delivery (NDSS '21)
    pub async fn send_control_content(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        use prost::Message as ProstMessage;

        // Check logged in
        let sender_id = self.current_user_id()?;

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
                    self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
                }

                // Serialize GroupEncrypted and wrap in EncryptedData
                let group_bytes = group_encrypted.encode_to_vec();
                let encrypted_content = chat_v1::message::Content {
                    r#type: Some(chat_v1::message::content::Type::Encrypted(
                        chat_v1::EncryptedData {
                            version: chat_v1::CryptoVersion::SignalProtocol as i32,
                            ciphertext: group_bytes.clone(),
                            associated_data: b"sender_key_control".to_vec(),
                            nonce: vec![],
                            auth_tag: vec![],
                            signature: vec![],
                            device_id: String::new(),
                        }
                    )),
                };

                // Send as ephemeral message via channel (fallback/legacy)
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

                // Also send via group mailboxes if available (NDSS '21 compliance)
                if let Some(fanout) = group_fanout {
                    // Use send_via_group_mailboxes for privacy-preserving delivery
                    if let Err(e) = self.send_via_group_mailboxes(
                        fanout,
                        channel_id,
                        &group_bytes,
                        &sender_id,
                    ).await {
                        tracing::debug!(
                            channel_id = %channel_id,
                            error = %e,
                            "Group mailbox delivery failed for control content (channel send succeeded)"
                        );
                    }
                }

                tracing::debug!(
                    channel_id = %channel_id,
                    has_mailbox_fanout = group_fanout.is_some(),
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

    /// Send control content to a group channel (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
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

    /// Send a GroupMailboxAddress announcement to a channel.
    ///
    /// Wraps the announcement in a Content proto and sends it via `send_control_content`.
    /// This is used after creating/joining a channel to announce our group mailbox
    /// for the NDSS '21 ephemeral mailbox routing.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Message client for sending
    /// * `auth_client` - Auth client for key fetching
    /// * `channel_client` - Channel client for encryption settings
    /// * `channel_id` - Target channel
    /// * `announcement` - The GroupMailboxAddress proto to send
    pub async fn send_group_mailbox_announcement(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut crate::network::grpc::channel_client::ChannelClient,
        channel_id: &ChannelId,
        announcement: crate::proto::content_v1::GroupMailboxAddress,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{content, Content};

        let content = Content {
            content: Some(content::Content::GroupMailboxAddress(announcement)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        // No group fanout for mailbox announcements - they bootstrap the process
        self.send_control_content(message_client, auth_client, channel_client, channel_id, content, None)
            .await
    }

    /// Send a GroupMailboxAddress announcement (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    pub async fn announce_group_mailbox(
        &self,
        channel_id: &ChannelId,
        announcement: crate::proto::content_v1::GroupMailboxAddress,
    ) -> Result<(), MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_group_mailbox_announcement(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            announcement,
        )
        .await
    }

    // ==================== DM Operations ====================

    /// Send a Content proto to a DM peer via two-layer encryption.
    ///
    /// Uses two encryption layers for privacy:
    /// 1. Signal Protocol (forward secrecy via Double Ratchet)
    /// 2. Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
    pub async fn send_dm_content(
        &self,
        params: SendDmContentParams<'_>,
    ) -> Result<(), MessageError> {
        use prost::Message as ProstMessage;

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
            // Check if this is an ephemeral message (typing indicator, receipt) that should be dropped
            // These don't make sense to queue - they're time-sensitive and only relevant in real-time
            let is_ephemeral = matches!(
                &content.content,
                Some(crate::proto::content_v1::content::Content::Typing(_)) |
                Some(crate::proto::content_v1::content::Content::Receipt(_))
            );

            if is_ephemeral {
                tracing::debug!(
                    peer_id = %peer_id,
                    "Dropping ephemeral message (typing/receipt) - bootstrap not complete"
                );
                return Err(MessageError::BootstrapInitiated);
            }

            // Get our user ID for Sesame deterministic initiator selection
            let our_user_id = self.current_user_id()?;

            // Sesame algorithm: lower UUID initiates bootstrap, higher UUID waits
            // This prevents race conditions where both sides try to initiate simultaneously
            // Reference: "Formal Security Analysis of the Signal Protocol" - sorting identity keys
            let we_are_initiator = our_user_id.as_uuid() < peer_id.as_uuid();

            if !we_are_initiator {
                // Acquire bootstrap lock EARLY to prevent concurrent REQUEST sends
                // This must be the first thing we do to prevent race conditions
                if !try_acquire_bootstrap_lock(peer_id) {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "BOOTSTRAP[1/4] Skipping - bootstrap already in progress for this peer"
                    );
                    // Still queue the message even if we skip the REQUEST
                    let content_bytes = content.encode_to_vec();
                    let _ = self.sesame_manager.queue_pending_message(
                        peer_id,
                        content_bytes,
                        attachment_ids.clone().unwrap_or_default(),
                        channel_id,
                    );
                    return Err(MessageError::BootstrapInitiated);
                }

                // We're the higher UUID - we should wait for peer to initiate
                // Queue the message so it will be sent once bootstrap completes
                let content_bytes = content.encode_to_vec();
                if let Err(e) = self.sesame_manager.queue_pending_message(
                    peer_id,
                    content_bytes,
                    attachment_ids.clone().unwrap_or_default(),
                    channel_id,
                ) {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "Sesame: Failed to queue message while waiting for peer bootstrap"
                    );
                } else {
                    tracing::info!(
                        peer_id = %peer_id,
                        "Sesame: Message queued - will send after peer initiates bootstrap"
                    );
                }

                // Check if we already received their bootstrap (conversation state might be pending)
                let already_pending = conversation.as_ref()
                    .map(|c| c.our_mailbox.is_some() || c.peer_mailbox.is_some())
                    .unwrap_or(false);

                if already_pending {
                    tracing::info!(
                        peer_id = %peer_id,
                        our_id = %our_user_id,
                        "Sesame: Bootstrap pending from peer (we're higher UUID) - message queued"
                    );
                    release_bootstrap_lock(peer_id);
                    return Err(MessageError::BootstrapInitiated);
                }

                // Not pending - send a bootstrap REQUEST to prompt peer (lower UUID) to initiate
                // This uses a KeyExchange with empty ephemeral_pk (0 bytes) as the signal
                // Reference: Deterministic initiator selection from "Formal Security Analysis of Signal"
                if let Some(grpc_pool) = grpc_pool {

                    tracing::info!(
                        peer_id = %peer_id,
                        our_id = %our_user_id,
                        our_uuid = %our_user_id,
                        peer_uuid = %peer_id,
                        "BOOTSTRAP[1/4] Higher UUID sending REQUEST to lower UUID peer"
                    );

                    // Build bootstrap request: KeyExchange with empty ephemeral_pk
                    let bootstrap_request = crate::proto::content_v1::KeyExchange {
                        ephemeral_pk: vec![], // 0 bytes = bootstrap REQUEST
                        conversation_id: our_user_id.to_string(),
                        timestamp: TimestampNanos::now().as_u64(),
                    };

                    let request_content = crate::proto::content_v1::Content {
                        content: Some(crate::proto::content_v1::content::Content::KeyExchange(bootstrap_request)),
                        ephemeral_reply_to: None,
                        timestamp: TimestampNanos::now().as_u64(),
                        padding: vec![],
                    };

                    let request_bytes = request_content.encode_to_vec();

                    // Encrypt and send to peer's device inbox
                    let peer_device_id = 1u32;
                    let peer_id_str = peer_id.to_string();

                    match self.signal_service.encrypt_message(
                        auth_client,
                        &peer_id_str,
                        peer_device_id,
                        &request_bytes,
                    ).await {
                        Ok(sealed_ciphertext) => {
                            // Get anonymous credential for the request - request tokens if needed
                            let token_manager = TokenManager::new(database.clone());
                            let token = match token_manager.get_available_token() {
                                Ok(Some(t)) => Some(t),
                                Ok(None) => {
                                    // No tokens - request more
                                    tracing::info!(
                                        peer_id = %peer_id,
                                        "No tokens for bootstrap request, requesting from server"
                                    );
                                    let mut mailbox_client = MailboxClient::new(
                                        grpc_pool.channel(),
                                        grpc_pool.auth_clone(),
                                    );
                                    match token_manager.request_tokens(&mut mailbox_client, 5).await {
                                        Ok(count) if count > 0 => {
                                            token_manager.get_available_token().ok().flatten()
                                        }
                                        Ok(_) => {
                                            tracing::warn!(peer_id = %peer_id, "No tokens received from server");
                                            None
                                        }
                                        Err(e) => {
                                            tracing::warn!(peer_id = %peer_id, error = %e, "Failed to request tokens");
                                            None
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(peer_id = %peer_id, error = %e, "Failed to check tokens");
                                    None
                                }
                            };

                            if let Some(token) = token {
                                tracing::debug!(
                                    peer_id = %peer_id,
                                    token_id = %token.id,
                                    "BOOTSTRAP[1/4] Got token, sending REQUEST to peer's device inbox"
                                );

                                let mut message_client = MessageClient::new(
                                    grpc_pool.channel(),
                                    grpc_pool.auth_clone(),
                                );

                                let sealed_msg = SealedSenderMessage {
                                    sealed_body: sealed_ciphertext.clone(),
                                    server_key_id: 1,
                                    timestamp: TimestampNanos::now().as_u64(),
                                };

                                tracing::debug!(
                                    peer_id = %peer_id,
                                    sealed_body_len = sealed_ciphertext.len(),
                                    "BOOTSTRAP[1/4] Sending sealed REQUEST (0-byte KeyExchange)"
                                );

                                if let Err(e) = message_client.send_sealed_message_with_credential(
                                    &peer_id_str,
                                    peer_device_id,
                                    sealed_msg,
                                    &token.public_key,
                                    &token.signature,
                                    token.randomizer.as_deref(),
                                ).await {
                                    tracing::error!(
                                        peer_id = %peer_id,
                                        error = %e,
                                        "BOOTSTRAP[1/4] FAILED to send REQUEST - peer won't receive prompt"
                                    );
                                } else {
                                    let _ = token_manager.mark_token_used(&token.id);
                                    tracing::info!(
                                        peer_id = %peer_id,
                                        "BOOTSTRAP[1/4] SUCCESS - REQUEST sent to peer's device inbox"
                                    );
                                }
                            } else {
                                tracing::error!(
                                    peer_id = %peer_id,
                                    "BOOTSTRAP[1/4] FAILED - No anonymous tokens available"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer_id = %peer_id,
                                error = %e,
                                "Failed to encrypt bootstrap request"
                            );
                        }
                    }

                    // DON'T release lock here - keep it held until peer's bootstrap MESSAGE arrives
                    // dm_bootstrap_handler will release it when processing the 64-byte MESSAGE
                    tracing::debug!(
                        peer_id = %peer_id,
                        "BOOTSTRAP[1/4] Lock held - will be released when peer's MESSAGE arrives"
                    );
                } else {
                    tracing::info!(
                        peer_id = %peer_id,
                        our_id = %our_user_id,
                        "Sesame: No gRPC pool for bootstrap request - message queued, waiting for peer"
                    );
                    // Release lock when no grpc_pool (can't send REQUEST anyway)
                    release_bootstrap_lock(peer_id);
                }

                return Err(MessageError::BootstrapInitiated);
            }

            // We're the lower UUID - we are the rightful initiator
            tracing::info!(
                peer_id = %peer_id,
                our_id = %our_user_id,
                "Sesame: We're lower UUID - initiating DM bootstrap"
            );

            // Need grpc_pool for bootstrap
            tracing::info!(peer_id = %peer_id, "Checking grpc_pool availability");
            let grpc_pool = grpc_pool.ok_or_else(|| {
                tracing::error!(peer_id = %peer_id, "No gRPC pool for bootstrap");
                MessageError::DmError("gRPC pool required for DM bootstrap".into())
            })?;
            tracing::info!(peer_id = %peer_id, "grpc_pool is available");

            tracing::info!(peer_id = %peer_id, "Creating DmBootstrap handler");
            let dm_bootstrap = DmBootstrap::new(database.clone());
            let mut mailbox_client =
                MailboxClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            tracing::info!(peer_id = %peer_id, "Calling dm_bootstrap.initiate_dm()");
            match dm_bootstrap.initiate_dm(*peer_id, &mut mailbox_client).await {
                Ok(bootstrap_result) => {
                    tracing::debug!(peer_id = %peer_id, "Bootstrap result received, encrypting for sealed sender");
                    // Send bootstrap message via sealed sender (device inbox)
                    let peer_device_id = 1u32;
                    let peer_id_str = peer_id.to_string();

                    tracing::debug!(
                        peer_id = %peer_id,
                        peer_device_id,
                        message_len = bootstrap_result.bootstrap_message.len(),
                        "Encrypting bootstrap message via Signal"
                    );
                    match self
                        .signal_service
                        .encrypt_message(
                            auth_client,
                            &peer_id_str,
                            peer_device_id,
                            &bootstrap_result.bootstrap_message,
                        )
                        .await
                    {
                        Ok(sealed_ciphertext) => {
                            tracing::debug!(
                                peer_id = %peer_id,
                                ciphertext_len = sealed_ciphertext.len(),
                                "Bootstrap encrypted, acquiring anonymous credential"
                            );

                            // Get anonymous credential for initial contact (NDSS '21)
                            // Per NDSS '21 paper, we use a blind-signed token to prove sender legitimacy
                            // without revealing identity. This prevents spam while preserving anonymity.
                            let token_manager = TokenManager::new(database.clone());

                            let credential_token = match token_manager.get_available_token() {
                                Ok(Some(token)) => token,
                                Ok(None) => {
                                    // No tokens available - request new ones from server
                                    tracing::info!(peer_id = %peer_id, "No tokens available, requesting from server");
                                    let mut mailbox_client = MailboxClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
                                    if let Err(e) = token_manager.request_tokens(&mut mailbox_client, 5).await {
                                        tracing::error!(peer_id = %peer_id, error = %e, "Failed to request anonymous tokens");
                                        return Err(MessageError::DmError(format!("Failed to get anonymous token: {}", e)));
                                    }

                                    // Try again to get a token
                                    match token_manager.get_available_token() {
                                        Ok(Some(t)) => t,
                                        Ok(None) => {
                                            return Err(MessageError::DmError("No tokens available after request".into()));
                                        }
                                        Err(e) => {
                                            return Err(MessageError::DmError(format!("Failed to get token: {}", e)));
                                        }
                                    }
                                }
                                Err(e) => {
                                    return Err(MessageError::DmError(format!("Failed to get anonymous token: {}", e)));
                                }
                            };

                            tracing::debug!(
                                peer_id = %peer_id,
                                token_id = %credential_token.id,
                                "Got anonymous credential, sending to device inbox"
                            );

                            // Send to peer's device inbox via MessageClient
                            let mut message_client =
                                MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                            let sealed_msg = SealedSenderMessage {
                                sealed_body: sealed_ciphertext,
                                server_key_id: 1, // Default server key
                                timestamp: TimestampNanos::now().as_u64(),
                            };

                            // Use anonymous credential (blind-signed token) for initial contact
                            // Server validates the signature to prove sender is legitimate, but doesn't know WHO
                            if let Err(e) = message_client
                                .send_sealed_message_with_credential(
                                    &peer_id_str,
                                    peer_device_id,
                                    sealed_msg,
                                    &credential_token.public_key,
                                    &credential_token.signature,
                                    credential_token.randomizer.as_deref(),
                                )
                                .await
                            {
                                tracing::error!(peer_id = %peer_id, error = %e, "Failed to send bootstrap to device inbox");
                                return Err(MessageError::DmError(format!(
                                    "Failed to send bootstrap: {}",
                                    e
                                )));
                            }

                            // Mark token as used (it's now spent for this purpose)
                            if let Err(e) = token_manager.mark_token_used(&credential_token.id) {
                                tracing::warn!(
                                    peer_id = %peer_id,
                                    token_id = %credential_token.id,
                                    error = %e,
                                    "Failed to mark anonymous token as used"
                                );
                            }

                            tracing::info!(
                                peer_id = %peer_id,
                                "DM bootstrap sent successfully - waiting for key exchange response"
                            );

                            // Queue the original message to be sent once bootstrap completes
                            // Sesame algorithm: pending messages are delivered after session establishment
                            let content_bytes = content.encode_to_vec();
                            if let Err(e) = self.sesame_manager.queue_pending_message(
                                peer_id,
                                content_bytes,
                                attachment_ids.clone().unwrap_or_default(),
                                channel_id,
                            ) {
                                tracing::warn!(
                                    peer_id = %peer_id,
                                    error = %e,
                                    "Failed to queue message for post-bootstrap delivery"
                                );
                            } else {
                                tracing::info!(
                                    peer_id = %peer_id,
                                    "Message queued for delivery after bootstrap completes"
                                );
                            }

                            return Err(MessageError::BootstrapInitiated);
                        }
                        Err(e) => {
                            tracing::error!(peer_id = %peer_id, error = %e, "Failed to encrypt bootstrap message");
                            return Err(MessageError::EncryptionFailed(format!(
                                "Bootstrap encrypt: {}",
                                e
                            )));
                        }
                    }
                }
                Err(crate::error::MailboxError::ConversationExists) => {
                    // Conversation exists but not ready - waiting for key exchange
                    // Queue the message to be sent once bootstrap completes
                    let content_bytes = content.encode_to_vec();
                    if let Err(e) = self.sesame_manager.queue_pending_message(
                        peer_id,
                        content_bytes,
                        attachment_ids.clone().unwrap_or_default(),
                        channel_id,
                    ) {
                        tracing::warn!(
                            peer_id = %peer_id,
                            error = %e,
                            "Failed to queue message while waiting for key exchange"
                        );
                    } else {
                        tracing::info!(
                            peer_id = %peer_id,
                            "Message queued - waiting for key exchange"
                        );
                    }
                    return Err(MessageError::BootstrapInitiated);
                }
                Err(e) => {
                    tracing::error!(peer_id = %peer_id, error = %e, "Failed to initiate DM bootstrap");
                    return Err(MessageError::DmError(format!("Bootstrap failed: {}", e)));
                }
            }
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

        // Store sent message in database for persistence (only for DataMessages)
        // Ephemeral messages (typing indicators, receipts) should not be stored
        tracing::info!(peer_id = %peer_id, "send_dm_content: checking if should store message");
        let our_user_id = self.current_user_id()?;
        let text = match &content.content {
            Some(crate::proto::content_v1::content::Content::DataMessage(dm)) => {
                tracing::info!(peer_id = %peer_id, text_len = dm.body.len(), "send_dm_content: DataMessage detected, will store");
                dm.body.clone()
            },
            _ => {
                // Not a DataMessage - don't store ephemeral messages (typing, receipts, etc.)
                tracing::info!(peer_id = %peer_id, "Skipping storage for non-DataMessage content");
                return Ok(());
            }
        };
        if let Ok(Some(dm_channel)) = self.channel_repo.get_dm_channel_by_peer(&our_user_id, peer_id) {

            // Use current timestamp for sent messages (Signal-style dedup)
            let sent_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let message = crate::domain::message::Message::new_text(
                dm_channel.id,
                Some(our_user_id),
                text,
                sent_at,
            );

            if let Err(e) = self.message_repo.insert(&message) {
                tracing::warn!(
                    peer_id = %peer_id,
                    channel_id = %dm_channel.id,
                    error = %e,
                    "Failed to store sent DM in database"
                );
            } else {
                tracing::debug!(
                    peer_id = %peer_id,
                    channel_id = %dm_channel.id,
                    message_id = %message.id,
                    "Stored sent DM in database"
                );
            }
        } else {
            tracing::warn!(
                peer_id = %peer_id,
                "Could not find DM channel to store sent message"
            );
        }

        Ok(())
    }

    /// Send a Content proto to a DM peer (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
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
    ///
    /// Convenience wrapper around `send_dm_content` that builds the Content proto.
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Auth client for key fetching
    /// * `peer_id` - The peer's user ID
    /// * `text` - Message text
    /// * `database` - Database for conversation state
    /// * `mailbox_manager` - For sending to the peer's mailbox
    /// * `grpc_pool` - gRPC pool for bootstrap (needed for first DM)
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
        tracing::debug!(peer_id = %peer_id, "MessageService: built text content, calling send_dm_content");
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

    /// Send a DM with text and/or attachments.
    ///
    /// Uses Signal Protocol encryption with sealed sender for privacy.
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Auth client for key fetching
    /// * `peer_id` - The peer's user ID
    /// * `text` - Message text (can be empty if attachments present)
    /// * `attachment_pointers` - Attachment pointers to include
    /// * `database` - Database for conversation state
    /// * `mailbox_manager` - For sending to the peer's mailbox
    /// * `grpc_pool` - gRPC pool for bootstrap (needed for first DM)
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
        tracing::debug!(peer_id = %peer_id, "MessageService: built data message content, calling send_dm_content");
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
    ///
    /// This is the preferred method - uses internal gRPC clients.
    /// Stores the sent message locally and returns it.
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

        // Use caller-provided timestamp for proto AND storage (ensures UI matches DB)
        let content = crate::services::build_data_message_content(text, attachment_pointers, sent_at);

        // Send the message via Signal Protocol
        let send_result = self.send_dm(peer_id, content, database.clone(), mailbox_manager).await;

        // Create the message for local storage
        let current_user_id = self.current_user_id()?;
        let mut message = Message::new_text(*dm_channel_id, Some(current_user_id), text.to_string(), sent_at);
        message.attachments = attachment_ids;

        // Handle BootstrapInitiated specially - save message to DB before returning error
        // This ensures queued messages survive UI reloads (which fetch from DB)
        if let Err(MessageError::BootstrapInitiated) = &send_result {
            // Save to local database even though message is queued for later sending
            if let Err(e) = self.message_repo.insert(&message) {
                tracing::warn!(
                    message_id = %message.id,
                    error = %e,
                    "Failed to store queued DM message locally"
                );
            } else {
                tracing::info!(
                    message_id = %message.id,
                    channel_id = %dm_channel_id,
                    attachment_count = message.attachments.len(),
                    "Stored queued DM message in local database (bootstrap pending)"
                );
            }
            return Err(MessageError::BootstrapInitiated);
        }

        // Propagate other errors
        send_result?;

        // Store in local database (normal send succeeded)
        if let Err(e) = self.message_repo.insert(&message) {
            tracing::warn!(
                message_id = %message.id,
                error = %e,
                "Failed to store sent DM message locally"
            );
        } else {
            tracing::info!(
                message_id = %message.id,
                channel_id = %dm_channel_id,
                attachment_count = message.attachments.len(),
                "Stored sent DM message in local database"
            );
        }

        Ok(message)
    }

    /// Send all pending messages for a peer after bootstrap completes.
    ///
    /// Called from the inbox event handler when DmBootstrapCompleted is received.
    /// Drains the Sesame pending message queue and sends each message.
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Auth client for key fetching
    /// * `peer_id` - The peer whose bootstrap completed
    /// * `database` - Database for conversation state
    /// * `mailbox_manager` - For sending to the peer's mailbox
    ///
    /// # Returns
    ///
    /// Number of messages successfully sent, or error.
    pub async fn send_pending_messages(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        database: Arc<Database>,
        mailbox_manager: &MailboxManager,
    ) -> Result<usize, MessageError> {
        use prost::Message as ProstMessage;

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

            // Use the ORIGINAL timestamp from the proto (NOT a new one!)
            // This ensures the stored message matches the sent message timestamp
            let original_sent_at = content.timestamp;

            // Send the message (no grpc_pool needed - conversation should be ready)
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
                    // Message was already saved to DB in send_dm_msg when BootstrapInitiated was returned
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "PENDING_MESSAGES: FAILED to send message"
                    );
                    // Continue with other messages
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
    ///
    /// This is called when session convergence fails and we need to
    /// start fresh with the peer. It:
    /// 1. Archives all existing sessions with the peer
    /// 2. Marks the peer as stale (needs device refresh)
    /// 3. Attempts to establish a new session by fetching their prekey bundle
    ///
    /// After calling this, subsequent messages will use the new session.
    pub async fn reset_and_establish_session(
        &self,
        auth_client: &mut crate::network::grpc::auth_client::AuthClient,
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

        // For sent messages, store original plaintext locally (not encrypted server response)
        // Forward secrecy deletes keys after encryption, so we can't decrypt our own ciphertext
        message.content = MessageContent::Text(trimmed_text.to_string());

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
            self.distribute_sender_key(auth_client, message_client, target_channel_id, &skdm).await?;
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

        // For sent messages, store original plaintext locally (not encrypted server response)
        // Forward secrecy deletes keys after encryption, so we can't decrypt our own ciphertext
        message.content = MessageContent::Text(forwarded_text);

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
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
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

        // For sent messages, store original plaintext locally (not encrypted server response)
        // Forward secrecy deletes keys after encryption, so we can't decrypt our own ciphertext
        message.content = MessageContent::Text(trimmed_text.to_string());

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
    /// Implements NDSS '21 pairwise distribution:
    /// - Each member receives SKDM + our mailbox announcement
    /// - Encrypted pairwise via Double Ratchet (Sealed Sender envelope)
    /// - Sent to member's device inbox (server sees sender but not group correlation)
    ///
    /// This should be called:
    /// - After creating a channel (distribute to initial members)
    /// - After joining a channel (distribute to existing members)
    /// - After a member joins (existing members distribute to new member)
    ///
    /// # Arguments
    ///
    /// * `auth_client` - For session establishment (X3DH if needed)
    /// * `message_client` - For sending sealed messages
    /// * `channel_id` - The group channel
    /// * `skdm` - The sender key distribution message to distribute
    pub async fn distribute_sender_key(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        // Get our user ID
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Get channel members (excluding ourselves)
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        let other_members: Vec<_> = members
            .iter()
            .filter(|m| m.user_id.to_string() != our_user_id)
            .collect();

        if other_members.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping sender key distribution (no other members)"
            );
            return Ok(());
        }

        // Get our mailbox public key for this channel (if available)
        let ephemeral_reply_to: Option<Vec<u8>> = self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec());
        let has_mailbox = ephemeral_reply_to.is_some();

        // Build the Content proto with SKDM + our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();

        let skdm_content = content_v1::Content {
            ephemeral_reply_to,
            timestamp,
            padding: vec![], // TODO: Add proper padding
            content: Some(content::Content::SenderKey(skdm.clone())),
        };

        let plaintext = skdm_content.encode_to_vec();

        // Distribute to each member pairwise via Sealed Sender
        let mut success_count = 0;
        let mut error_count = 0;

        for member in &other_members {
            let member_id = member.user_id.to_string();

            // Encrypt pairwise via Double Ratchet (X3DH if no session exists)
            // Device ID 1 is the primary device; ideally we'd send to all devices
            match self.signal_service
                .encrypt_message(auth_client, &member_id, 1, &plaintext)
                .await
            {
                Ok(sealed_bytes) => {
                    // Create SealedSenderMessage proto
                    let sealed_message = SealedSenderMessage {
                        sealed_body: sealed_bytes,
                        server_key_id: 0, // Server will use current key
                        timestamp,
                    };

                    // Send to member's device inbox
                    match message_client
                        .send_sealed_message(&member_id, 0, sealed_message) // 0 = all devices
                        .await
                    {
                        Ok(_) => {
                            success_count += 1;
                            tracing::debug!(
                                channel_id = %channel_id,
                                member_id = %member_id,
                                "Distributed SKDM to member via sealed sender"
                            );
                        }
                        Err(e) => {
                            error_count += 1;
                            tracing::warn!(
                                channel_id = %channel_id,
                                member_id = %member_id,
                                error = %e,
                                "Failed to send SKDM to member"
                            );
                        }
                    }
                }
                Err(e) => {
                    error_count += 1;
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to encrypt SKDM for member"
                    );
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            total_members = other_members.len(),
            success = success_count,
            errors = error_count,
            has_mailbox = has_mailbox,
            "Distributed sender key via pairwise sealed sender"
        );

        // Consider it success if at least one member received the SKDM
        if success_count == 0 && !other_members.is_empty() {
            return Err(MessageError::EncryptionFailed(
                "Failed to distribute SKDM to any member".into()
            ));
        }

        Ok(())
    }

    /// Distribute sender key to channel members (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    pub async fn distribute_skdm(
        &self,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.distribute_sender_key(&mut auth_client, &mut message_client, channel_id, skdm)
            .await
    }

    /// Send encrypted content to group member mailboxes via GroupFanout.
    ///
    /// This implements the NDSS '21 paper's per-group ephemeral mailboxes for
    /// privacy-preserving group messaging. Messages are fan-out to N member
    /// mailboxes, preventing server-side correlation of group membership.
    ///
    /// # Arguments
    ///
    /// * `group_fanout` - GroupFanout for N-way parallel delivery
    /// * `channel_id` - Target channel
    /// * `encrypted_content` - Pre-encrypted content (sender key ciphertext)
    /// * `sender_id` - Our user ID for associated_data
    ///
    /// # Note
    ///
    /// This is called IN ADDITION to channel-based sending during the transition
    /// period. Once all members have migrated to mailbox-based receiving, the
    /// channel send can be removed.
    pub async fn send_via_group_mailboxes(
        &self,
        group_fanout: &GroupFanout,
        channel_id: &ChannelId,
        encrypted_content: &[u8],
        sender_id: &UserId,
    ) -> Result<(), MessageError> {
        // Check if group mailboxes are ready
        match group_fanout.is_ready_to_send(channel_id) {
            Ok(true) => {
                // Build associated_data with sender info for recipient to extract
                let associated_data = format!("sender:{}:channel:{}", sender_id, channel_id);

                // Wrap in EncryptedData for mailbox delivery
                use prost::Message as ProstMessage;
                let mailbox_envelope = chat_v1::EncryptedData {
                    version: chat_v1::CryptoVersion::SignalProtocol as i32,
                    ciphertext: encrypted_content.to_vec(),
                    associated_data: associated_data.into_bytes(),
                    nonce: vec![],
                    auth_tag: vec![],
                    signature: vec![],
                    device_id: String::new(),
                };
                let envelope_bytes = mailbox_envelope.encode_to_vec();

                // Fan-out to all member mailboxes
                match group_fanout
                    .send_to_group(channel_id, &envelope_bytes, EnvelopeType::Message)
                    .await
                {
                    Ok(result) => {
                        if result.is_complete_success() {
                            tracing::debug!(
                                channel_id = %channel_id,
                                total = result.total_members,
                                "Sent message to all group mailboxes"
                            );
                        } else {
                            tracing::warn!(
                                channel_id = %channel_id,
                                successful = result.successful,
                                failed = result.failed,
                                "Partial delivery to group mailboxes"
                            );
                        }
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel_id,
                            error = %e,
                            "Failed to send to group mailboxes (channel delivery still succeeded)"
                        );
                        // Don't fail - channel delivery already succeeded
                        Ok(())
                    }
                }
            }
            Ok(false) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    "Group mailboxes not ready - skipping fanout"
                );
                Ok(())
            }
            Err(e) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %e,
                    "Could not check group mailbox readiness - skipping fanout"
                );
                Ok(())
            }
        }
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
        let channel_id = ChannelId::from(sender_id); // SavedMessages channel_id = user_id

        // Generate timestamp ONCE - used everywhere
        let sent_at = TimestampNanos::now().as_u64();
        let mut message = Message::new_text(channel_id, Some(sender_id), trimmed_text.to_string(), sent_at);
        message.attachments = attachment_ids;

        // Store locally
        self.message_repo.insert(&message)?;

        if device_count <= 1 {
            // Only this device - we're done (local-only storage)
            tracing::info!(
                "SavedMessages: stored locally only (single device), message_id={}",
                message.id
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
                message.id
            );
        }

        Ok(message)
    }

    /// Send a message to SavedMessages (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    pub async fn save_message(
        &self,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
    ) -> Result<Message, MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.send_saved_message(&mut auth_client, &mut message_client, text, attachment_ids)
            .await
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

    // Convert timestamp (proto uses nanos)
    let created_at = TimestampNanos::from_nanos(proto.timestamp).to_datetime().unwrap_or_else(Utc::now);
    let updated_at = if proto.updated_at > 0 {
        TimestampNanos::from_nanos(proto.updated_at).to_datetime().unwrap_or(created_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        TimestampNanos::from_nanos(proto.edited_at).to_datetime()
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
        sent_at: proto.timestamp,
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

    // Convert timestamp (proto uses nanos)
    let created_at = TimestampNanos::from_nanos(proto.timestamp).to_datetime().unwrap_or_else(Utc::now);
    let updated_at = if proto.updated_at > 0 {
        TimestampNanos::from_nanos(proto.updated_at).to_datetime().unwrap_or(created_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        TimestampNanos::from_nanos(proto.edited_at).to_datetime()
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
        sent_at: proto.timestamp,
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
    fn test_nanos_to_datetime() {
        let nanos = 1706140800_000_000_000u64; // 2024-01-25 00:00:00 UTC in nanoseconds
        let dt = TimestampNanos::from_nanos(nanos).to_datetime().unwrap();
        assert_eq!(dt.timestamp_nanos_opt().unwrap(), nanos as i64);
    }

    #[test]
    fn test_proto_content_to_domain_encrypted() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        // Encrypted content should return placeholder text
        let proto_content = Content {
            r#type: Some(Type::Encrypted(chat_v1::EncryptedData {
                version: 1,
                nonce: vec![0x12, 0x34, 0x56, 0x78],
                ciphertext: vec![0xDE, 0xAD, 0xBE, 0xEF],
                auth_tag: vec![0xAA, 0xBB],
                associated_data: vec![],
                signature: vec![],
                device_id: "device-1".to_string(),
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => {
                assert_eq!(text, "[Encrypted message]");
            }
            _ => panic!("Expected Text variant with placeholder"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_file() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let file_id = AttachmentId::new();
        let proto_content = Content {
            r#type: Some(Type::File(chat_v1::FileMetadata {
                id: file_id.to_string(),
                name: "test.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: 1024,
                sha256: vec![0xAB, 0xCD, 0xEF], // Vec<u8>, not String
                chunk_size: 256,
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Mixed(mixed) => {
                assert!(mixed.text.is_none());
                assert_eq!(mixed.attachment_ids.len(), 1);
                assert_eq!(mixed.attachment_ids[0], file_id);
            }
            _ => panic!("Expected Mixed variant for file content"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_empty_type() {
        use chat_v1::message::Content;

        // Content with no type should return empty text
        let proto_content = Content { r#type: None };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => {
                assert!(text.is_empty());
            }
            _ => panic!("Expected empty Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_reaction() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        // Reaction content should return placeholder
        let proto_content = Content {
            r#type: Some(Type::Reaction(chat_v1::ReactionUpdate {
                message_id: "msg-123".to_string(),
                emoji: "👍".to_string(),
                add: true,
                proof_of_work: vec![],
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => {
                assert_eq!(text, "[Special message]");
            }
            _ => panic!("Expected Text variant with placeholder"),
        }
    }

    #[test]
    fn test_file_id_parsing_error() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        // Invalid file ID should return error
        let proto_content = Content {
            r#type: Some(Type::File(chat_v1::FileMetadata {
                id: "not-a-valid-uuid".to_string(),
                name: "test.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: 1024,
                sha256: vec![0xAB, 0xCD],
                chunk_size: 256,
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content));
        assert!(result.is_err());
    }
}
