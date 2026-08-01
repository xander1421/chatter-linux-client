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
mod group;
pub mod core;
pub mod conversion;
mod server_dm;
mod thread;
mod pin;
mod saved;

use std::sync::Arc;

use crate::crypto::signal::{SesameManager, SqliteSesameStore};
use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::ConversationType;
use crate::domain::message::Message;
use crate::error::{NetworkError, StorageError};
use crate::mailbox::group::GroupMailboxManager;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::message_client::MessageClient;
use crate::services::current_user::CurrentUserError;
use crate::services::ephemeral_service::EphemeralService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::signal_service::SignalService;
use crate::storage::Database;
use crate::storage::repositories::attachment_repo::AttachmentRepository;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::search_index::{SearchIndex, SearchResult};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::user_repo::UserRepository;

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

    /// Invalid message format or content.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// Group mailbox operation failed.
    #[error("mailbox error: {0}")]
    MailboxError(String),

    /// Attachment upload/download failed.
    #[error("attachment error: {0}")]
    AttachmentError(String),
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
    /// For sending to the peer's mailbox.
    pub mailbox_manager: &'a MailboxManager,
    /// Timestamp in nanoseconds (caller generates this for consistent UI/storage).
    pub sent_at: u64,
    /// Reply quote, when this send is a reply. Carries the `Quote` on the wire
    /// and stamps the local echo's reply fields (via `assemble_reply_parts`) so
    /// the receiver's bubble survives — same contract as the ServerDm path.
    pub quote: Option<&'a crate::services::gateway::outgoing::QuoteIntent>,
}

/// Result from sending a DM with file attachments.
pub struct DmSendResult {
    /// The stored message (None if bootstrap was initiated).
    pub message: Option<Message>,
    /// All uploaded attachment IDs.
    pub attachment_ids: Vec<AttachmentId>,
    /// Attachment IDs that are GIFs (for animation registration).
    pub gif_attachment_ids: Vec<AttachmentId>,
    /// Timestamp used for the message.
    pub sent_at: u64,
    /// True if this was a first DM and bootstrap was initiated.
    /// The message was queued and will be sent when bootstrap completes.
    pub bootstrap_initiated: bool,
}

/// Message service for all message operations.
///
/// Orchestrates message sending, editing, and deletion by coordinating
/// network client and local storage.
#[derive(Clone)]
pub struct MessageService {
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) database: Arc<Database>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) message_repo: Arc<MessageRepository>,
    pub(crate) channel_repo: Arc<ChannelRepository>,
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    pub(crate) user_repo: Arc<UserRepository>,
    pub(crate) signal_service: Arc<SignalService>,
    /// Group mailbox manager for NDSS '21 per-group mailboxes.
    pub(crate) group_mailbox_manager: Arc<GroupMailboxManager>,
    /// Sesame session manager for multi-session handling and message queueing.
    pub(crate) sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
    /// Full-text search index for local message search.
    pub(crate) search_index: Arc<SearchIndex>,
    /// Token manager for anonymous mailbox credentials.
    pub(crate) token_manager: Arc<TokenManager>,
    /// Event bus for emitting `DomainEvent::MessageReceived` after deferred
    /// group decrypt succeeds.
    pub(crate) event_bus: crate::events::EventBus,
    /// Local disappearing-message engine. Arms a message's local TTL on send
    /// (`track_armed`) so the SENDER's own copy is reaped once the timer
    /// elapses — the recipient half is armed on receive in the gateway/inbox.
    pub(crate) ephemeral_service: Arc<EphemeralService>,
    /// Group messages queued waiting on a `SenderKeyDistribution`. Keyed by
    /// `(channel_id, sender_id, sender_device_id)`. Drained by
    /// `retry_pending_group_decrypts` when the matching SKDM lands. Each entry
    /// carries the time it was parked so a never-arriving SKDM can't grow this
    /// map without bound (TTL + global cap enforced in
    /// `defer_pending_group_decrypt`).
    pub(crate) pending_group_decrypts: Arc<parking_lot::Mutex<
        std::collections::HashMap<
            (crate::domain::ids::ChannelId, crate::domain::ids::UserId, u32),
            Vec<(std::time::Instant, crate::proto::chat_v1::Message)>,
        >,
    >>,
}

