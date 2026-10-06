//! Message service - modular implementation.
//!
//! Send, edit, delete, forward, and quote messages.
//!
//! This module is organized into focused submodules:
//! - `dm` - DM operations (Signal Protocol encryption)
//! - `group` - Group/sender key operations
//! - `core` - Core message operations (send, edit, delete, reply, forward)
//! - `pin` - Pin operations
//! - `saved` - Saved messages (Note to Self)

mod dm;
pub(crate) use dm::null_message_content;
mod dm_bootstrap;
mod dm_resend;
mod group;
pub use group::ChannelStateSync;
pub(crate) use group::GroupStateGate;
mod call_event;
pub mod channel_inbound;
pub mod conversion;
pub mod core;
mod pin;
mod saved;
mod server_dm;
pub use call_event::ChannelCallPhase;
mod receipts;
pub use receipts::advance_delivery_status;

use std::sync::Arc;

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
use crate::services::signal::SignalService;
use crate::storage::Database;
use crate::storage::repositories::attachment_repo::AttachmentRepository;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::search_index::{SearchIndex, SearchResult};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::repositories::{MessageRepository, PendingDmRepository};

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

    /// Resync truth exists but its ingestion is not yet durable.
    #[error("resync ingestion not yet applied")]
    ResyncPending,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// The recipient's identity key changed and has not been accepted, so there
    /// is no key we may encrypt to.
    ///
    /// **Kept out of [`Self::EncryptionFailed`] on purpose.** That variant's
    /// user-facing text is "Encryption failed — try again", and retrying this
    /// cannot ever succeed: `ensure_session` refuses for as long as the change
    /// is parked, and only an explicit acceptance clears it. Stringifying it
    /// into `EncryptionFailed` is precisely how the user ended up being told to
    /// retry a dead conversation.
    #[error("recipient identity changed and has not been accepted")]
    RecipientIdentityChanged {
        /// The peer whose key changed, so the UI can point at the right
        /// conversation.
        peer_id: String,
    },

    /// The server could not prove the recipient's identity key is in the Key
    /// Transparency log, so the session was refused (see
    /// [`crate::error::CryptoError::NotInTransparencyLog`]). Not
    /// [`Self::EncryptionFailed`] for the same reason as
    /// [`Self::RecipientIdentityChanged`]: retrying fails identically until the
    /// server proves the key.
    #[error("recipient's identity key is not in the Key Transparency log")]
    RecipientKeyUnverified {
        /// The peer whose key could not be verified.
        peer_id: String,
    },

    /// DM conversation error.
    #[error("dm error: {0}")]
    DmError(String),

    /// DM bootstrap was initiated - message will be sent after key exchange completes.
    #[error("DM bootstrap initiated - secure channel being established")]
    BootstrapInitiated,

    /// Shield mode, first contact: none of the peer's devices is cached, and
    /// finding them takes an identified fetch Shield forbids.
    #[error("{0} is not reachable privately yet — send them one message outside Shield first")]
    NotReachablePrivately(UserId),

    /// Invalid message format or content.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// Group mailbox operation failed.
    #[error("mailbox error: {0}")]
    MailboxError(String),

    /// Attachment upload/download failed.
    #[error("attachment error: {0}")]
    AttachmentError(String),

    /// The channel's verified membership (Design B) does not allow sender-key
    /// traffic: no verified state yet (`status: None`), a change that failed
    /// verification, or a detected fork. No SKDM goes out and nothing is
    /// group-encrypted until it clears. Retrying cannot help.
    #[error("channel membership could not be verified ({})", match status {
        None => "no verified state",
        Some(crate::services::channel::verified_state::VerificationStatus::Fork) => "fork detected",
        Some(_) => "verification failed",
    })]
    MembershipNotVerified {
        channel_id: ChannelId,
        status: Option<crate::services::channel::verified_state::VerificationStatus>,
    },
}

impl MessageError {
    /// Classify a send-side [`ServiceError`] from the Signal layer.
    ///
    /// One place, because every send path that touches sessions can hit the
    /// identity-change refusal and each of them used to flatten it into
    /// `EncryptionFailed(format!("…: {e}"))` — which is a string, so the UI
    /// could only offer "try again".
    pub(crate) fn from_signal_send(err: crate::error::ServiceError, context: &str) -> Self {
        match err {
            crate::error::ServiceError::Network(error) => Self::Network(error),
            crate::error::ServiceError::NotLoggedIn => Self::NotLoggedIn,
            crate::error::ServiceError::Validation(reason) => Self::InvalidContent(reason),
            crate::error::ServiceError::Crypto(crate::error::CryptoError::UntrustedIdentity {
                peer_id,
                ..
            }) => Self::RecipientIdentityChanged { peer_id },
            crate::error::ServiceError::Crypto(
                crate::error::CryptoError::NotInTransparencyLog { peer_id },
            ) => Self::RecipientKeyUnverified { peer_id },
            error => Self::EncryptionFailed(format!("{context}: {error}")),
        }
    }
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

/// Result of a friend-DM send (`send_server_dm`).
pub struct DmSendResult {
    /// The stored message.
    pub message: Option<Message>,
    /// All uploaded attachment IDs.
    pub attachment_ids: Vec<AttachmentId>,
    /// Attachment IDs that are GIFs (for animation registration).
    pub gif_attachment_ids: Vec<AttachmentId>,
    /// Timestamp used for the message.
    pub sent_at: u64,
    /// True if the Shield mailbox with this friend was not established yet:
    /// the message was queued and goes out when the bootstrap completes.
    pub bootstrap_initiated: bool,
    /// Delivery failed: the message is stored as `"failed"` for a retry
    /// (`retry_server_dm`), and this says why.
    pub send_error: Option<MessageError>,
}

/// What a post-bootstrap flush did with the queue it drained (either side:
/// `MessageService::send_pending_messages`, the responder's flush in
/// `dm_bootstrap_handler`).
#[derive(Debug, Default)]
pub struct PendingFlush {
    /// Bubble timestamps (`DataMessage.timestamp`) of the messages that went out.
    pub sent: Vec<u64>,
    /// Bubble timestamps of the messages that did not.
    pub failed: Vec<u64>,
    /// Why, when anything failed. The queue is drained either way, so this is
    /// a message lost unless it is announced.
    pub error: Option<String>,
}

impl PendingFlush {
    /// Announce the outcome: `DmBootstrapCompleted` for what went out (it also
    /// ends the UI's "Setting up secure channel…"), `MessageSendFailed` for
    /// what did not — each bubble ends as "sent" or "failed", never "sending".
    pub fn emit(self, peer_id: UserId, event_bus: &crate::events::EventBus) {
        if self.error.is_none() || !self.sent.is_empty() {
            event_bus.emit_domain(crate::events::DomainEvent::DmBootstrapCompleted {
                peer_id,
                sent_timestamps: self.sent,
            });
        }
        if let Some(error) = self.error {
            event_bus.emit_domain(crate::events::DomainEvent::MessageSendFailed {
                channel_id: ChannelId::from(peer_id),
                error,
                message_timestamps: self.failed,
            });
        }
    }

