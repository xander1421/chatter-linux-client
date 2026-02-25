//! Message service - modular implementation.
//!
//! Send, edit, delete, forward, and quote messages.
//!
//! This module is organized into focused submodules:
//! - `dm` - DM operations (Signal Protocol encryption)
//! - `group` - Group/sender key operations
//! - `core` - Core message operations (send, edit, delete, reply, forward)
//! - `thread` - Thread operations
//! - `pin` - Pin operations
//! - `saved` - Saved messages (Note to Self)

mod dm;
mod dm_bootstrap;
mod forward;
mod group;
pub mod core;
pub mod conversion;
mod thread;
mod pin;
mod saved;

use std::sync::Arc;

use crate::crypto::signal::{SesameManager, SqliteSesameStore};
use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::error::{NetworkError, StorageError};
use crate::mailbox::group::GroupMailboxManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::message_client::MessageClient;
use crate::services::current_user::CurrentUserError;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::signal_service::SignalService;
use crate::storage::Database;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::message_repo::MessageRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

use crate::domain::channel::ChannelType;

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
    MessageNotFound(crate::domain::ids::MessageId),

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

impl From<CurrentUserError> for MessageError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => MessageError::Storage(e),
            CurrentUserError::NotLoggedIn => MessageError::NotLoggedIn,
        }
    }
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
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) message_repo: Arc<MessageRepository>,
    pub(crate) channel_repo: Arc<ChannelRepository>,
    pub(crate) signal_service: Arc<SignalService>,
    /// Group mailbox manager for NDSS '21 per-group mailboxes.
    pub(crate) group_mailbox_manager: Arc<GroupMailboxManager>,
    /// Sesame session manager for multi-session handling and message queueing.
    pub(crate) sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
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

    pub(crate) fn message_client(&self) -> MessageClient {
        MessageClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    pub(crate) fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    pub(crate) fn channel_client(&self) -> ChannelClient {
        ChannelClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        )
    }

    /// Get encryption settings for a channel.
    ///
    /// Looks up channel locally first, fetches from server if not found.
    /// Returns (is_dm, require_encryption) tuple.
    pub(crate) async fn get_channel_encryption_settings(
        &self,
        channel_id: &ChannelId,
        channel_client: &mut ChannelClient,
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
            .await?;

        let is_dm = proto_channel.r#type == crate::proto::chat_v1::ChannelType::DirectMessage as i32;
        let require_encryption = proto_channel
            .config
            .map(|c| c.encrypted)
            .unwrap_or(false);

        Ok((is_dm, require_encryption))
    }

}

// Re-export helper functions used by other parts of the codebase
pub use self::core::{proto_to_domain_message, proto_to_domain_message_with_sender, proto_content_to_domain};
