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
    pub(crate) fn current_user_id(&self) -> Result<UserId, AnonDmError> {
        Ok(self.settings_repo.current_user_id()?)
    }
}