    /// Record one queued message's outcome. `bubble` is its
    /// `DataMessage.timestamp`; control content (receipts, profile-key
    /// handshakes) has no bubble and only contributes to `error`.
    pub(crate) fn record(&mut self, bubble: Option<u64>, result: Result<(), String>) {
        match result {
            Ok(()) => self.sent.extend(bubble),
            Err(e) => {
                self.failed.extend(bubble);
                self.error = Some(e);
            }
        }
    }
}

/// The bubble a queued `Content` belongs to: its `DataMessage.timestamp`.
pub(crate) fn queued_bubble_timestamp(content: &crate::proto::content_v1::Content) -> Option<u64> {
    match &content.content {
        Some(crate::proto::content_v1::content::Content::DataMessage(dm)) => Some(dm.timestamp),
        _ => None,
    }
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
    /// DMs held back until a first-contact bootstrap completes.
    pub(crate) pending_dms: Arc<PendingDmRepository>,
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
    /// NDSS'21 ephemeral-mailbox transport — a friend DM's transport while
    /// Shield mode is on (see `send_friend_dm_content`).
    pub(crate) mailbox_manager: Arc<MailboxManager>,
    /// Shield mode. The one bit that picks a friend DM's transport; shared
    /// with `ChatterClient`, which is what flips it.
    pub(crate) shield_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Group messages queued waiting on a `SenderKeyDistribution`. Keyed by
    /// `(channel_id, sender_id, sender_device_id)`. Drained by
    /// `retry_pending_group_decrypts` when the matching SKDM lands. Each entry
    /// carries the time it was parked so a never-arriving SKDM can't grow this
    /// map without bound (TTL + global cap enforced in
    /// `defer_pending_group_decrypt`).
    pub(crate) pending_group_decrypts: Arc<
        parking_lot::Mutex<
            std::collections::HashMap<
                (
                    crate::domain::ids::ChannelId,
                    crate::domain::ids::UserId,
                    u32,
                ),
                Vec<(std::time::Instant, crate::proto::chat_v1::Message)>,
            >,
        >,
    >,
    /// Backoff gate for outgoing `SenderKeyRequest`s — see
    /// [`MessageService::should_request_sender_key`].
    pub(crate) sender_key_requests: Arc<parking_lot::Mutex<group::SenderKeyRequestGate>>,
    /// Group messages we already sent a `DecryptionErrorMessage` for, keyed
    /// `(message id, sender device)`: one retry request per message per device.
    pub(crate) group_retry_requests:
        Arc<parking_lot::Mutex<std::collections::HashSet<(String, u32)>>>,
    /// Group messages this device asked a sender to resend — shared with
    /// `MessageGateway`, which honours a resend's carried server id only
    /// against an entry here, and consumes it.
    pub(crate) solicited_resends: SolicitedResends,
    /// Our own device list as the server last gave it: (fetched at, epoch
    /// nanos; device ids). See [`MessageService::is_retired_device`].
    pub(crate) own_devices: Arc<tokio::sync::Mutex<Option<(u64, std::collections::HashSet<u32>)>>>,
    /// Peer devices the server refused a send to as "unknown destination
    /// device", and when. Timed, because the server reuses a removed device's
    /// id (`MAX(device_id) + 1`).
    pub(crate) retired_peer_devices:
        Arc<parking_lot::Mutex<std::collections::HashMap<(UserId, u32), std::time::Instant>>>,
    /// Rate limit on resend requests for messages the parked-decrypt cap
    /// evicted — see [`group::eviction_resend_due`].
    pub(crate) eviction_resends: Arc<parking_lot::Mutex<group::EvictionResendGate>>,
    /// The channel service's change fetcher (W4), called when a peer's
    /// `(revision, state_hash)` stamp is ahead of our verified state. See
    /// [`group::ChannelStateSync`].
    pub(crate) channel_state_sync:
        Arc<parking_lot::RwLock<Option<Arc<dyn group::ChannelStateSync>>>>,
}

/// `(channel, sender, server sent_at in epoch millis)` of every group message
/// this device sent a `DecryptionErrorMessage` for. A pairwise resend may file
/// itself under the server id it carries only when it answers one of these:
/// otherwise any member could squat another member's id before the real copy
/// lands.
///
/// Persisted (settings key `SOLICITED_RESENDS`) so an answer that arrives
/// after an app restart still keeps the server id. An entry lives as long as
/// the sender will resend — 24 h from `sent_at` (`resend_on_decryption_error`).
#[derive(Clone)]
pub(crate) struct SolicitedResends {
    set: Arc<parking_lot::Mutex<std::collections::HashSet<SolicitedResend>>>,
    settings: Arc<SettingsRepository>,
}

type SolicitedResend = (ChannelId, UserId, u64);

/// How long a sender answers a resend request; older entries are dropped.
const SOLICITED_RESEND_TTL_MS: u64 = 24 * 60 * 60 * 1000;

impl SolicitedResends {
    pub(crate) fn load(settings: Arc<SettingsRepository>) -> Self {
        use crate::storage::repositories::settings_repo::keys;
        let stored: Vec<SolicitedResend> = settings
            .get(keys::SOLICITED_RESENDS)
            .inspect_err(|e| tracing::warn!(error = %e, "Could not load solicited resends"))
            .ok()
            .flatten()
            .unwrap_or_default();
        let this = Self {
            set: Arc::new(parking_lot::Mutex::new(stored.into_iter().collect())),
            settings,
        };
        this.persist(&mut this.set.lock());
        this
    }