impl MessageService {
    /// Create a new MessageService with dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        database: Arc<Database>,
        settings_repo: Arc<SettingsRepository>,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        attachment_repo: Arc<AttachmentRepository>,
        user_repo: Arc<UserRepository>,
        signal_service: Arc<SignalService>,
        group_mailbox_manager: Arc<GroupMailboxManager>,
        sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
        search_index: Arc<SearchIndex>,
        token_manager: Arc<TokenManager>,
        event_bus: crate::events::EventBus,
        ephemeral_service: Arc<EphemeralService>,
    ) -> Self {
        Self {
            grpc_pool,
            database,
            settings_repo,
            message_repo,
            channel_repo,
            attachment_repo,
            user_repo,
            signal_service,
            group_mailbox_manager,
            sesame_manager,
            search_index,
            token_manager,
            event_bus,
            ephemeral_service,
            pending_group_decrypts: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Get database reference (for internal DM operations).
    pub fn database(&self) -> Arc<Database> {
        self.database.clone()
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Internal client helpers
    // ─────────────────────────────────────────────────────────────────────────────

    pub(crate) fn message_client(&self) -> MessageClient {
        self.grpc_pool.message_client()
    }

    pub(crate) fn auth_client(&self) -> AuthClient {
        self.grpc_pool.auth_client()
    }

    pub(crate) fn channel_client(&self) -> ChannelClient {
        self.grpc_pool.channel_client()
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Local repository access methods
    // ─────────────────────────────────────────────────────────────────────────────

    /// Get messages for a channel from local storage.
    pub fn get_messages_local(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let messages = self.message_repo.list_for_channel(channel_id, conversation_type, before, limit)?;
        Ok(messages)
    }

    /// Get messages for a channel in chronological order (oldest first).
    ///
    /// Returns messages ready for chat display — no reversal needed by caller.
    pub fn get_messages_local_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let messages = self.message_repo.list_for_channel_asc(channel_id, conversation_type, before, limit)?;
        Ok(messages)
    }

    /// Get sent_at timestamps of messages from a specific peer in a conversation.
    ///
    /// Used for sending read receipts — returns the peer's message timestamps
    /// so the caller can acknowledge them without filtering in the UI layer.
    pub fn get_peer_message_timestamps(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        peer_id: &UserId,
        limit: u32,
    ) -> Result<Vec<u64>, MessageError> {
        self.message_repo
            .get_timestamps_from_sender(channel_id, conversation_type, peer_id, limit)
            .map_err(MessageError::from)
    }

    /// Get a message by sender ID and sent timestamp (Signal-style lookup).
    pub fn get_by_sender_timestamp(
        &self,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.message_repo
            .get_by_sender_timestamp(sender_id, sent_at)
            .map_err(MessageError::from)
    }

    /// Get a message by ID from local storage.
    pub fn get_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<Option<Message>, MessageError> {
        self.message_repo.get(message_id).map_err(MessageError::from)
    }

    /// Whether the parent message a reply quotes is missing or deleted.
    ///
    /// Signal-Desktop parity: a reply keeps its quoted snapshot even after the
    /// original is gone, but renders an "Original message not found" warning.
    /// Returns `true` when the parent row is absent from local storage or is a
    /// soft-delete tombstone (`is_deleted == true`). On a storage error we fall
    /// back to `false` so transient failures never produce a spurious warning.
    pub fn reply_parent_missing(&self, parent: &crate::domain::ids::MessageId) -> bool {
        match self.message_repo.get(parent) {
            Ok(Some(msg)) => msg.is_deleted,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Search messages using full-text search index.
    ///
    /// Returns search results with message IDs and channel IDs.
    /// Use `get_message_local` to fetch full message details.
    pub fn search_local(
        &self,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<Vec<SearchResult>, MessageError> {
        self.search_index
            .search(query, channel_id, limit)
            .map_err(MessageError::from)
    }

    /// Get thread replies from local storage.
    pub fn list_for_thread(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        // Resolve cursor MessageId to sent_at timestamp for unified repo
        let before_sent_at = if let Some(before_id) = before {
            self.message_repo
                .get(before_id)?
                .map(|msg| msg.sent_at)
        } else {
            None
        };
        self.message_repo
            .list_for_thread(thread_root_id, before_sent_at, limit)
            .map_err(MessageError::from)
    }

    /// Update a message in local storage.
    pub fn update_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo.update(message).map_err(MessageError::from)
    }

    /// Get pinned messages for a channel from local storage.
    pub fn get_pinned_messages_local(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, MessageError> {
        self.message_repo.list_pinned_all(channel_id, conversation_type).map_err(MessageError::from)
    }

    /// Set the pinned status of a message in local storage.
    /// Mark all unread messages in a conversation as read (sets is_read=1).
    /// Callers use this when the user is actively viewing a conversation so new
    /// incoming messages don't bump the unread badge.
    pub fn mark_conversation_read(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<(), MessageError> {
        self.message_repo.mark_conversation_read(conversation_id, conversation_type)?;
        Ok(())
    }

    pub fn set_message_pinned(
        &self,
        message_id: &crate::domain::ids::MessageId,
        is_pinned: bool,
    ) -> Result<(), MessageError> {
        if let Some(mut message) = self.message_repo.get(message_id)? {
            message.is_pinned = is_pinned;
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// Persist an edit to the database (update body + set is_edited flag).
    pub fn update_message_body(
        &self,
        message_id: &crate::domain::ids::MessageId,
        new_content: &crate::domain::message::MessageContent,
    ) -> Result<(), MessageError> {
        if let Some(mut message) = self.message_repo.get(message_id)? {
            message.content = new_content.clone();
            message.is_edited = true;
            message.edited_at = Some(chrono::Utc::now());
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// Add or remove a reaction on a message and persist to DB.
    pub fn update_reaction(
        &self,
        message_id: &crate::domain::ids::MessageId,
        emoji: &str,
        add: bool,
    ) -> Result<(), MessageError> {
        if let Some(mut message) = self.message_repo.get(message_id)? {
            if add {
                if let Some(rc) = message.reaction_counts.iter_mut().find(|r| r.emoji == emoji) {
                    rc.count += 1;
                } else {
                    message.reaction_counts.push(crate::domain::message::ReactionCount {
                        emoji: emoji.to_string(),
                        count: 1,
                    });
                }
            } else {
                if let Some(rc) = message.reaction_counts.iter_mut().find(|r| r.emoji == emoji) {
                    rc.count = rc.count.saturating_sub(1);
                }
                message.reaction_counts.retain(|r| r.count > 0);
            }
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// Add or remove a reaction on a message identified by (sender_id, sent_at_ns).
    /// Signal-style identification.
    pub fn update_reaction_by_sender(
        &self,
        sender_id: &crate::domain::ids::UserId,
        sent_at_ns: u64,
        emoji: &str,
        add: bool,
    ) -> Result<(), MessageError> {
        if let Some(mut message) = self.message_repo.get_by_sender_timestamp(sender_id, sent_at_ns)? {
            if add {
                if let Some(rc) = message.reaction_counts.iter_mut().find(|r| r.emoji == emoji) {
                    rc.count += 1;
                } else {
                    message.reaction_counts.push(crate::domain::message::ReactionCount {
                        emoji: emoji.to_string(),
                        count: 1,
                    });
                }
            } else {
                if let Some(rc) = message.reaction_counts.iter_mut().find(|r| r.emoji == emoji) {
                    rc.count = rc.count.saturating_sub(1);
                }
                message.reaction_counts.retain(|r| r.count > 0);
            }
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// Update delivery status for a message by sent_at timestamp.
    ///
    /// Used when receipts (delivered/read) are received.
    pub fn update_delivery_status(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status(channel_id, conversation_type, sent_at, status)
            .map_err(MessageError::from)
    }

    /// Update delivery status by conversation + timestamp (scoped to one conversation).
    pub fn update_delivery_status_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamp(conversation_id, conversation_type, sent_at, status)
            .map_err(MessageError::from)
    }

    /// Mark the message with the given sent-at timestamp as read (multi-device
    /// read-sync). See `UnifiedMessageRepository::mark_read_by_timestamp`.
    pub fn mark_read_by_timestamp(&self, sent_at: u64) -> Result<bool, MessageError> {
        self.message_repo
            .mark_read_by_timestamp(sent_at)
            .map_err(MessageError::from)
    }

    /// Update delivery status by timestamp with automatic conversation lookup.
    ///
    /// For callers that lack conversation context (e.g. cross-channel receipts).
    /// Looks up matching messages first, then updates each by primary key.
    pub fn update_delivery_status_by_timestamp_with_lookup(
        &self,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamp_with_lookup(sent_at, status)
            .map_err(MessageError::from)
    }

    /// Delete a message from local storage.
    pub fn delete_message_local(&self, message_id: &crate::domain::ids::MessageId) -> Result<(), MessageError> {
        self.message_repo.delete(message_id).map_err(MessageError::from)
    }

    /// Store a message in local storage (async).
    pub async fn store_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo.insert(message).map_err(MessageError::from)
    }

    /// Store a message in local storage (sync).
    pub fn store_message_local(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo.insert(message).map_err(MessageError::from)
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, MessageError> {
        use crate::services::current_user::CurrentUserProvider;
        self.settings_repo
            .current_user_id()
            .map_err(|_| MessageError::NotLoggedIn)
    }

    /// Persist a reaction received via inbox polling.
    ///
    /// Returns `true` if the reaction was newly inserted, `false` if it was
    /// already known (from a prior poll cycle, or because the reaction was
    /// already attached to the message snapshot we fetched). Inbox handlers
    /// gate `DomainEvent::ReactionAdded` on this so the UI doesn't double-
    /// count when the snapshot + delta both carry the same reaction.
    pub fn record_reaction(
        &self,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
    ) -> Result<bool, MessageError> {
        let current = self.current_user_id()?;
        if remove {
            self.message_repo
                .remove_reaction(target_sender, target_sent_at_ns, reactor, &current, emoji)
                .map_err(MessageError::from)
        } else {
            self.message_repo
                .add_reaction(target_sender, target_sent_at_ns, reactor, &current, emoji)
                .map_err(MessageError::from)
        }
    }

    /// Classify a raw `(channel_id, conversation_type, peer_id)` triple into
    /// the typed `ActiveConversation` variant it represents.
    ///
    /// - AnonDm/AnonGroup `ConversationType`s map straight to their variants.
    /// - ServerDm → channel-free `ServerDm { peer_id }` (the peer is the
    ///   passed `peer_id`, or the `channel_id`'s UUID which equals it).
    /// - MailboxDm + known peer → `MailboxDm` (still channel-scoped).
    /// - Otherwise → `Channel`.
    ///
    /// Lives here because UI callers used to inline this across multiple
    /// feature handlers.
    /// Derive the `ConversationType` for a channel/DM id from the local channel
    /// row. Defaults to `Channel` when the row isn't synced yet. Thin accessor
    /// so callers outside `message/` (e.g. call-history storage) don't reach
    /// into `channel_repo` directly.
    pub fn conversation_type(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> crate::domain::message::ConversationType {
        conversion::conversation_type_for_channel(&self.channel_repo, channel_id)
    }

    pub fn classify_active_conversation(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        peer_id: Option<crate::domain::ids::UserId>,
    ) -> crate::state::ActiveConversation {
        use crate::domain::message::ConversationType;
        use crate::state::ActiveConversation;

        match conversation_type {
            ConversationType::AnonDm => {
                ActiveConversation::AnonDm(crate::domain::anon_dm::AnonDmId::from_uuid(*channel_id.as_uuid()))
            }
            ConversationType::AnonGroup => {
                ActiveConversation::AnonGroup(crate::domain::anon_group::AnonGroupId::from_uuid(*channel_id.as_uuid()))
            }
            ConversationType::ServerDm => {
                // Channel-free DM: keyed by peer. conversation_id == peer
                // user_id, so recover the peer from either source.
                let peer = peer_id.unwrap_or_else(|| {
                    crate::domain::ids::UserId::from_uuid(*channel_id.as_uuid())
                });
                ActiveConversation::ServerDm { peer_id: peer }
            }
            ConversationType::MailboxDm => match peer_id {
                Some(p) => ActiveConversation::MailboxDm { channel_id: *channel_id, peer_id: p },
                None => ActiveConversation::Channel(*channel_id),
            },
            _ => ActiveConversation::Channel(*channel_id),
        }
    }

    /// Check if the locally-logged-in user has reacted with an emoji on a message.
    ///
    /// Signal-style message identification via (sender_id, sent_at_ns). Reads
    /// the per-user blob cache on the message row — authoritative for both
    /// anon (rebuilt locally) and regular (delivered from server) paths.
    pub fn user_has_reacted(
        &self,
        sender_id: &UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .user_has_reacted(sender_id, sent_at_ns, emoji)
            .map_err(MessageError::from)
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

        // Server channels are never DMs anymore (DMs are channel-free,
        // Signal-style — the DIRECT_MESSAGE channel type no longer exists
        // on the wire). A server-fetched channel row is always a real
        // group/public/saved-messages channel.
        let is_dm = false;
        let require_encryption = proto_channel
            .config
            .map(|c| c.encrypted)
            .unwrap_or(false);

        Ok((is_dm, require_encryption))
    }

}

// Re-export helper functions used by other parts of the codebase
pub use self::core::{proto_to_domain_message, proto_to_domain_message_with_sender, proto_content_to_domain};
