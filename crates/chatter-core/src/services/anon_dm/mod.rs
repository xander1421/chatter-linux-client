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

mod acceptance;
mod blocking;
mod calls;
mod helpers;
mod infrastructure;
mod initiation;
mod messaging;
mod processing;
mod queries;

// Re-export call types for external use
pub use calls::{
    AnonCallOfferResult, AnonCallRoomCredentials, PendingAnonCall, ReceivedCallAnswer,
    ReceivedCallOffer,
};

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::anon_dm::AnonDmId;
use crate::error::StorageError;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::current_user::CurrentUserProvider;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::pairwise::PairwiseService;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::SettingsRepository;

/// Anonymous DM service errors.
#[derive(Debug, thiserror::Error)]
pub enum AnonDmError {
    #[error("Delivery unconfirmed; the original submission is retained and retry is disabled")]
    DeliveryUnconfirmed,
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

    /// Anon DM setup is only permitted in anonymous (shield) mode. Starting it
    /// outside shield mode would run the ECIES init + delivery over the
    /// identified/direct path, leaking the very link it is meant to hide.
    #[error("requires anon (shield) mode")]
    RequiresAnonMode,

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
    /// Unified message repository for all message CRUD operations.
    pub(crate) message_repo: Arc<MessageRepository>,
    /// Attachment storage for incoming file metadata.
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    /// Reusable Signal Protocol pairwise encryption.
    pub(crate) pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    pub(crate) mailbox_manager: Arc<MailboxManager>,
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) token_manager: Arc<TokenManager>,
}

impl AnonDmService {
    /// Create a new AnonDmService.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        anon_dm_repo: Arc<AnonDmRepository>,
        message_repo: Arc<MessageRepository>,
        attachment_repo: Arc<AttachmentRepository>,
        pairwise_service: Arc<PairwiseService>,
        mailbox_manager: Arc<MailboxManager>,
        grpc_pool: Arc<GrpcClientPool>,
        token_manager: Arc<TokenManager>,
    ) -> Self {
        Self {
            settings_repo,
            anon_dm_repo,
            message_repo,
            attachment_repo,
            pairwise_service,
            mailbox_manager,
            grpc_pool,
            token_manager,
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

    /// Persist the user's avatar bytes locally.
    ///
    /// Bytes are never uploaded to the server; they only travel over
    /// already-established Signal sessions via `send_profile_update`.
    /// Callers must size-check; we enforce [`crate::domain::anon_dm_init::MAX_AVATAR_SIZE`]
    /// as defense in depth.
    pub fn set_my_avatar_bytes(&self, bytes: &[u8]) -> Result<(), AnonDmError> {
        use crate::storage::settings_keys;
        if bytes.len() > crate::domain::anon_dm_init::MAX_AVATAR_SIZE {
            return Err(AnonDmError::MessageError(format!(
                "avatar too large: {} bytes, max {}",
                bytes.len(),
                crate::domain::anon_dm_init::MAX_AVATAR_SIZE,
            )));
        }
        self.settings_repo
            .set_bytes(settings_keys::AVATAR, bytes)
            .map_err(|e| AnonDmError::Storage(e))?;
        tracing::info!(avatar_size = bytes.len(), "Stored avatar bytes locally");
        Ok(())
    }

    // CRYPTO-2: there used to be `download_avatar_direct` / `download_and_store_avatar`
    // here, which GETted a URL chosen by an anonymous DM peer with no user
    // interaction, no scheme allowlist, no size cap (`response.bytes()` buffers
    // whatever the host sends), no digest to check the bytes against, and — on
    // the direct path — reqwest's default root store rather than the CA this
    // build pins. A peer picked the host, so the fetch was an SSRF and an
    // IP-disclosure primitive against whoever received their profile update, and
    // an unbounded allocation besides.
    //
    // Nothing is lost by deleting it: a profile update already carries the
    // avatar inline, encrypted under the conversation key and size-bounded like
    // any other message body, and the receiving path has always preferred those
    // bytes (see the `avatar` branch in the UI's anon-DM profile handler). The
    // URL was only ever the fallback. Peer-supplied URLs are now ignored.
}