    /// Record a request for `(channel, sender, sent_at ms)`.
    pub(crate) fn insert(&self, entry: SolicitedResend) {
        let mut set = self.set.lock();
        // cleared wholesale at 4096 entries; a forgotten entry
        // files that one resend under a fresh local id.
        if set.len() >= 4096 {
            set.clear();
        }
        set.insert(entry);
        self.persist(&mut set);
    }

    /// Consume the request a resend answers; `false` when there was none.
    pub(crate) fn take(&self, entry: &SolicitedResend) -> bool {
        let mut set = self.set.lock();
        let found = set.remove(entry);
        if found {
            self.persist(&mut set);
        }
        found
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, entry: &SolicitedResend) -> bool {
        self.set.lock().contains(entry)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.set.lock().is_empty()
    }

    /// Drop expired entries and write the rest. A failed write only costs a
    /// resend after a restart its server id.
    fn persist(&self, set: &mut std::collections::HashSet<SolicitedResend>) {
        use crate::storage::repositories::settings_repo::keys;
        let now_ms = crate::util::time::TimestampNanos::now().as_u64() / 1_000_000;
        set.retain(|&(_, _, sent_at_ms)| {
            now_ms.saturating_sub(sent_at_ms) < SOLICITED_RESEND_TTL_MS
        });
        let entries: Vec<&SolicitedResend> = set.iter().collect();
        if let Err(e) = self.settings.set(keys::SOLICITED_RESENDS, &entries) {
            tracing::warn!(error = %e, "Could not persist solicited resends");
        }
    }
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
        pending_dms: Arc<PendingDmRepository>,
        search_index: Arc<SearchIndex>,
        token_manager: Arc<TokenManager>,
        event_bus: crate::events::EventBus,
        ephemeral_service: Arc<EphemeralService>,
        mailbox_manager: Arc<MailboxManager>,
    ) -> Self {
        let solicited_resends = SolicitedResends::load(settings_repo.clone());
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
            pending_dms,
            search_index,
            token_manager,
            event_bus,
            ephemeral_service,
            mailbox_manager,
            shield_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_group_decrypts: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            sender_key_requests: Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            group_retry_requests: Arc::new(parking_lot::Mutex::new(
                std::collections::HashSet::new(),
            )),
            solicited_resends,
            own_devices: Arc::new(tokio::sync::Mutex::new(None)),
            retired_peer_devices: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            eviction_resends: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            channel_state_sync: Arc::new(parking_lot::RwLock::new(None)),
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
        let messages =
            self.message_repo
                .list_for_channel(channel_id, conversation_type, before, limit)?;
        Ok(self.resolve_stored_quotes(messages))
    }

    /// Re-resolve quote references at load time, including parents received after
    /// the reply. Persist only the resolved reference; retain the quoted snapshot.
    fn resolve_stored_quotes(&self, mut messages: Vec<Message>) -> Vec<Message> {
        for message in &mut messages {
            if let Ok(Some(parent)) = self.message_repo.resolve_reply_parent(message) {
                if message.reply_to != Some(parent) {
                    message.reply_to = Some(parent);
                    if let Err(error) = self.message_repo.update_reply_parent(&message.id, &parent)
                    {
                        tracing::warn!(%error, "Could not persist resolved quote");
                    }
                }
            }
        }
        messages
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
        let messages =
            self.message_repo
                .list_for_channel_asc(channel_id, conversation_type, before, limit)?;
        Ok(self.resolve_stored_quotes(messages))
    }

