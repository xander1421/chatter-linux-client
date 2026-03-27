//! Anonymous DM service - modular implementation.
//!
//! Handles all anonymous DM operations following the NDSS'21 model.
//!
//! Organized into focused submodules:
//! - `initiation` - Starting new conversations (Sender Side)
//! - `acceptance` - Accepting/Handling requests (Recipient Side)
//! - `messaging` - Sending messages (active conversations)
//! - `processing` - Processing incoming content and storage logic
//! - `queries` - Listing and fetching conversations/messages
//! - `infrastructure` - Delivery and mailbox management
//! - `blocking` - Blocking and rate limiting
//! - `helpers` - Misc helpers (profile key, avatar, etc.)

mod initiation;
mod acceptance;
mod messaging;
mod processing;
mod queries;
mod infrastructure;
mod blocking;
mod helpers;
mod calls;

// Re-export call types for external use
pub use calls::{AnonCallOfferResult, AnonCallRoomCredentials, ReceivedCallOffer, ReceivedCallAnswer, PendingAnonCall};

use std::sync::Arc;

use crate::domain::anon_dm::AnonDmId;
use crate::domain::UserId;
use crate::error::StorageError;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::pairwise::PairwiseService;
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::SettingsRepository;

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

/// Decrypted profile data from an anonymous DM request or acceptance.
#[derive(Debug, Clone)]
pub struct DecryptedPeerProfile {
    /// The decrypted display name (required).
    pub display_name: String,
    /// The decrypted avatar bytes (optional).
    pub avatar: Option<Vec<u8>>,
    /// The profile key used for decryption (for storage).
    pub profile_key: Option<[u8; 32]>,
}

/// Anonymous DM service.
///
/// Handles all anonymous DM operations following the NDSS'21 model.
pub struct AnonDmService {
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) anon_dm_repo: Arc<AnonDmRepository>,
    /// Attachment storage for incoming file metadata.
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    /// Reusable Signal Protocol pairwise encryption.
    pub(crate) pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    pub(crate) mailbox_manager: Arc<MailboxManager>,
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) token_manager: Arc<TokenManager>,
    /// Event bus for emitting DomainEvents (sender-side message display).
    pub(crate) event_bus: crate::events::EventBus,
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
        event_bus: crate::events::EventBus,
    ) -> Self {
        Self {
            settings_repo,
            anon_dm_repo,
            attachment_repo,
            pairwise_service,
            mailbox_manager,
            grpc_pool,
            token_manager,
            event_bus,
        }
    }

    /// Get the anon DM repository (for inbox provider initialization).
    pub fn anon_dm_repo(&self) -> Arc<AnonDmRepository> {
        self.anon_dm_repo.clone()
    }

    /// Get the pairwise service (for inbox provider initialization).
    pub fn pairwise_service(&self) -> Arc<PairwiseService> {
        self.pairwise_service.clone()
    }

    /// Get the current user ID.
    pub(crate) fn current_user_id(&self) -> Result<UserId, AnonDmError> {
        Ok(self.settings_repo.current_user_id()?)
    }

    /// Get our avatar bytes from settings.
    pub fn get_my_avatar(&self) -> Option<Vec<u8>> {
        use crate::storage::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }
    
    /// Download avatar from URL and store it in the conversation record.
    ///
    /// Returns the downloaded bytes on success (for UI display).
    pub async fn download_and_store_avatar(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        url: &str,
    ) -> Result<Vec<u8>, AnonDmError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| AnonDmError::MessageError(format!("HTTP client error: {}", e)))?;

        let response = client.get(url).send().await
            .map_err(|e| AnonDmError::MessageError(format!("Avatar download failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AnonDmError::MessageError(
                format!("Avatar download HTTP {}", response.status())
            ));
        }

        let bytes = response.bytes().await
            .map_err(|e| AnonDmError::MessageError(format!("Avatar read failed: {}", e)))?
            .to_vec();

        if bytes.is_empty() {
            return Err(AnonDmError::MessageError("Empty avatar response".into()));
        }

        // Persist to DB
        if let Ok(Some(mut conv)) = self.anon_dm_repo.get_conversation(conversation_id) {
            conv.peer_avatar = Some(bytes.clone());
            self.anon_dm_repo.update_conversation(&conv)
                .map_err(|e| AnonDmError::MessageError(format!("Avatar store failed: {}", e)))?;
        }

        Ok(bytes)
    }

    /// Get an available call room token.
    pub fn get_available_call_room_token(&self) -> Result<Option<crate::mailbox::CallRoomToken>, AnonDmError> {
        self.token_manager
            .get_available_call_room_token()
            .map_err(|e| AnonDmError::MessageError(format!("Failed to get call room token: {}", e)))
    }
    
    /// Mark a call room token as used.
    pub fn mark_call_room_token_used(&self, token_id: &str) -> Result<(), AnonDmError> {
        self.token_manager
            .mark_call_room_token_used(token_id)
            .map_err(|e| AnonDmError::MessageError(format!("Failed to mark token as used: {}", e)))
    }
    
    /// Request new call room tokens.
    pub async fn request_call_room_tokens(&self, count: u32) -> Result<u32, AnonDmError> {
        if !self.grpc_pool.is_connected() {
            return Err(AnonDmError::MessageError("No gRPC connection available".to_string()));
        }
        let mut client = self.grpc_pool.mailbox_client();
        self.token_manager
            .request_call_room_tokens(&mut client, count)
            .await
            .map_err(|e| AnonDmError::MessageError(format!("Failed to request tokens: {}", e)))
    }
    
}
