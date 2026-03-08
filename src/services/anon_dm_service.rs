//! Anonymous DM service.
//!
//! Client-side managed DMs for public accounts where strangers can message
//! without friend requests. Server has no knowledge of these conversations.
//!
//! # Architecture
//!
//! Similar to anonymous groups (NDSS'21 style) but for 1:1 conversations:
//!
//! - **Message Requests**: Sent to recipient's device inbox (they need to accept)
//! - **Messages**: Encrypted with Signal Protocol, delivered to anonymous mailbox
//!
//! Each participant has their own anonymous mailbox for the conversation,
//! registered using blind signature tokens.

use std::sync::Arc;

use chrono::Utc;
use prost::Message as ProstMessage;
use rand::RngCore;
use uuid::Uuid;

use crate::domain::anon_dm::{
    AnonDm, AnonDmId, AnonDmMailbox, AnonDmMessage as DomainAnonDmMessage,
    AnonDmMessageId, AnonDmStatus,
};
use crate::domain::private_friend_code::PrivateFriendCode;
use crate::domain::UserId;
use crate::error::StorageError;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::network::grpc::anonymous_client::AnonymousMessageClient;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::proto::content_v1::{self, AnonDmAccept, AnonDmMessage, AnonDmRequest, Content};
use crate::services::gateway::builders::{
    build_typing_content, build_reaction_content, build_read_receipt_content,
    build_delete_for_everyone_content, build_pin_content, build_edit_content,
};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::pairwise::PairwiseService;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::SettingsRepository;
use crate::services::AttachmentService;
use crate::util::time::TimestampNanos;

/// Anonymous DM service errors.
#[derive(Debug, thiserror::Error)]
pub enum AnonDmError {
    /// Not logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Conversation not found.
    #[error("conversation not found: {0}")]
    ConversationNotFound(AnonDmId),

    /// Conversation not ready (not accepted yet).
    #[error("conversation not ready: {0}")]
    ConversationNotReady(AnonDmId),

    /// Cannot message self.
    #[error("cannot send message to yourself")]
    CannotMessageSelf,

    /// Conversation already exists with this peer.
    #[error("conversation already exists with peer: {0}")]
    ConversationExists(UserId),

    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Current user error.
    #[error("current user error: {0}")]
    CurrentUser(#[from] crate::services::current_user::CurrentUserError),

    /// Message send error.
    #[error("message error: {0}")]
    MessageError(String),

    /// Crypto error.
    #[error("crypto error: {0}")]
    CryptoError(String),

    /// Invalid data.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Rate limit exceeded.
    #[error("rate limit exceeded: {0} messages per minute")]
    RateLimitExceeded(u32),

    /// Conversation is blocked.
    #[error("conversation is blocked: {0}")]
    ConversationBlocked(AnonDmId),

    /// User is blocked globally.
    #[error("user is blocked: {0}")]
    UserBlocked(UserId),

    /// Invalid private friend code.
    #[error("invalid private friend code: {0}")]
    InvalidFriendCode(String),
}

/// Anonymous DM service.
///
/// Handles all anonymous DM operations following the NDSS'21 model.
pub struct AnonDmService {
    settings_repo: Arc<SettingsRepository>,
    anon_dm_repo: Arc<AnonDmRepository>,
    /// Attachment storage for incoming file metadata.
    attachment_repo: Arc<AttachmentRepository>,
    /// Reusable Signal Protocol pairwise encryption.
    pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    mailbox_manager: Arc<MailboxManager>,
    grpc_pool: Arc<GrpcClientPool>,
    token_manager: Arc<TokenManager>,
}

impl AnonDmService {
    /// Create a new AnonDmService.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        anon_dm_repo: Arc<AnonDmRepository>,
        attachment_repo: Arc<AttachmentRepository>,
        pairwise_service: Arc<PairwiseService>,
        mailbox_manager: Arc<MailboxManager>,
        grpc_pool: Arc<GrpcClientPool>,
        token_manager: Arc<TokenManager>,
    ) -> Self {
        Self {
            settings_repo,
            anon_dm_repo,
            attachment_repo,
            pairwise_service,
            mailbox_manager,
            grpc_pool,
            token_manager,
        }
    }

    /// Get the current user ID.
    fn current_user_id(&self) -> Result<UserId, AnonDmError> {
        Ok(self.settings_repo.current_user_id()?)
    }