    /// History older than `before`, oldest first: one page, or with `until`
    /// as many pages as it takes to reach that message (a jump to a quote,
    /// a search hit or a pin that is older than what is loaded). Empty when
    /// there is nothing older, or when `until` is not a message of this
    /// conversation's main list — never the whole history for a target that
    /// is not in it.
    pub fn get_older_messages_local_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: &crate::domain::ids::MessageId,
        until: Option<&crate::domain::ids::MessageId>,
        page_size: u32,
    ) -> Result<Vec<Message>, MessageError> {
        if let Some(target) = until {
            let reachable = self.message_repo.get(target)?.is_some_and(|m| {
                m.conversation_id == *channel_id
                    && m.conversation_type == *conversation_type
                    && m.thread_id.is_none()
            });
            if !reachable {
                return Ok(Vec::new());
            }
        }
        let mut older = Vec::new();
        let mut before = *before;
        loop {
            let page = self.message_repo.list_for_channel_asc(
                channel_id,
                conversation_type,
                Some(&before),
                page_size,
            )?;
            let Some(first) = page.first() else { break };
            before = first.id;
            let reached = until.is_none_or(|target| page.iter().any(|m| m.id == *target));
            older.splice(0..0, page);
            if reached {
                break;
            }
        }
        Ok(self.resolve_stored_quotes(older))
    }

    /// Reply count per thread root for a conversation (drives the "N replies"
    /// indicator on root bubbles).
    pub fn thread_reply_counts(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<std::collections::HashMap<crate::domain::ids::MessageId, u32>, MessageError> {
        self.message_repo
            .reply_counts_for_channel(channel_id, conversation_type)
            .map_err(MessageError::from)
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
        Ok(self
            .resolve_stored_quotes(self.message_repo.get(message_id)?.into_iter().collect())
            .pop())
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
            self.message_repo.get(before_id)?.map(|msg| msg.sent_at)
        } else {
            None
        };
        self.message_repo
            .list_for_thread(thread_root_id, before_sent_at, limit)
            .map(|messages| self.resolve_stored_quotes(messages))
            .map_err(MessageError::from)
    }

    /// Number of replies in a thread (local storage).
    pub fn count_for_thread(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
    ) -> Result<u32, MessageError> {
        self.message_repo
            .count_for_thread(thread_root_id)
            .map_err(MessageError::from)
    }

    /// Update a message in local storage.
    pub fn update_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .update(message)
            .map_err(MessageError::from)
    }

    /// Get pinned messages for a channel from local storage.
    pub fn get_pinned_messages_local(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, MessageError> {
        self.message_repo
            .list_pinned_all(channel_id, conversation_type)
            .map(|messages| self.resolve_stored_quotes(messages))
            .map_err(MessageError::from)
    }

    /// Set the pinned status of a message in local storage.
    /// Mark all unread messages in a conversation as read (sets is_read=1).
    /// Callers use this when the user is actively viewing a conversation so new
    /// incoming messages don't bump the unread badge.
    /// Mark a conversation read and report the boundary that was cleared.
    /// See [`crate::domain::message::UnreadAnchor`] for why the two are one call.
    pub fn mark_conversation_read(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<crate::domain::message::UnreadAnchor, MessageError> {
        let (anchor, read) = self
            .message_repo
            .mark_conversation_read_returning(conversation_id, conversation_type)?;
        self.sync_read(conversation_id, conversation_type, read);
        // Unconditional: an anonymous message has no sender, so `read` can be
        // empty while its notification is still up.
        self.event_bus
            .emit_domain(crate::events::DomainEvent::ConversationsRead {
                conversation_ids: vec![*conversation_id],
                from_other_device: false,
            });
        Ok(anchor)
    }

    /// Tell our other devices what was just read here (Signal's
    /// `SyncMessage.Read`), whatever the read-receipt setting — that setting
    /// is about what the sender learns, not our own devices. Friend DMs and
    /// channels only: anonymous conversations stay off our own devices' wire.
    /// Fire-and-forget; a failed sync leaves the other devices unread.
    fn sync_read(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        read: Vec<(UserId, u64)>,
    ) {
        use crate::proto::content_v1::{Content, ReadSync, ReadSyncEntry, content};

        if !matches!(
            conversation_type,
            ConversationType::ServerDm | ConversationType::Channel
        ) {
            return;
        }
        let Ok(me) = self.current_user_id() else {
            return;
        };
        let messages: Vec<ReadSyncEntry> = read
            .into_iter()
            .filter(|(sender, _)| *sender != me)
            .map(|(sender, sent_at)| ReadSyncEntry {
                conversation_id: conversation_id.to_string(),
                sender_aci: sender.to_string(),
                sent_timestamp: sent_at,
            })
            .collect();
        if messages.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("ReadSync: no runtime to send on; other devices stay unread");
            return;
        };
        let sync = Content {
            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
            content: Some(content::Content::ReadSync(ReadSync { messages })),
            ..Default::default()
        };
        let service = self.clone();
        runtime.spawn(async move {
            let devices = service
                .sync_to_own_devices(&mut service.auth_client(), sync)
                .await;
            tracing::debug!(devices, "ReadSync sent to our other devices");
        });
    }

    /// Apply a `ReadSync` from another of our devices. Returns the
    /// conversations in which something was newly read. Sends nothing back:
    /// only a local read (`mark_conversation_read`) syncs.
    pub fn apply_read_sync(
        &self,
        sync: &crate::proto::content_v1::ReadSync,
    ) -> Result<Vec<ChannelId>, MessageError> {
        let me = self.current_user_id()?;
        let reads: Vec<(ChannelId, UserId, u64)> = sync
            .messages
            .iter()
            .filter_map(|m| {
                Some((
                    m.conversation_id.parse().ok()?,
                    m.sender_aci.parse().ok()?,
                    m.sent_timestamp,
                ))
            })
            .collect();
        Ok(self.message_repo.mark_read_synced(&reads, &me)?)
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

    /// Persist an edit of one of OUR messages made on this device (update
    /// body + set is_edited flag). Anyone else's edit is persisted, ordered
    /// and dated by [`apply_dm_edit`](Self::apply_dm_edit) /
    /// [`apply_channel_edit`](Self::apply_channel_edit) when it arrives; the
    /// `MessageEdited` it announces must not be stored again here with
    /// `edited_at = now`.
    pub fn update_message_body(
        &self,
        message_id: &crate::domain::ids::MessageId,
        new_content: &crate::domain::message::MessageContent,
    ) -> Result<(), MessageError> {
        let me = self.current_user_id()?;
        if let Some(mut message) = self.message_repo.get(message_id)? {
            if message.sender_id != Some(me)
                || (message.is_edited && message.content == *new_content)
            {
                return Ok(());
            }
            message.content = new_content.clone();
            message.is_edited = true;
            message.edited_at = Some(chrono::Utc::now());
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

    /// How far from its message an edit may be and still apply
    /// ([`message_modifier_repo::EDIT_MAX_AGE_NS`](crate::storage::repositories::message_modifier_repo::EDIT_MAX_AGE_NS)).
    pub const EDIT_MAX_AGE_NS: u64 =
        crate::storage::repositories::message_modifier_repo::EDIT_MAX_AGE_NS;

    /// Where a modifier from `author` in a friend DM may land: the DM with
    /// them — or, when the author is us (a copy from our own other device),
    /// whichever DM our message is in.
    fn dm_modifier_scope(&self, author: &UserId) -> Option<ChannelId> {
        (self.current_user_id().ok().as_ref() != Some(author)).then(|| ChannelId::from(*author))
    }

    /// A friend DM edit from `peer` of its message sent at `target_sent_at`;
    /// see [`apply_edit`](Self::apply_edit).
    pub fn apply_dm_edit(
        &self,
        peer: &UserId,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_edit(
            peer,
            self.dm_modifier_scope(peer),
            target_sent_at,
            new_body,
            edit_ts,
            server_ts,
        )
    }

    /// A channel edit from `sender` of its message sent at `target_sent_at`;
    /// see [`apply_edit`](Self::apply_edit).
    pub fn apply_channel_edit(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_edit(
            sender,
            Some(*channel_id),
            target_sent_at,
            new_body,
            edit_ts,
            server_ts,
        )
    }

    /// Receive `author`'s edit of its own message sent at `target_sent_at`:
    /// [`MessageModifierRepository::apply_received_edit`](crate::storage::repositories::message_modifier_repo::MessageModifierRepository::apply_received_edit),
    /// the one choke point every received edit goes through.
    fn apply_edit(
        &self,
        author: &UserId,
        conversation: Option<ChannelId>,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        Ok(
            crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
                self.database.clone(),
            )
            .apply_received_edit(
                &self.message_repo,
                author,
                conversation,
                target_sent_at,
                new_body,
                edit_ts,
                server_ts,
            )?,
        )
    }

    /// A friend DM delete-for-everyone from `peer` of its message sent at
    /// `target_sent_at`; see [`apply_delete`](Self::apply_delete).
    pub fn apply_dm_delete(
        &self,
        peer: &UserId,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_delete(peer, self.dm_modifier_scope(peer), target_sent_at)
    }

    /// A channel delete-for-everyone from `sender` of its message sent at
    /// `target_sent_at`; see [`apply_delete`](Self::apply_delete).
    pub fn apply_channel_delete(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_delete(sender, Some(*channel_id), target_sent_at)
    }

    /// Delete the message `author`'s delete-for-everyone names and return it,
    /// for the caller to announce (`DomainEvent::MessageDeleted`). The row is
    /// gone BEFORE the announcement, as an edit is applied before its
    /// `MessageEdited`: a subscriber that rebuilds the chat-list preview from
    /// storage on the event must not still find the deleted message there.
    /// One not here yet is held (Signal-Desktop `Deletes.preload.ts:28`), so
    /// it is never stored when it arrives (`message_modifier_repo`).
    fn apply_delete(
        &self,
        author: &UserId,
        conversation: Option<ChannelId>,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        match self
            .message_repo
            .get_by_sender_timestamp(author, target_sent_at)?
        {
            Some(message) if conversation.is_none_or(|c| c == message.conversation_id) => {
                self.message_repo.delete(&message.id)?;
                Ok(Some(message))
            }
            Some(_) => Ok(None),
            None => {
                crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
                    self.database.clone(),
                )
                .queue_delete(author, target_sent_at, conversation, crate::util::time::TimestampNanos::now().as_u64())?;
                Ok(None)
            }
        }
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
                if let Some(rc) = message
                    .reaction_counts
                    .iter_mut()
                    .find(|r| r.emoji == emoji)
                {
                    rc.count += 1;
                } else {
                    message
                        .reaction_counts
                        .push(crate::domain::message::ReactionCount {
                            emoji: emoji.to_string(),
                            count: 1,
                        });
                }
            } else {
                if let Some(rc) = message
                    .reaction_counts
                    .iter_mut()
                    .find(|r| r.emoji == emoji)
                {
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
            .update_delivery_status_by_timestamp(
                conversation_id,
                conversation_type,
                sent_at,
                status,
            )
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

    /// Set the stored delivery status of our friend-DM messages in
    /// `conversation_id` sent at `timestamps` — how a background send (the
    /// post-bootstrap flush) records `"sent"` or `"failed"` for bubbles it
    /// stored as `"sending"`.
    pub fn set_friend_dm_delivery_status(
        &self,
        conversation_id: &ChannelId,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamps(
                conversation_id,
                &ConversationType::ServerDm,
                timestamps,
                status,
            )
            .map_err(MessageError::from)
    }

    /// Delete a message from local storage.
    pub fn delete_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<(), MessageError> {
        self.message_repo
            .delete(message_id)
            .map_err(MessageError::from)
    }

    /// Store a message in local storage (async).
    pub async fn store_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .insert(message)
            .map_err(MessageError::from)
    }

    /// Store a message in local storage (sync).
    pub fn store_message_local(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .insert(message)
            .map_err(MessageError::from)
    }

    /// [`store_message_local`](Self::store_message_local) for a record whose
    /// id is its dedup key: `false` when it was already stored.
    pub fn store_message_local_once(&self, message: &Message) -> Result<bool, MessageError> {
        self.message_repo
            .insert_or_ignore(message)
            .map_err(MessageError::from)
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, MessageError> {
        use crate::services::current_user::CurrentUserProvider;
        self.settings_repo
            .current_user_id()
            .map_err(|_| MessageError::NotLoggedIn)
    }

    /// This device's id; 0 when unknown. Only labels a sync copy, so its
    /// absence must not fail a send.
    pub(super) fn current_device_id_or_zero(&self) -> u32 {
        self.settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// Persist a reaction. Does no conversation binding: a peer's reaction
    /// from the inbox goes through [`Self::record_inbound_reaction`].
    ///
    /// Returns `true` if the reaction was newly inserted, `false` if it was
    /// already known (from a prior poll cycle, or because the reaction was
    /// already attached to the message snapshot we fetched). Inbox handlers
    /// gate `DomainEvent::ReactionAdded` on this so the UI doesn't double-
    /// count when the snapshot + delta both carry the same reaction.
    pub fn record_reaction(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
    ) -> Result<bool, MessageError> {
        let current = self.current_user_id()?;
        if remove {
            self.message_repo
                .remove_reaction(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                )
                .map_err(MessageError::from)
        } else {
            self.message_repo
                .add_reaction(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                )
                .map_err(MessageError::from)
        }
    }

    pub fn record_reaction_at(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, MessageError> {
        let current = self.current_user_id()?;
        if remove {
            self.message_repo
                .remove_reaction_at(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                    action_ts,
                )
                .map_err(MessageError::from)
        } else {
            self.message_repo
                .add_reaction_at(
                    scope,
                    target_sender,
                    target_sent_at_ns,
                    reactor,
                    &current,
                    emoji,
                    action_ts,
                )
                .map_err(MessageError::from)
        }
    }

    /// Resolve the target of an inbound peer reaction or pin, or `None` when
    /// it is not here or must be dropped.
    ///
    /// `(target_author, sent_at)` names a message in *any* conversation, so a
    /// peer who learned one could otherwise react to or pin it from a
    /// conversation they are not in. Signal-Desktop
    /// (`ts/messageModifiers/Reactions.preload.ts`, `isMessageAMatchForReaction`)
    /// accepts a modifier from our own devices for any message we hold, and from
    /// anyone else only if they are a valid sender in the target's
    /// conversation. Here the target must also live in `conversation` — the
    /// one the modifier arrived in (`ChannelId::from(peer)` for a friend DM,
    /// the carried channel id for a channel). A modifier whose target is not
    /// here yet is held by [`Self::record_inbound_reaction`] /
    /// [`Self::resolve_inbound_pin`]; `message_modifier_repo::may_modify`
    /// applies this same rule when the target arrives.
    pub fn resolve_inbound_target(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
    ) -> Result<Option<Message>, MessageError> {
        Ok(
            match self.lookup_inbound_target(
                conversation,
                actor,
                target_author,
                target_sent_at_ns,
            )? {
                InboundTarget::Allowed(target) => Some(target),
                InboundTarget::NotHeld | InboundTarget::Refused => None,
            },
        )
    }

    fn lookup_inbound_target(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
    ) -> Result<InboundTarget, MessageError> {
        let Some(target) = self.get_by_sender_timestamp(target_author, target_sent_at_ns)? else {
            return Ok(InboundTarget::NotHeld);
        };
        if *actor == self.current_user_id()? {
            return Ok(InboundTarget::Allowed(target));
        }
        let participant = target.conversation_id == *conversation
            && match target.conversation_type {
                ConversationType::ServerDm => *conversation == ChannelId::from(*actor),
                ConversationType::Channel => self
                    .channel_repo
                    .get_membership(conversation, actor)?
                    .is_some(),
                _ => false,
            };
        if !participant {
            tracing::debug!(
                %actor, %conversation, target_conversation = %target.conversation_id,
                "Inbound reaction/pin: target outside the arrival conversation, or sender not in it — dropped"
            );
            return Ok(InboundTarget::Refused);
        }
        Ok(InboundTarget::Allowed(target))
    }

    /// Where a reaction or pin from `actor` arriving in `conversation` may
    /// land once its target arrives: that conversation — or, from our own
    /// device, anywhere (as [`Self::lookup_inbound_target`] lets it).
    fn held_modifier_scope(
        &self,
        conversation: &ChannelId,
        actor: &UserId,
    ) -> Result<Option<ChannelId>, MessageError> {
        Ok((*actor != self.current_user_id()?).then_some(*conversation))
    }

    /// Authenticate a DM destination claim against the actual envelope sender.
    /// Legacy self-device envelopes have no destination and are rejected.
    pub fn dm_reaction_admission(
        &self,
        transport_peer: Option<UserId>,
        reactor: UserId,
        target: &str,
    ) -> Result<(crate::domain::reaction::ReactionScope, UserId), MessageError> {
        let own = self.current_user_id()?;
        let (claimed, author) = crate::domain::reaction::decode_dm_reaction_target(target)?;
        let conversation_peer = if reactor == own {
            claimed.or(transport_peer).ok_or_else(|| {
                MessageError::InvalidContent("Self-device reaction lacks destination scope".into())
            })?
        } else {
            if claimed.is_some_and(|destination| destination != own)
                || transport_peer.is_some_and(|peer| peer != reactor)
            {
                return Err(MessageError::InvalidContent(
                    "Reaction destination differs from authenticated transport".into(),
                ));
            }
            reactor
        };
        if conversation_peer == own {
            return Err(MessageError::InvalidContent(
                "Reaction destination is not a peer conversation".into(),
            ));
        }
        Ok((
            crate::domain::reaction::ReactionScope::new(
                ChannelId::from(conversation_peer),
                ConversationType::ServerDm,
            ),
            author,
        ))
    }

    /// Persist a peer's reaction that arrived in `conversation`, after binding
    /// its target to that conversation ([`Self::resolve_inbound_target`]).
    /// One whose target is not here yet is held until it arrives (Signal
    /// `Reactions.preload.ts` keeps it likewise), ordered against other held
    /// actions on the same emoji by `reaction_ts`. `Ok(false)` when held,
    /// dropped or already recorded.
    #[allow(clippy::too_many_arguments)]
    pub fn record_inbound_reaction(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        reactor: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
        emoji: &str,
        remove: bool,
        reaction_ts: u64,
    ) -> Result<bool, MessageError> {
        let allowed = *reactor == self.current_user_id()?
            || match scope.kind {
                ConversationType::ServerDm => scope.id == ChannelId::from(*reactor),
                ConversationType::Channel | ConversationType::AnonGroup => self
                    .channel_repo
                    .get_membership(&scope.id, reactor)?
                    .is_some(),
                _ => false,
            };
        if !allowed {
            return Ok(false);
        }
        let Some(_target) =
            self.message_repo
                .get_by_reaction_scope(scope, target_author, target_sent_at_ns)?
        else {
            crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
                self.database.clone(),
            )
            .queue_reaction(
                target_author,
                target_sent_at_ns,
                reactor,
                emoji,
                remove,
                scope,
                reaction_ts,
                crate::util::time::TimestampNanos::now().as_u64(),
            )?;
            return Ok(false);
        };
        self.record_reaction_at(
            scope,
            target_author,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            reaction_ts,
        )
    }

    /// Resolve a channel transport target only within its actual channel ID.
    /// Encrypted server-channel rows use AnonGroup storage. Never search another ID.
    pub fn channel_reaction_scope(
        &self,
        channel: ChannelId,
        _author: &UserId,
        _sent_at: u64,
    ) -> Result<crate::domain::reaction::ReactionScope, MessageError> {
        let mut found = None;
        for kind in [ConversationType::Channel, ConversationType::AnonGroup] {
            let candidate = crate::domain::reaction::ReactionScope::new(channel, kind);
            if self
                .message_repo
                .get_by_reaction_scope(&candidate, _author, _sent_at)?
                .is_some()
            {
                if found.is_some() {
                    return Err(MessageError::InvalidContent(
                        "Ambiguous channel reaction storage kind".into(),
                    ));
                }
                found = Some(candidate);
            }
        }
        if let Some(scope) = found {
            return Ok(scope);
        }
        let channel_row = self.channel_repo.get(&channel)?.ok_or_else(|| {
            MessageError::InvalidContent("Reaction channel metadata unavailable".into())
        })?;
        Ok(crate::domain::reaction::ReactionScope::new(
            channel,
            if channel_row.config.require_encryption {
                ConversationType::AnonGroup
            } else {
                ConversationType::Channel
            },
        ))
    }

    /// The target of a pin (or `unpin`) that arrived in `conversation`, for
    /// the caller to announce ([`Self::resolve_inbound_target`]). One whose
    /// target is not here yet is held and applied to it on arrival, the latest
    /// `pin_ts` winning; `None` then, as when dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_inbound_pin(
        &self,
        conversation: &ChannelId,
        pinner: &UserId,
        target_author: &UserId,
        target_sent_at_ns: u64,
        unpin: bool,
        pin_ts: u64,
    ) -> Result<Option<Message>, MessageError> {
        match self.lookup_inbound_target(conversation, pinner, target_author, target_sent_at_ns)? {
            InboundTarget::Allowed(target) => Ok(Some(target)),
            InboundTarget::Refused => Ok(None),
            InboundTarget::NotHeld => {
                tracing::debug!(%pinner, %target_author, target_sent_at_ns, "Pin before its message — held");
                crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(self.database.clone())
                    .queue_pin(
                        target_author, target_sent_at_ns, pinner, unpin,
                        self.held_modifier_scope(conversation, pinner)?, pin_ts,
                        crate::util::time::TimestampNanos::now().as_u64(),
                    )?;
                Ok(None)
            }
        }
    }

    /// Validate server reaction projection identity without replacing the
    /// independent sealed peer-control ledger or resolving historical ambiguity.
    pub fn reconcile_channel_reactions(
        &self,
        message: &crate::proto::chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<(), MessageError> {
        let id = message
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid message id".into()))?;
        let local = self.get_message_local(&id)?;
        let author = if message.author_id.is_empty() {
            None
        } else {
            Some(message.author_id.parse::<UserId>().map_err(|_| {
                MessageError::InvalidContent("invalid reaction snapshot author".into())
            })?)
        };
        // An uncached own encrypted row has no authenticated message clock.
        // Preserve its backup limitation while validating all supplied identities.
        let encrypted = matches!(
            message
                .content
                .as_ref()
                .and_then(|content| content.r#type.as_ref()),
            Some(crate::proto::chat_v1::message::content::Type::Encrypted(_))
        );
        let target_timestamp = local
            .as_ref()
            .map(|row| row.sent_at)
            .or_else(|| (!encrypted).then_some(message.timestamp));
        // Validate the entire snapshot before any early return or ledger mutation.
        let _validated: Vec<(UserId, String)> = message
            .reactions
            .iter()
            .map(|reaction| {
                let reactor = reaction
                    .reactor_id
                    .parse::<UserId>()
                    .map_err(|_| MessageError::InvalidContent("invalid reactor id".into()))?;
                let target = reaction.target_author_aci.parse::<UserId>().map_err(|_| {
                    MessageError::InvalidContent("invalid reaction target author".into())
                })?;
                if Some(target) != author
                    || target_timestamp
                        .is_some_and(|timestamp| reaction.target_sent_timestamp != timestamp)
                {
                    return Err(MessageError::InvalidContent(
                        "reaction target mismatch".into(),
                    ));
                }
                Ok((reactor, reaction.emoji.clone()))
            })
            .collect::<Result<_, MessageError>>()?;
        let Some(local) = local else {
            return Ok(());
        };
        if local.conversation_id != channel_id
            || !crate::domain::reaction::channel_snapshot_kind_matches(
                &local.conversation_type,
                encrypted,
            )
        {
            return Err(MessageError::InvalidContent(
                "Reaction snapshot conversation kind/id mismatch".into(),
            ));
        }
        if local.is_deleted {
            return Ok(());
        }
        if local.sender_id != author {
            return Err(MessageError::InvalidContent(
                "reaction snapshot author mismatch".into(),
            ));
        }
        // Client channel reactions travel as sealed peer controls. A server
        // Message.reactions projection is not a complete snapshot of that ledger,
        // even when nonempty, and cannot clear legacy ambiguity or tombstones.
        // Retain verified local votes/projections; peer controls remain authority.
        Ok(())
    }

    /// Classify a raw `(channel_id, conversation_type, peer_id)` triple into
    /// the typed `ActiveConversation` variant it represents.
    ///
    /// - AnonDm/AnonGroup `ConversationType`s map straight to their variants.
    /// - ServerDm → channel-free `ServerDm { peer_id }` (the peer is the
    ///   passed `peer_id`, or the `channel_id`'s UUID which equals it).
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
            ConversationType::AnonDm => ActiveConversation::AnonDm(
                crate::domain::anon_dm::AnonDmId::from_uuid(*channel_id.as_uuid()),
            ),
            ConversationType::AnonGroup => ActiveConversation::AnonGroup(
                crate::domain::anon_group::AnonGroupId::from_uuid(*channel_id.as_uuid()),
            ),
            ConversationType::ServerDm => {
                // Channel-free DM: keyed by peer. conversation_id == peer
                // user_id, so recover the peer from either source.
                let peer = peer_id.unwrap_or_else(|| {
                    crate::domain::ids::UserId::from_uuid(*channel_id.as_uuid())
                });
                ActiveConversation::ServerDm { peer_id: peer }
            }
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
        scope: &crate::domain::reaction::ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .user_has_reacted(scope, sender_id, sent_at_ns, emoji)
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

        let proto_channel = channel_client.get_channel(&channel_id.to_string()).await?;

        // Server channels are never DMs anymore (DMs are channel-free,
        // Signal-style — the DIRECT_MESSAGE channel type no longer exists
        // on the wire). A server-fetched channel row is always a real
        // group/public/saved-messages channel.
        let is_dm = false;
        let require_encryption = proto_channel.config.map(|c| c.encrypted).unwrap_or(false);

        Ok((is_dm, require_encryption))
    }
}

// Re-export helper functions used by other parts of the codebase
pub use self::core::{
    proto_content_to_domain, proto_to_domain_message, proto_to_domain_message_with_sender,
};

#[cfg(test)]
mod solicited_resends_tests {
    use super::*;

    #[test]
    fn solicited_resends_survive_a_restart_and_expire_after_24h() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("s.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        let settings = Arc::new(SettingsRepository::new(db));
        let now_ms = crate::util::time::TimestampNanos::now().as_u64() / 1_000_000;
        let fresh = (ChannelId::new(), UserId::new(), now_ms);
        let stale = (
            ChannelId::new(),
            UserId::new(),
            now_ms - SOLICITED_RESEND_TTL_MS - 1,
        );

        let before = SolicitedResends::load(settings.clone());
        before.insert(fresh);
        before.insert(stale);

        let after = SolicitedResends::load(settings.clone());
        assert!(
            after.contains(&fresh),
            "a request made before the restart is still honoured"
        );
        assert!(
            !after.contains(&stale),
            "past the resend window it is dropped"
        );
        assert!(after.take(&fresh));
        assert!(
            SolicitedResends::load(settings).is_empty(),
            "consumed entries stay consumed"
        );
    }
}

/// What an inbound reaction or pin names ([`MessageService::lookup_inbound_target`]).
enum InboundTarget {
    /// Held here, and the sender may modify it.
    Allowed(Message),
    /// Not here (yet).
    NotHeld,
    /// Here, but not the sender's to modify from where it arrived.
    Refused,
}

#[cfg(test)]
mod inbound_target_tests {
    use super::*;
    use crate::domain::channel::{Channel, MemberRole, Membership, NotificationPreference};
    use crate::storage::repositories::settings_repo::keys;

    struct Fixture {
        client: Arc<crate::client::ChatterClient>,
        _dir: tempfile::TempDir,
        me: UserId,
        /// A friend we DM, who is NOT in `channel`.
        bob: UserId,
        /// A friend we DM, who IS in `channel`.
        carol: UserId,
        channel: ChannelId,
    }

    /// Carol's message in our DM with her, and her message in `channel`.
    const DM_TS: u64 = 100;
    const CH_TS: u64 = 200;

    fn fixture() -> Fixture {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (me, bob, carol, channel) = (
            UserId::new(),
            UserId::new(),
            UserId::new(),
            ChannelId::new(),
        );
        let svc = client.messages();
        svc.settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        svc.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        svc.channel_repo
            .insert(&Channel::new(channel, "g".into(), ChannelType::Private, me))
            .unwrap();
        for user_id in [me, carol] {
            svc.channel_repo
                .insert_membership(&Membership {
                    channel_id: channel,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
        }
        svc.message_repo
            .insert(&Message::new_text(
                ChannelId::from(carol),
                ConversationType::ServerDm,
                Some(carol),
                "dm".into(),
                DM_TS,
            ))
            .unwrap();
        svc.message_repo
            .insert(&Message::new_text(
                channel,
                ConversationType::Channel,
                Some(carol),
                "ch".into(),
                CH_TS,
            ))
            .unwrap();
        Fixture {
            client,
            _dir: dir,
            me,
            bob,
            carol,
            channel,
        }
    }

    fn reactors(f: &Fixture, ts: u64) -> Vec<UserId> {
        f.client
            .messages()
            .message_repo
            .get_reactions(
                &crate::domain::reaction::ReactionScope::new(
                    if ts == DM_TS {
                        ChannelId::from(f.carol)
                    } else {
                        f.channel
                    },
                    if ts == DM_TS {
                        ConversationType::ServerDm
                    } else {
                        ConversationType::Channel
                    },
                ),
                &f.carol,
                ts,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.user_id)
            .collect()
    }

    /// Signal binds a reaction to the conversation it arrived in: a peer who
    /// knows (author, sent_at) of a message elsewhere cannot touch it.
    #[tokio::test]
    async fn reactions_bind_to_the_arrival_conversation() {
        let f = fixture();
        let svc = f.client.messages();

        // Bob, from his own DM, targets Carol's DM message and her channel message.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        // Bob claims the channel he is not a member of.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.bob,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        // Carol, a channel member, aims a channel reaction at her DM message.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(reactors(&f, DM_TS).is_empty() && reactors(&f, CH_TS).is_empty());

        // The legitimate ones land.
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm
                ),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.carol,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert_eq!(reactors(&f, DM_TS), vec![f.carol]);
        assert_eq!(reactors(&f, CH_TS), vec![f.carol]);

        // Removal is bound the same way.
        svc.record_reaction(
            &crate::domain::reaction::ReactionScope::new(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
            ),
            &f.carol,
            DM_TS,
            &f.bob,
            "y",
            false,
        )
        .unwrap();
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                DM_TS,
                "y",
                true,
                1
            )
            .unwrap()
        );
        assert!(reactors(&f, DM_TS).contains(&f.bob));
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm
                ),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                true,
                2
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn scoped_inbound_and_linked_device_reactions_never_cross_same_author_clock() {
        let f = fixture();
        let svc = f.client.messages();
        let dm_scope = crate::domain::reaction::ReactionScope::new(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
        );
        let encrypted_scope =
            crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::AnonGroup);
        svc.message_repo
            .insert(&Message::new_text(
                f.channel,
                ConversationType::AnonGroup,
                Some(f.carol),
                "same author and clock".into(),
                DM_TS,
            ))
            .unwrap();
        let encoded = crate::domain::reaction::encode_dm_reaction_target(&f.carol, &f.carol);
        let (admitted, author) = svc.dm_reaction_admission(None, f.me, &encoded).unwrap();
        assert_eq!(admitted, dm_scope);
        assert_eq!(author, f.carol);
        assert!(
            svc.dm_reaction_admission(None, f.me, &f.carol.to_string())
                .is_err(),
            "legacy own-device ambiguity is never guessed"
        );
        assert!(
            svc.dm_reaction_admission(Some(f.bob), f.bob, &encoded)
                .is_err(),
            "remote actor cannot claim a destination other than this recipient"
        );
        let remote = crate::domain::reaction::encode_dm_reaction_target(&f.me, &f.carol);
        assert_eq!(
            svc.dm_reaction_admission(Some(f.carol), f.carol, &remote)
                .unwrap()
                .0,
            dm_scope
        );
        assert!(
            svc.record_inbound_reaction(&admitted, &f.me, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(&admitted, &f.me, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_reactions(&encrypted_scope, &author, DM_TS)
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.record_inbound_reaction(&encrypted_scope, &f.carol, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            svc.record_inbound_reaction(&dm_scope, &f.me, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(&dm_scope, &f.me, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_by_reaction_scope(&dm_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty()
        );
        assert_eq!(
            svc.message_repo
                .get_by_reaction_scope(&encrypted_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts[0]
                .count,
            1
        );
        assert!(
            svc.record_inbound_reaction(&encrypted_scope, &f.carol, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_by_reaction_scope(&encrypted_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty()
        );
    }

    /// Pins resolve their target through the same binding.
    #[tokio::test]
    async fn pin_targets_bind_to_the_arrival_conversation() {
        let f = fixture();
        let svc = f.client.messages();
        let resolve = |conv: &ChannelId, actor: &UserId, ts| {
            svc.resolve_inbound_target(conv, actor, &f.carol, ts)
                .unwrap()
        };

        assert!(resolve(&ChannelId::from(f.bob), &f.bob, DM_TS).is_none());
        assert!(resolve(&f.channel, &f.bob, CH_TS).is_none());
        assert!(resolve(&f.channel, &f.carol, DM_TS).is_none());
        assert!(resolve(&ChannelId::from(f.carol), &f.carol, CH_TS).is_none());
        assert!(
            resolve(&ChannelId::new(), &f.carol, 999).is_none(),
            "unknown target"
        );

        assert_eq!(
            resolve(&ChannelId::from(f.carol), &f.carol, DM_TS)
                .unwrap()
                .conversation_id,
            ChannelId::from(f.carol)
        );
        assert_eq!(
            resolve(&f.channel, &f.carol, CH_TS)
                .unwrap()
                .conversation_id,
            f.channel
        );
        // Our own devices act on any conversation we hold (Signal: isMe).
        assert!(resolve(&ChannelId::from(f.me), &f.me, DM_TS).is_some());
    }
}