    /// Get our profile key if available.
    fn get_my_profile_key(&self) -> Option<[u8; 32]> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|v| {
                if v.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&v);
                    Some(arr)
                } else {
                    None
                }
            })
    }

    /// Get our display name from settings.
    fn get_my_display_name(&self) -> Option<String> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_string(settings_keys::DISPLAY_NAME)
            .ok()
            .flatten()
    }

    /// Get our avatar bytes from settings (stored locally, not on server).
    fn get_my_avatar(&self) -> Option<Vec<u8>> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }

    /// Get encrypted avatar for sharing during bootstrap.
    ///
    /// Encrypts avatar with profile key for privacy-preserving sharing.
    fn get_encrypted_avatar(&self, profile_key: &[u8; 32]) -> Option<Vec<u8>> {
        let avatar = self.get_my_avatar()?;
        crate::crypto::profile_crypto::encrypt_avatar(profile_key, &avatar).ok()
    }

    // ========================================================================
    // Initiation (Sender Side)
    // ========================================================================

    /// Start a new anonymous DM conversation with a stranger.
    ///
    /// Creates a local conversation and sends a message request to the peer's
    /// device inbox. The conversation becomes active when they accept.
    pub async fn start_conversation(
        &self,
        peer_id: UserId,
        intro_message: Option<&str>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;

        // Can't message yourself
        if peer_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if self.anon_dm_repo.get_conversation_by_peer(&peer_id)?.is_some() {
            return Err(AnonDmError::ConversationExists(peer_id));
        }

        // Create conversation locally
        let dm = AnonDm::new_initiated(peer_id);
        self.anon_dm_repo.insert_conversation(&dm)?;

        // Create our mailbox for this conversation
        let mailbox = self.create_conversation_mailbox(&dm.id).await?;
        self.anon_dm_repo.insert_mailbox(&mailbox)?;

        // Get profile key if available
        let profile_key = self.get_my_profile_key();

        // Send message request to peer's device inbox
        self.send_message_request(
            &peer_id,
            &dm.id,
            &mailbox.public_key,
            intro_message,
            profile_key.as_ref(),
        )
        .await?;

        tracing::info!(
            "Started anonymous DM conversation {} with {}",
            dm.id,
            peer_id
        );

        Ok(dm)
    }

    /// Start a new anonymous DM conversation using NDSS'21-compliant flow.
    ///
    /// This is the privacy-preserving method that:
    /// 1. Uses keys from the private friend code (no server lookup)
    /// 2. Encrypts initial message with ECIES (no X3DH prekey fetch)
    /// 3. Server cannot link sender to recipient
    ///
    /// # Arguments
    ///
    /// * `friend_code` - Private friend code containing peer's identity key and delivery token
    /// * `intro_message` - Optional introduction message
    ///
    /// # Privacy Guarantees
    ///
    /// - No prekey fetch: Keys come from out-of-band friend code
    /// - No server lookup: Peer ID, identity key, delivery token all embedded
    /// - Anonymous delivery: Uses blind signature token
    /// - Sealed sender: Server can't identify sender
    pub async fn start_conversation_private(
        &self,
        friend_code: &PrivateFriendCode,
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;
        let peer_id = friend_code.user_id;

        // Can't message yourself
        if peer_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if self.anon_dm_repo.get_conversation_by_peer(&peer_id)?.is_some() {
            return Err(AnonDmError::ConversationExists(peer_id));
        }

        // Create conversation locally
        let dm = AnonDm::new_initiated(peer_id);
        self.anon_dm_repo.insert_conversation(&dm)?;

        // Create our mailbox for this conversation
        let mailbox = self.create_conversation_mailbox(&dm.id).await?;
        self.anon_dm_repo.insert_mailbox(&mailbox)?;

        // Get profile key if available
        let profile_key = self.get_my_profile_key();

        // Send message request using ECIES (NDSS'21 compliant - no prekey fetch)
        self.send_message_request_ecies(
            friend_code,
            &dm.id,
            &mailbox.public_key,
            intro_message,
            sender_display_name,
            profile_key.as_ref(),
        )
        .await?;

        tracing::info!(
            conversation_id = %dm.id,
            peer_id = %peer_id,
            "Started NDSS'21-compliant anonymous DM conversation (no prekey fetch)"
        );

        Ok(dm)
    }

    /// Send a message request using HPKE encryption (NDSS'21 compliant).
    ///
    /// Unlike `send_message_request` which uses Signal Protocol (requiring prekey fetch),
    /// this method encrypts directly to the peer's identity key using HPKE (RFC 9180).
    /// The identity key comes from the private friend code (out-of-band).
    ///
    /// # Security Note
    ///
    /// Phase 1 does NOT have Perfect Forward Secrecy. The init message contains
    /// ONLY key material (mailbox key, identity key, delivery token) - NO sensitive
    /// user data. PFS begins in Phase 2 when Double Ratchet is established.
    async fn send_message_request_ecies(
        &self,
        friend_code: &PrivateFriendCode,
        conversation_id: &AnonDmId,
        our_mailbox_pk: &[u8; 32],
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
        profile_key: Option<&[u8; 32]>,
    ) -> Result<(), AnonDmError> {
        use crate::crypto::ecies;
        use crate::domain::anon_dm_init::AnonDmInitMessage;

        // Get our identity key to include in the initial message
        // This allows recipient to establish a proper Signal session later
        let our_identity_key = self.get_my_identity_key()?;
        let our_delivery_token = profile_key
            .map(PrivateFriendCode::derive_delivery_token)
            .unwrap_or([0u8; 12]);

        // Get our user ID - sender identity is revealed after decryption per NDSS'21 Section II-A
        let our_user_id = self.current_user_id()?;

        // Build the initial contact message per NDSS'21: "init" || pk_e
        // Extended to include our identity for subsequent session establishment
        let init_message = AnonDmInitMessage::new(
            *conversation_id,
            our_user_id,
            *our_mailbox_pk,
            our_identity_key,
            our_delivery_token,
            sender_display_name.unwrap_or(""),
            intro_message.unwrap_or_default(),
        );

        // Serialize the init message
        let plaintext = init_message.to_bytes()
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Encrypt with ECIES to recipient's identity key (no prekey fetch!)
        let recipient_x25519 = friend_code.x25519_public_key();
        let ecies_message = ecies::encrypt(&recipient_x25519, &plaintext)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Get blind signature token for anonymous delivery
        let token = self.get_or_request_token().await?;

        // Send to recipient's device inbox using anonymous credential
        // The ECIES ciphertext goes in the sealed_body, but we're NOT using
        // sealed sender here - we're using raw ECIES + anonymous credential
        let mut anon_client = AnonymousMessageClient::new(self.grpc_pool.channel());

        // Use a custom envelope type for ECIES messages
        // Set timestamp to current time in nanoseconds for server validation
        let timestamp_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time should be after Unix epoch")
            .as_nanos() as u64;

        let sealed_message = SealedSenderMessage {
            sealed_body: ecies_message.to_bytes(),
            timestamp: timestamp_nanos,
            // server_key_id defaults to 0 - not used for ECIES envelopes
            ..Default::default()
        };

        // Use envelope_type=4 (ECIES_INIT) so receiver knows to use HPKE decryption
        const ENVELOPE_TYPE_ECIES_INIT: i32 = 4;

        anon_client
            .send_with_anonymous_credential_typed(
                &friend_code.user_id.to_string(),
                0, // All devices
                sealed_message,
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
                ENVELOPE_TYPE_ECIES_INIT,
            )
            .await
            .map_err(|e| AnonDmError::MessageError(format!("Failed to send ECIES message: {}", e)))?;

        // Mark token as used
        self.token_manager
            .mark_token_used(&token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        tracing::debug!(
            conversation_id = %conversation_id,
            peer_id = %friend_code.user_id,
            "Sent ECIES-encrypted message request (NDSS'21 compliant)"
        );

        Ok(())
    }

    /// Get our Signal Protocol identity key.
    fn get_my_identity_key(&self) -> Result<[u8; 33], AnonDmError> {
        use crate::storage::repositories::settings_keys;

        // Get identity key from settings
        let key_bytes = self
            .settings_repo
            .get_bytes(settings_keys::IDENTITY_KEY_PUBLIC)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
            .ok_or(AnonDmError::NotLoggedIn)?;

        if key_bytes.len() != 33 {
            return Err(AnonDmError::CryptoError(format!(
                "Invalid identity key length: {}",
                key_bytes.len()
            )));
        }

        let mut key = [0u8; 33];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    }

    /// Send a message request to a peer's device inbox.
    async fn send_message_request(
        &self,
        peer_id: &UserId,
        conversation_id: &AnonDmId,
        our_mailbox_pk: &[u8; 32],
        intro_message: Option<&str>,
        profile_key: Option<&[u8; 32]>,
    ) -> Result<(), AnonDmError> {
        // Encrypt display name and avatar with profile key if available
        let (encrypted_display_name, encrypted_avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = {
                let display_name = self.get_my_display_name().unwrap_or_default();
                if !display_name.is_empty() {
                    crate::crypto::profile_crypto::encrypt_display_name(pk, &display_name).ok()
                } else {
                    None
                }
            };
            // Encrypt avatar (stored locally, shared encrypted during bootstrap)
            let enc_avatar = self.get_encrypted_avatar(pk);
            (enc_name, enc_avatar)
        } else {
            (None, None)
        };

        let request = AnonDmRequest {
            conversation_id: conversation_id.to_vec(),
            initiator_mailbox_pk: our_mailbox_pk.to_vec(),
            intro_message: intro_message.unwrap_or_default().to_string(),
            profile_key: profile_key.map(|k| k.to_vec()),
            timestamp: TimestampNanos::now().as_u64(),
            encrypted_display_name,
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmRequest(request)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to peer's device inbox via pairwise sealed sender
        self.send_to_device_inbox(peer_id, content).await?;

        tracing::debug!(
            "Sent message request for conversation {} to {}",
            conversation_id,
            peer_id
        );
        Ok(())
    }

    // ========================================================================
    // Acceptance (Recipient Side)
    // ========================================================================

    /// Accept an incoming message request.
    ///
    /// Creates our mailbox and sends acceptance to initiator's anonymous mailbox.
    pub async fn accept_request(
        &self,
        conversation_id: AnonDmId,
    ) -> Result<AnonDm, AnonDmError> {
        let _current_user = self.current_user_id()?;

        // Get the conversation
        let mut dm = self
            .anon_dm_repo
            .get_conversation(&conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(conversation_id))?;

        // Must be an incoming request
        if dm.initiated_by_us || dm.status != AnonDmStatus::Pending {
            return Err(AnonDmError::InvalidData(
                "Not a pending incoming request".to_string(),
            ));
        }

        // Create our mailbox for this conversation
        let mailbox = self.create_conversation_mailbox(&conversation_id).await?;
        self.anon_dm_repo.insert_mailbox(&mailbox)?;

        // Get peer's mailbox to send acceptance
        let peer_mailbox_pk = dm
            .peer_mailbox_pk
            .ok_or_else(|| AnonDmError::InvalidData("No peer mailbox".to_string()))?;

        // Get profile key if available
        let profile_key = self.get_my_profile_key();

        // Get our display name
        let my_display_name = self.get_my_display_name().unwrap_or_default();

        // Send acceptance to initiator's anonymous mailbox
        self.send_acceptance(
            &dm.peer_id,
            &conversation_id,
            &peer_mailbox_pk,
            &mailbox.public_key,
            profile_key.as_ref(),
            &my_display_name,
        )
        .await?;

        // Update conversation status
        dm.status = AnonDmStatus::Active;
        dm.updated_at = Utc::now();
        self.anon_dm_repo.update_conversation(&dm)?;

        tracing::info!(
            "Accepted anonymous DM conversation {} from {}",
            conversation_id,
            dm.peer_id
        );

        Ok(dm)
    }

    /// Send acceptance to initiator's anonymous mailbox.
    async fn send_acceptance(
        &self,
        peer_id: &UserId,
        conversation_id: &AnonDmId,
        peer_mailbox_pk: &[u8; 32],
        our_mailbox_pk: &[u8; 32],
        profile_key: Option<&[u8; 32]>,
        display_name: &str,
    ) -> Result<(), AnonDmError> {
        // Encrypt display name and avatar with profile key if available
        // (matches AnonDmRequest encryption for consistency)
        let (encrypted_display_name, encrypted_avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = if !display_name.is_empty() {
                crate::crypto::profile_crypto::encrypt_display_name(pk, display_name).ok()
            } else {
                None
            };
            // Encrypt avatar (stored locally, shared encrypted during bootstrap)
            let enc_avatar = self.get_encrypted_avatar(pk);
            (enc_name, enc_avatar)
        } else {
            (None, None)
        };

        let accept = AnonDmAccept {
            conversation_id: conversation_id.to_vec(),
            responder_mailbox_pk: our_mailbox_pk.to_vec(),
            profile_key: profile_key.map(|k| k.to_vec()),
            timestamp: TimestampNanos::now().as_u64(),
            responder_display_name: display_name.to_string(),
            encrypted_display_name,
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmAccept(accept)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to initiator's anonymous mailbox
        self.send_to_anon_mailbox(peer_id, peer_mailbox_pk, content)
            .await?;

        tracing::debug!(
            "Sent acceptance for conversation {} to {}",
            conversation_id,
            peer_id
        );
        Ok(())
    }

    /// Decline an incoming message request.
    pub fn decline_request(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        // Must be an incoming request
        if dm.initiated_by_us || dm.status != AnonDmStatus::Pending {
            return Err(AnonDmError::InvalidData(
                "Not a pending incoming request".to_string(),
            ));
        }

        // Update status to declined
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Declined)?;

        tracing::info!(
            "Declined anonymous DM conversation {} from {}",
            conversation_id,
            dm.peer_id
        );

        Ok(())
    }

    /// Block a conversation (can be done at any time).
    pub fn block_conversation(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Blocked)?;
        Ok(())
    }

    // ========================================================================
    // Handle Acceptance (Initiator Side)
    // ========================================================================

    /// Handle an acceptance from the recipient.
    ///
    /// Updates the conversation with peer's mailbox, display name, and activates it.
    pub fn handle_acceptance(
        &self,
        conversation_id: AnonDmId,
        peer_id: UserId,
        responder_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        peer_display_name: Option<String>,
        peer_avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonDmError> {
        let mut dm = self
            .anon_dm_repo
            .get_conversation(&conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(conversation_id))?;

        // Verify this is our outgoing request
        if !dm.initiated_by_us || dm.peer_id != peer_id {
            return Err(AnonDmError::InvalidData(
                "Acceptance from unexpected peer".to_string(),
            ));
        }

        // Update with peer's mailbox, display name, avatar, and activate
        dm.peer_mailbox_pk = Some(responder_mailbox_pk);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        dm.status = AnonDmStatus::Active;
        dm.updated_at = Utc::now();
        self.anon_dm_repo.update_conversation(&dm)?;

        tracing::info!(
            "Received acceptance for conversation {} from {}",
            conversation_id,
            peer_id
        );

        Ok(())
    }

    // ========================================================================
    // Handle Incoming Request (Recipient Side)
    // ========================================================================

    /// Handle an incoming message request.
    ///
    /// Creates a pending conversation that can be accepted or declined.
    pub fn handle_request(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_mailbox_pk: [u8; 32],
        intro_message: Option<&str>,
        profile_key: Option<[u8; 32]>,
        peer_display_name: Option<String>,
        peer_avatar: Option<Vec<u8>>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;

        // Can't message yourself
        if sender_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if let Some(existing) = self.anon_dm_repo.get_conversation(&conversation_id)? {
            // Already have this conversation
            return Ok(existing);
        }

        // Create conversation as incoming request
        let mut dm = AnonDm::from_request(conversation_id, sender_id, sender_mailbox_pk);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        self.anon_dm_repo.insert_conversation(&dm)?;

        // Store the intro message if present
        if let Some(text) = intro_message {
            if !text.is_empty() {
                // Use current time as sent_at for intro messages (they don't have a Content timestamp)
                let sent_at = TimestampNanos::now().as_u64();
                let msg = DomainAnonDmMessage::new_text(conversation_id, sender_id, text, sent_at);
                self.anon_dm_repo.insert_message(&msg)?;
            }
        }

        tracing::info!(
            "Received message request {} from {}",
            conversation_id,
            sender_id
        );

        Ok(dm)
    }

    // ========================================================================
    // Messaging
    // ========================================================================

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
    async fn send_ephemeral_message(
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

    // ========================================================================
    // Message Delivery Infrastructure
    // ========================================================================

    /// Send content to a peer's device inbox (for message requests).
    ///
    /// Uses FULLY ANONYMOUS operations:
    /// 1. Anonymous prekey fetch (blind signature token #1)
    /// 2. Anonymous message send (blind signature token #2)
    ///
    /// Server learns NOTHING about the sender.
    async fn send_to_device_inbox(
        &self,
        peer_id: &UserId,
        content: Content,
    ) -> Result<(), AnonDmError> {
        // Serialize content
        let plaintext = content.encode_to_vec();

        // Get token #1 for anonymous prekey fetch
        let prekey_token = self.get_or_request_token().await?;

        // Encrypt with Signal Protocol ANONYMOUSLY
        // This fetches prekeys using anonymous credential - server doesn't know who's asking
        let ciphertext = self
            .pairwise_service
            .encrypt_anonymous(peer_id, &plaintext, &prekey_token)
            .await
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Mark prekey token as used
        self.token_manager
            .mark_token_used(&prekey_token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Get token #2 for anonymous message send
        let send_token = self.get_or_request_token().await?;

        // Build SealedSenderMessage
        let sealed_message = SealedSenderMessage {
            sealed_body: ciphertext,
            ..Default::default()
        };

        // Send to device inbox using ANONYMOUS client (no auth headers)
        let mut anon_client = AnonymousMessageClient::new(self.grpc_pool.channel());

        anon_client
            .send_with_anonymous_credential(
                &peer_id.to_string(),
                0, // All devices
                sealed_message,
                &send_token.public_key,
                &send_token.signature,
                send_token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| AnonDmError::MessageError(format!("Failed to send to device inbox: {}", e)))?;

        // Mark send token as used
        self.token_manager
            .mark_token_used(&send_token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        tracing::debug!(
            peer_id = %peer_id,
            "Sent to device inbox using fully anonymous path"
        );

        Ok(())
    }

    /// Get an available token or request new ones.
    async fn get_or_request_token(&self) -> Result<crate::mailbox::token_manager::MailboxToken, AnonDmError> {
        match self.token_manager.get_available_token() {
            Ok(Some(token)) => Ok(token),
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client = MailboxClient::new(
                    self.grpc_pool.channel(),
                    self.grpc_pool.auth_clone(),
                );
                self.token_manager
                    .request_tokens(&mut mailbox_client, 5)
                    .await
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

                self.token_manager
                    .get_available_token()
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
                    .ok_or_else(|| AnonDmError::CryptoError("No tokens available".to_string()))
            }
            Err(e) => Err(AnonDmError::CryptoError(e.to_string())),
        }
    }

    /// Send content to a peer's anonymous mailbox.
    ///
    /// Uses pairwise Signal Protocol encryption and delivers to the anonymous mailbox.
    async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonDmError> {
        // Serialize content
        let plaintext = content.encode_to_vec();

        // Encrypt with Signal Protocol
        let ciphertext = self
            .pairwise_service
            .encrypt(peer_id, &plaintext)
            .await
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Send to anonymous mailbox
        self.mailbox_manager
            .send_to_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| AnonDmError::MessageError(e.to_string()))?;

        Ok(())
    }

    /// Create a mailbox for a conversation using blind signature tokens.
    async fn create_conversation_mailbox(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<AnonDmMailbox, AnonDmError> {
        // Get or request a blind-signed token
        let token = match self.token_manager.get_available_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client = MailboxClient::new(
                    self.grpc_pool.channel(),
                    self.grpc_pool.auth_clone(),
                );
                self.token_manager
                    .request_tokens(&mut mailbox_client, 5)
                    .await
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

                self.token_manager
                    .get_available_token()
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
                    .ok_or_else(|| AnonDmError::CryptoError("No tokens available".to_string()))?
            }
            Err(e) => return Err(AnonDmError::CryptoError(e.to_string())),
        };

        // Use the token's Ed25519 keypair for the mailbox
        let public_key: [u8; 32] = token
            .public_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid public key length".to_string()))?;
        let private_key: [u8; 64] = token
            .private_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid private key length".to_string()))?;

        // Register the mailbox with the server using the blind signature
        let ttl_seconds = 30 * 24 * 60 * 60; // 30 days
        let mut mailbox_client = MailboxClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        let response = mailbox_client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                ttl_seconds,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| {
                AnonDmError::MessageError(format!("Failed to create mailbox on server: {}", e))
            })?;

        tracing::info!(
            conversation_id = %conversation_id,
            mailbox_id = %response.mailbox_id,
            expires_at = response.expires_at,
            "Created ephemeral mailbox for anonymous DM"
        );

        let now = Utc::now();
        let mailbox = AnonDmMailbox {
            id: Uuid::new_v4().to_string(),
            conversation_id: *conversation_id,
            public_key,
            private_key,
            token_id: token.id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30),
        };

        // Mark the token as used
        self.token_manager
            .mark_token_used(&token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        Ok(mailbox)
    }

    /// Create an incoming conversation from ECIES init message (NDSS'21 private contact).
    ///
    /// This is called when we receive a Phase 1 HPKE-encrypted initial contact message.
    /// Stores the conversation and peer's keys for Phase 2 Double Ratchet setup.
    ///
    /// # Arguments
    ///
    /// * `dm` - The conversation metadata (pre-created with peer info)
    /// * `peer_identity_key` - Peer's Signal identity key (33 bytes)
    /// * `peer_delivery_token` - Peer's delivery token for sealed sender
    ///
    /// Note: The identity_key and delivery_token are logged for debugging but not
    /// stored separately. They're needed for Phase 2 session establishment.
    pub fn create_incoming_conversation(
        &self,
        dm: &AnonDm,
        _peer_identity_key: &[u8; 33],
        _peer_delivery_token: &[u8; 12],
    ) -> Result<(), AnonDmError> {
        // Check if conversation already exists
        if let Some(_existing) = self.anon_dm_repo.get_conversation(&dm.id)? {
            tracing::warn!(
                conversation_id = %dm.id,
                "Conversation already exists from ECIES init"
            );
            return Ok(());
        }

        // Store the conversation
        self.anon_dm_repo.insert_conversation(dm)?;

        tracing::info!(
            conversation_id = %dm.id,
            peer_id = %dm.peer_id,
            "Created incoming conversation from ECIES init message"
        );

        // TODO: Store peer_identity_key and peer_delivery_token for Phase 2
        // This would require adding new tables or extending the conversation table.
        // For now, the Double Ratchet session will be established when they accept.

        Ok(())
    }

    // ========================================================================
    // Queries
    // ========================================================================

    /// Get a conversation by ID.
    pub fn get_conversation(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.get_conversation(conversation_id)?)
    }

    /// Get a conversation by peer ID.
    pub fn get_conversation_by_peer(&self, peer_id: &UserId) -> Result<Option<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.get_conversation_by_peer(peer_id)?)
    }

    /// List all active conversations.
    pub fn list_conversations(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_active_conversations()?)
    }

    /// List pending message requests (incoming only).
    pub fn list_message_requests(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_message_requests()?)
    }

    /// List outgoing pending requests.
    pub fn list_outgoing_requests(&self) -> Result<Vec<AnonDm>, AnonDmError> {
        Ok(self.anon_dm_repo.list_outgoing_requests()?)
    }

    /// Get total unread message count.
    pub fn get_total_unread_count(&self) -> Result<u32, AnonDmError> {
        Ok(self.anon_dm_repo.get_total_unread_count()?)
    }

    /// Mark a conversation as read.
    pub fn mark_as_read(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        Ok(self.anon_dm_repo.mark_as_read(conversation_id)?)
    }

    // ========================================================================
    // Incoming Message Processing
    // ========================================================================

    /// Process an incoming message from an anonymous DM conversation.
    ///
    /// This is the service-layer entry point for handling incoming `InboxEvent::AnonDmMessage`.
    /// It validates the conversation, routes based on content type, and returns domain events.
    ///
    /// # Arguments
    /// * `conversation_id` - The conversation ID
    /// * `sender_id` - The sender's user ID
    /// * `inner_content` - The inner message content (decrypted)
    ///
    /// # Returns
    /// A list of domain events to emit, or an error if processing failed.
    pub fn process_incoming_content(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        inner_content: &Content,
    ) -> Result<Vec<crate::inbox::handlers::DomainEvent>, AnonDmError> {
        use crate::inbox::handlers::DomainEvent;
        use crate::proto::content_v1::{content::Content as ContentType, typing_message, receipt_message};

        // Check if sender is globally blocked
        if self.is_user_blocked(sender_id).unwrap_or(false) {
            tracing::info!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                "[ANON_DM] Dropped message from blocked user"
            );
            return Ok(vec![]);
        }

        // Check if conversation is blocked
        if self.is_conversation_blocked(conversation_id).unwrap_or(false) {
            tracing::info!(
                conversation_id = %conversation_id,
                "[ANON_DM] Dropped message for blocked conversation"
            );
            return Ok(vec![]);
        }

        // Verify conversation exists and is active
        match self.get_conversation(conversation_id) {
            Ok(Some(dm)) => {
                if dm.peer_id != *sender_id {
                    tracing::info!(
                        sender_id = %sender_id,
                        conversation_id = %conversation_id,
                        expected_peer_id = %dm.peer_id,
                        "[ANON_DM] Dropped message from unexpected sender"
                    );
                    return Ok(vec![]);
                }
                if !dm.is_ready() {
                    tracing::info!(
                        conversation_id = %conversation_id,
                        status = ?dm.status,
                        has_peer_mailbox = dm.peer_mailbox_pk.is_some(),
                        "[ANON_DM] Dropped message in non-active conversation"
                    );
                    return Ok(vec![]);
                }
            }
            Ok(None) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    "[ANON_DM] Dropped message for unknown conversation"
                );
                return Ok(vec![]);
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "[ANON_DM] Failed to check conversation"
                );
                return Ok(vec![]);
            }
        }

        let mut events = Vec::new();

        // Route based on inner content type
        match &inner_content.content {
            Some(ContentType::DataMessage(data_msg)) => {
                // Check for delete first
                if let Some(delete) = &data_msg.delete {
                    let deleted = self.delete_message_by_timestamp(
                        conversation_id,
                        sender_id,
                        delete.target_sent_timestamp,
                    )?;

                    if deleted {
                        tracing::info!(
                            "Deleted message at timestamp {} from {} in anon DM {}",
                            delete.target_sent_timestamp,
                            sender_id,
                            conversation_id
                        );
                    }

                    events.push(DomainEvent::AnonDmMessageDeleted {
                        conversation_id: *conversation_id,
                        message_timestamp: delete.target_sent_timestamp,
                    });
                    return Ok(events);
                }

                // Handle text and/or attachments
                let has_text = !data_msg.body.is_empty();
                let has_attachments = !data_msg.attachments.is_empty();

                if has_text || has_attachments {
                    // Create attachment records from pointers if any
                    let attachment_ids = if has_attachments {
                        let attachment_service = AttachmentService::new(self.attachment_repo.clone());
                        let ids = attachment_service.create_from_pointers(&data_msg.attachments);
                        tracing::info!(
                            conversation_id = %conversation_id,
                            attachment_count = ids.len(),
                            "[ANON_DM] Created attachment records from pointers"
                        );
                        ids
                    } else {
                        Vec::new()
                    };

                    // Use sender's timestamp for consistent ordering across devices
                    let sent_at = inner_content.timestamp;
                    let message_id = self.store_message_with_attachments(
                        conversation_id,
                        sender_id,
                        &data_msg.body,
                        &attachment_ids,
                        sent_at,
                    )?;
                    tracing::info!(
                        conversation_id = %conversation_id,
                        message_id = %message_id,
                        sent_at = sent_at,
                        has_text = has_text,
                        attachment_count = attachment_ids.len(),
                        "[ANON_DM] Stored message"
                    );

                    events.push(DomainEvent::AnonDmMessageReceived {
                        conversation_id: *conversation_id,
                        sender_id: *sender_id,
                        message_id,
                        inner_content: inner_content.clone(),
                        sent_at,
                    });
                }
            }
            Some(ContentType::Typing(typing)) => {
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();
                let started = typing.action == typing_message::Action::Started as i32;

                if started {
                    events.push(DomainEvent::TypingStarted {
                        channel_id,
                        user_id: *sender_id,
                    });
                } else {
                    events.push(DomainEvent::TypingStopped {
                        channel_id,
                        user_id: *sender_id,
                    });
                }
            }
            Some(ContentType::Reaction(reaction)) => {
                let message_id = crate::domain::MessageId::from_timestamp(reaction.target_sent_timestamp);
                let author_id = reaction.target_author_aci.parse().unwrap_or(*sender_id);

                if reaction.remove {
                    events.push(DomainEvent::ReactionRemoved {
                        message_id,
                        author_id,
                        reactor_id: *sender_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                } else {
                    events.push(DomainEvent::ReactionAdded {
                        message_id,
                        author_id,
                        reactor_id: *sender_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                }
            }
            Some(ContentType::Receipt(receipt)) => {
                if receipt.r#type == receipt_message::Type::Read as i32 {
                    events.push(DomainEvent::MessagesRead {
                        reader_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                    });
                } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
                    events.push(DomainEvent::MessagesDelivered {
                        recipient_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                    });
                }
            }
            Some(ContentType::Pin(pin)) => {
                let message_id = crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp);
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();

                if pin.unpin {
                    events.push(DomainEvent::MessageUnpinned {
                        message_id,
                        unpinner_id: *sender_id,
                        channel_id,
                    });
                } else {
                    events.push(DomainEvent::MessagePinned {
                        message_id,
                        pinner_id: *sender_id,
                        channel_id,
                    });
                }
            }
            Some(ContentType::AnonGroupInvite(invite)) => {
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&invite.group_id)
                    .unwrap_or_default();
                let mut pk = [0u8; 32];
                if invite.inviter_mailbox_pk.len() == 32 {
                    pk.copy_from_slice(&invite.inviter_mailbox_pk);
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Processing anonymous group invite in anon DM"
                );

                events.push(DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name: invite.group_name.clone(),
                    inviter_id: *sender_id,
                    inviter_mailbox_pk: pk,
                    profile_key: None,
                    inviter_display_name: None,
                    inviter_avatar: None,
                });
            }
            Some(ContentType::AnonGroupJoinAck(ack)) => {
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&ack.group_id)
                    .unwrap_or_default();
                
                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Processing anonymous group join ack in anon DM"
                );

                events.push(DomainEvent::AnonGroupMemberJoined {
                    group_id,
                    user_id: *sender_id,
                    profile_key: None,
                    display_name: None,
                    avatar: None,
                });
            }
            Some(ContentType::AnonGroupMemberList(list)) => {
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                    .unwrap_or_default();

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    member_count = list.members.len(),
                    "Processing anonymous group member list in anon DM"
                );

                // Emit event for UI to update local state via AnonGroupService
                events.push(DomainEvent::AnonGroupMemberListReceived {
                    group_id,
                    members: list.members.clone(),
                });
            }
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "[ANON_DM] Ignoring unhandled inner content type"
                );
            }
        }

        Ok(events)
    }

    // ========================================================================
    // Message Storage
    // ========================================================================

    /// Store a message received from the peer.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let message = DomainAnonDmMessage {
            id: AnonDmMessageId::new(),
            conversation_id: *conversation_id,
            sender_id: *sender_id,
            content_type,
            content_json: content_json.to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            is_read: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_dm_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored message {} in conversation {} with sent_at={}",
            message.id,
            conversation_id,
            sent_at
        );
        Ok(message.id)
    }

    /// Store a text message.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_text_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let message = DomainAnonDmMessage::new_text(*conversation_id, *sender_id, text, sent_at);
        self.anon_dm_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored text message {} in conversation {} with sent_at={}",
            message.id,
            conversation_id,
            sent_at
        );
        Ok(message.id)
    }

    /// Store a message with text and/or attachments.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[crate::domain::AttachmentId],
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        use crate::storage::serialization::db_serialize;

        // Determine content type: 1=text, 2=attachment only, 3=mixed
        let has_text = !text.is_empty();
        let has_attachments = !attachment_ids.is_empty();

        let (content_type, content_json) = match (has_text, has_attachments) {
            (true, false) => (1, serde_json::json!({ "text": text }).to_string()),
            (false, true) => (2, serde_json::json!({ "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (true, true) => (3, serde_json::json!({ "text": text, "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (false, false) => (0, "{}".to_string()),
        };

        // Serialize attachment IDs to blob
        let attachment_ids_vec: Vec<_> = attachment_ids.to_vec();
        let attachments_blob = db_serialize(&attachment_ids_vec).unwrap_or_else(|_| vec![8, 0, 0, 0, 0, 0, 0, 0]);

        let message = DomainAnonDmMessage {
            id: AnonDmMessageId::new(),
            conversation_id: *conversation_id,
            sender_id: *sender_id,
            content_type,
            content_json,
            attachments_blob,
            is_deleted: false,
            is_read: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_dm_repo.insert_message(&message)?;
        tracing::info!(
            conversation_id = %conversation_id,
            message_id = %message.id,
            sent_at = sent_at,
            has_text = has_text,
            attachment_count = attachment_ids.len(),
            "[ANON_DM] Stored message (sender)"
        );
        Ok(message.id)
    }

    /// Get messages for a conversation with pagination.
    pub fn get_messages(
        &self,
        conversation_id: &AnonDmId,
        before: Option<&chrono::DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<DomainAnonDmMessage>, AnonDmError> {
        Ok(self
            .anon_dm_repo
            .get_messages(conversation_id, before, limit)?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonDmMessageId,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_message(id)?)
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, etc. where we identify
    /// messages by (sender_id, sent_at) tuple per Signal Protocol convention.
    pub fn get_message_by_sender_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_message_by_sender_timestamp(conversation_id, sender_id, timestamp_ns)?)
    }

    /// Get the most recent message in a conversation.
    pub fn get_latest_message(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<DomainAnonDmMessage>, AnonDmError> {
        Ok(self.anon_dm_repo.get_latest_message(conversation_id)?)
    }

    /// Get the message count for a conversation.
    pub fn get_message_count(&self, conversation_id: &AnonDmId) -> Result<i64, AnonDmError> {
        Ok(self.anon_dm_repo.get_message_count(conversation_id)?)
    }

    /// Delete a message.
    pub fn delete_message(&self, id: &AnonDmMessageId) -> Result<(), AnonDmError> {
        self.anon_dm_repo.delete_message(id)?;
        Ok(())
    }

    /// Delete a message by sender timestamp (for incoming delete-for-everyone).
    ///
    /// Uses (conversation_id, sender_id, sent_timestamp) as the Signal-style lookup key.
    pub fn delete_message_by_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.mark_message_deleted_by_timestamp(
            conversation_id,
            sender_id,
            sent_timestamp,
        )?)
    }

    // ========================================================================
    // Blocking
    // ========================================================================

    /// Unblock a conversation.
    pub fn unblock_conversation(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Active)?;
        tracing::info!("Unblocked conversation {}", conversation_id);
        Ok(())
    }

    /// Check if a conversation is blocked.
    pub fn is_conversation_blocked(&self, conversation_id: &AnonDmId) -> Result<bool, AnonDmError> {
        match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) => Ok(dm.status == AnonDmStatus::Blocked),
            None => Err(AnonDmError::ConversationNotFound(*conversation_id)),
        }
    }

    /// Block a user globally (block all anonymous DMs from this user).
    pub fn block_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<(), AnonDmError> {
        self.anon_dm_repo.block_user(user_id, reason)?;
        tracing::info!("Globally blocked user {} for anonymous DMs", user_id);
        Ok(())
    }

    /// Unblock a user globally.
    pub fn unblock_user(&self, user_id: &UserId) -> Result<(), AnonDmError> {
        self.anon_dm_repo.unblock_user(user_id)?;
        tracing::info!("Unblocked user {} for anonymous DMs", user_id);
        Ok(())
    }

    /// Check if a user is globally blocked.
    pub fn is_user_blocked(&self, user_id: &UserId) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.is_user_blocked(user_id)?)
    }

    /// Get all globally blocked users.
    pub fn get_blocked_users(&self) -> Result<Vec<UserId>, AnonDmError> {
        Ok(self.anon_dm_repo.get_blocked_users()?)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Default rate limit: 30 messages per minute per conversation.
    const RATE_LIMIT_MESSAGES: u32 = 30;
    /// Default rate limit window: 1 minute.
    const RATE_LIMIT_WINDOW: chrono::Duration = chrono::Duration::minutes(1);

    /// Check if sending a message would exceed rate limits.
    ///
    /// Returns Ok(()) if allowed, Err if rate limited.
    pub fn check_rate_limit(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let count = self.anon_dm_repo.increment_rate_limit(conversation_id, Self::RATE_LIMIT_WINDOW)?;

        if count > Self::RATE_LIMIT_MESSAGES {
            tracing::warn!(
                "Rate limit exceeded for conversation {}: {} messages in window",
                conversation_id,
                count
            );
            return Err(AnonDmError::RateLimitExceeded(Self::RATE_LIMIT_MESSAGES));
        }

        Ok(())
    }

    /// Check if the conversation is blocked (cannot send).
    pub fn check_conversation_not_blocked(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if dm.status == AnonDmStatus::Blocked {
            return Err(AnonDmError::ConversationBlocked(*conversation_id));
        }

        Ok(())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Generate random padding to reach a target size.
    fn generate_padding(boundary: usize) -> Vec<u8> {
        let padding_needed = boundary;
        let mut padding = vec![0u8; padding_needed];
        rand::rng().fill_bytes(&mut padding);
        padding
    }
}
