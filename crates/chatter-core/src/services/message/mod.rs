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
mod channel_send;
pub(crate) use channel_send::{spawn_channel_send_recovery, owns as channel_send_owns};
pub(crate) mod dm_outbox;
pub(crate) use dm_outbox::spawn_dm_send_recovery;
pub mod conversion;
pub mod core;
mod saved;
pub use saved::SavedSyncOutcome;
pub(crate) use saved::persist_saved_device_snapshot;
pub(crate) use saved::spawn_saved_sync_recovery;
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

/// Lets the durable send journals (`channel_send`, `dm_outbox`) use `?` on
/// raw SQLite calls; the error is the same `Storage` variant as a repository's.
impl From<rusqlite::Error> for MessageError {
    fn from(err: rusqlite::Error) -> Self {
        MessageError::Storage(err.into())
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
    /// Session-local backoff for exact failed channel envelopes; never an acknowledgement.
    pub(crate) channel_decrypt_failures: Arc<parking_lot::Mutex<channel_inbound::DecryptFailureGate>>,
    /// Group messages we already sent a `DecryptionErrorMessage` for, keyed
    /// `(message id, sender device)`: one retry request per message per device.
    pub(crate) group_retry_requests:
        Arc<parking_lot::Mutex<std::collections::HashSet<(String, u32)>>>,
    /// Group messages this device asked a sender to resend — shared with
    /// `MessageGateway`, which honours a resend's carried server id only
    /// against an entry here, and consumes it.
    pub(crate) solicited_resends: SolicitedResends,
    /// Serialize immediate and lifecycle Saved attempts across service clones.
    pub(crate) saved_sync_gate: Arc<tokio::sync::Mutex<()>>,
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
            channel_decrypt_failures: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            group_retry_requests: Arc::new(parking_lot::Mutex::new(
                std::collections::HashSet::new(),
            )),
            solicited_resends,
            saved_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
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

    /// Explicit call identity never falls back to timestamp semantics on refusal.
    pub(crate) async fn apply_scoped_call_or_timestamp_delete_received(
        &self,
        sender: &UserId,
        target_author: Option<&str>,
        destination: Option<&str>,
        target_call_id: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(claim) = target_call_id else {
            return self
                .apply_scoped_dm_delete_received(sender, target_author, destination, timestamp)
                .await;
        };
        let me = self.current_user_id()?;
        if sender.as_uuid().is_nil() {
            return Ok(None);
        }
        let peer = if *sender == me {
            let (Some(destination), Some(author)) = (destination, target_author) else {
                return Ok(None);
            };
            let (Ok(peer), Ok(author)) = (destination.parse::<UserId>(), author.parse::<UserId>())
            else {
                return Ok(None);
            };
            if peer == me || peer.as_uuid().is_nil() || (author != me && author != peer) {
                return Ok(None);
            }
            peer
        } else {
            if destination.is_some() {
                return Ok(None);
            }
            if target_author.is_some_and(|value| {
                value
                    .parse::<UserId>()
                    .ok()
                    .is_none_or(|a| a != me && a != *sender)
            }) {
                return Ok(None);
            }
            *sender
        };
        Ok(
            crate::storage::repositories::message_modifier_repo::apply_call_delete(
                self.database.clone(),
                &self.message_repo,
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(peer),
                    ConversationType::ServerDm,
                ),
                claim,
            )?,
        )
    }

    /// Route using the authenticated sender, accepting a destination only on
    /// self-device copies. No missing self-copy destination, no author guessing.
    pub async fn apply_scoped_dm_delete_received(
        &self,
        sender: &UserId,
        target_author: Option<&str>,
        destination: Option<&str>,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        let me = self.current_user_id()?;
        let peer = if *sender == me {
            let (Some(destination), Some(author)) = (destination, target_author) else {
                return Ok(None);
            };
            let Ok(peer) = destination.parse::<UserId>() else {
                return Ok(None);
            };
            if peer == me
                || author
                    .parse::<UserId>()
                    .ok()
                    .is_none_or(|author| author != me && author != peer)
            {
                return Ok(None);
            }
            peer
        } else {
            if destination.is_some() {
                return Ok(None);
            }
            *sender
        };
        self.apply_targeted_dm_delete_received(&peer, target_author, target_sent_at)
            .await
    }

    pub async fn apply_targeted_dm_delete_received(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.apply_targeted_dm_delete(peer, target_author, target_sent_at)
    }

    fn admitted_dm_delete_author(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
    ) -> Result<Option<UserId>, MessageError> {
        let me = self.current_user_id()?;
        // Self-device deletes need a destination claim; this route has none.
        if *peer == me {
            return Ok(None);
        }
        let author = match target_author {
            Some(value) => match value.parse::<UserId>() {
                Ok(author) => author,
                Err(_) => return Ok(None),
            },
            None => return Ok(None),
        };
        Ok((author == *peer || author == me).then_some(author))
    }

    fn targeted_dm_delete_target(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(author) = self.admitted_dm_delete_author(peer, target_author)? else {
            return Ok(None);
        };
        Ok(self.message_repo.get_by_reaction_scope(
            &crate::domain::reaction::ReactionScope::new(
                ChannelId::from(*peer),
                ConversationType::ServerDm,
            ),
            &author,
            timestamp,
        )?)
    }

    /// Exact author and exact arrival conversation/type; never guess a second author.
    pub fn apply_targeted_dm_delete(
        &self,
        peer: &UserId,
        target_author: Option<&str>,
        timestamp: u64,
    ) -> Result<Option<Message>, MessageError> {
        let Some(author) = self.admitted_dm_delete_author(peer, target_author)? else {
            return Ok(None);
        };
        let target = self.targeted_dm_delete_target(peer, target_author, timestamp)?;
        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
            self.database.clone(),
        )
        .queue_dm_delete(
            &author,
            timestamp,
            Some(ChannelId::from(*peer)),
            crate::util::time::TimestampNanos::now().as_u64(),
        )?;
        if let Some(message) = &target {
            self.message_repo.delete(&message.id)?;
        }
        Ok(target)
    }

    /// Apply an authenticated channel author's delete in its exact channel scope.
    /// Retain a typed tombstone even for a held target, so history replay cannot
    /// resurrect it and collisions in other conversations or types survive.
    pub fn apply_channel_delete(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        target_sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
            self.database.clone(),
        )
        .queue_channel_delete(
            sender,
            target_sent_at,
            *channel_id,
            crate::util::time::TimestampNanos::now().as_u64(),
        )?;
        let target = self.message_repo.get_by_reaction_scope(
            &crate::domain::reaction::ReactionScope::new(*channel_id, ConversationType::Channel),
            sender,
            target_sent_at,
        )?;
        if let Some(message) = &target {
            // Storage must reflect deletion before the caller announces it.
            self.message_repo.delete(&message.id)?;
        }
        Ok(target)
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
        let action_ts = self.message_repo.allocate_reaction_clock(reactor)?;
        self.record_reaction_at(
            scope,
            target_sender,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            action_ts,
        )
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

    /// Outgoing completion requires action evidence, not a visible INSERT/DELETE.
    pub(crate) fn record_reaction_completion_at(
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
        Self::record_reaction_completion_with_repo(
            &self.message_repo,
            &current,
            scope,
            target_sender,
            target_sent_at_ns,
            reactor,
            emoji,
            remove,
            action_ts,
        )
    }

    // Shared with credential-free completion regression fixtures.
    pub(crate) fn record_reaction_completion_with_repo(
        repo: &MessageRepository,
        current: &UserId,
        scope: &crate::domain::reaction::ReactionScope,
        target_sender: &UserId,
        target_sent_at_ns: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, MessageError> {
        let outcome = if remove {
            repo.remove_reaction_outcome_at(
                scope,
                target_sender,
                target_sent_at_ns,
                reactor,
                current,
                emoji,
                action_ts,
            )
        } else {
            repo.add_reaction_outcome_at(
                scope,
                target_sender,
                target_sent_at_ns,
                reactor,
                current,
                emoji,
                action_ts,
            )
        }?;
        Ok(outcome.completed)
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
    /// Self-device envelopes without a destination are rejected.
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
                ConversationType::Channel => self
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
            || local.conversation_type != ConversationType::Channel
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
        // even when nonempty, and cannot clear ambiguity or tombstones.
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

    /// Whether a channel requires end-to-end encryption.
    ///
    /// Looks up channel locally first, fetches from server if not found.
    pub(crate) async fn channel_requires_encryption(
        &self,
        channel_id: &ChannelId,
        channel_client: &mut ChannelClient,
    ) -> Result<bool, MessageError> {
        // Try local lookup first
        if let Ok(Some(channel)) = self.channel_repo.get(channel_id) {
            return Ok(channel.config.require_encryption);
        }

        // Not found locally - fetch from server
        tracing::debug!(
            channel_id = %channel_id,
            "Channel not in local repo, fetching from server"
        );

        let proto_channel = channel_client.get_channel(&channel_id.to_string()).await?;
        Ok(proto_channel.config.map(|c| c.encrypted).unwrap_or(false))
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
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
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

    #[tokio::test]
    async fn dm_delete_connected_processor_emits_actual_author_and_rejects_other_scope() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        for (author, ts) in [(f.me, DM_TS), (f.carol, DM_TS)] {
            let target = if author == f.me {
                let msg = Message::new_text(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm,
                    Some(author),
                    "own".into(),
                    ts,
                );
                svc.message_repo.insert(&msg).unwrap();
                msg
            } else {
                svc.get_by_sender_timestamp(&author, ts).unwrap().unwrap()
            };
            let (tx, mut rx) = broadcast::channel(8);
            let stats = Arc::new(RwLock::new(InboxStats::default()));
            let mut domain = f.client.event_bus().subscribe_domain();
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let bytes = prost::Message::encode_to_vec(&wire);
            let decoded =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                matches!(crate::services::gateway::MessageGateway::parse_content(decoded.clone()).unwrap(), crate::services::gateway::DmContent::DeleteForEveryone { target_author_aci: Some(value), .. } if value == author.to_string())
            );
            let content = decoded.content.unwrap();
            dispatch_control_content(content, &f.carol, 1, "synthetic", 2, &tx, &stats);
            processor.process(&rx.try_recv().unwrap()).await;
            match domain.try_recv().unwrap() {
                crate::inbox::handlers::DomainEvent::MessageDeleted {
                    message_id,
                    author_id,
                    channel_id,
                    target_sent_timestamp,
                } => {
                    assert_eq!(message_id, target.id);
                    assert_eq!(author_id, author);
                    assert_eq!(channel_id, Some(ChannelId::from(f.carol)));
                    assert_eq!(target_sent_timestamp, ts);
                }
                event => panic!("unexpected event: {event:?}"),
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            svc.message_repo.insert(&target).unwrap();
            assert!(
                svc.message_repo.get(&target.id).unwrap().is_none(),
                "history replay cannot resurrect live deletion"
            );
        }
        let unrelated_own = Message::new_text(
            ChannelId::from(f.bob),
            ConversationType::ServerDm,
            Some(f.me),
            "unrelated own".into(),
            303,
        );
        svc.message_repo.insert(&unrelated_own).unwrap();
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 303)
                .unwrap()
                .is_none()
        );
        assert!(svc.message_repo.get(&unrelated_own.id).unwrap().is_some());
        let late = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.me),
            "actual late DM".into(),
            303,
        );
        svc.message_repo.insert(&late).unwrap();
        assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        // Bob cannot use Carol's timestamp to delete her DM or channel row.
        assert!(svc.apply_targeted_dm_delete(&f.bob, Some(&f.bob.to_string()), CH_TS).unwrap().is_none());
        assert!(
            svc.get_by_sender_timestamp(&f.carol, CH_TS)
                .unwrap()
                .is_some()
        );
        let own_channel = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::Channel,
            Some(f.me),
            "collision".into(),
            302,
        );
        svc.message_repo.insert(&own_channel).unwrap();
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 302)
                .unwrap()
                .is_none()
        );
        assert!(svc.apply_targeted_dm_delete(&f.me, Some(&f.me.to_string()), 302).unwrap().is_none());
        assert!(svc.message_repo.get(&own_channel.id).unwrap().is_some());
        let late = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.me),
            "actual late DM".into(),
            302,
        );
        svc.message_repo.insert(&late).unwrap();
        assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        let handle = f
            .client
            .conversation(crate::state::ActiveConversation::ServerDm { peer_id: f.bob });
        assert!(handle.message_menu(&f.carol, CH_TS).is_err());
        assert!(handle.is_own_message(&f.carol, CH_TS).is_err());
        let channel_handle = f
            .client
            .conversation(crate::state::ActiveConversation::Channel(ChannelId::new()));
        assert!(channel_handle.message_menu(&f.carol, CH_TS).is_err());
    }

    #[tokio::test]
    async fn self_device_dm_delete_carries_destination_through_production_processor() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        for (ts, author) in [(910, f.me), (911, f.carol)] {
            let target = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(author),
                "self target".into(),
                ts,
            );
            let other_author = if author == f.me { f.carol } else { f.me };
            let survivor = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(other_author),
                "same timestamp".into(),
                ts,
            );
            let other_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(author),
                "other chat".into(),
                ts,
            );
            for msg in [&target, &survivor, &other_chat] {
                svc.message_repo.insert(msg).unwrap();
            }
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let copy = super::dm::own_device_copy(&f.carol, &wire, 1).unwrap();
            let bytes = prost::Message::encode_to_vec(&copy);
            let decoded =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                matches!(crate::services::gateway::MessageGateway::parse_content(decoded.clone()).unwrap(),
                crate::services::gateway::DmContent::DeleteForEveryone {
                    target_author_aci: Some(a), destination_user_id: Some(d), ..
                } if a == author.to_string() && d == f.carol.to_string())
            );
            let mut domain = f.client.event_bus().subscribe_domain();
            dispatch_control_content(decoded.content.unwrap(), &f.me, 2, "self", 3, &tx, &stats);
            processor.process(&rx.try_recv().unwrap()).await;
            assert!(
                matches!(domain.try_recv().unwrap(), crate::inbox::handlers::DomainEvent::MessageDeleted {
                message_id, author_id, channel_id: Some(conversation), ..
            } if message_id == target.id && author_id == author && conversation == ChannelId::from(f.carol))
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            for msg in [&survivor, &other_chat] {
                assert!(svc.message_repo.get(&msg.id).unwrap().is_some());
            }
            svc.message_repo.insert(&target).unwrap();
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
        }
        // Missing, malformed, self destinations, third-party authors, and peer
        // destination claims cannot establish a self-device conversation.
        for (index, sender, author, destination) in [
            (0, f.me, Some(f.me.to_string()), None),
            (1, f.me, None, Some(f.carol.to_string())),
            (2, f.me, Some(f.me.to_string()), Some(String::new())),
            (3, f.me, Some(f.me.to_string()), Some("invalid".into())),
            (4, f.me, Some(f.me.to_string()), Some(f.me.to_string())),
            (5, f.me, Some(f.bob.to_string()), Some(f.carol.to_string())),
            (6, f.carol, Some(f.me.to_string()), Some(f.bob.to_string())),
            (
                7,
                f.carol,
                Some(f.me.to_string()),
                Some(f.carol.to_string()),
            ),
            (8, f.carol, Some(f.me.to_string()), Some(String::new())),
            (9, f.me, None, None),
            (10, f.me, Some(String::new()), Some(f.carol.to_string())),
            (11, f.me, Some("invalid".into()), Some(f.carol.to_string())),
        ] {
            let ts = 920 + index;
            let held = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(f.me),
                "held protected target".into(),
                ts,
            );
            let cross_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(f.me),
                "held cross-chat target".into(),
                ts,
            );
            svc.message_repo.insert(&held).unwrap();
            svc.message_repo.insert(&cross_chat).unwrap();
            let mut content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                author.as_deref(),
                None,
            );
            if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
                &mut content.content
            {
                dm.delete.as_mut().unwrap().destination_user_id = destination;
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            dispatch_control_content(
                content.content.unwrap(),
                &sender,
                2,
                "forged",
                3,
                &tx,
                &stats,
            );
            processor.process(&rx.try_recv().unwrap()).await;
            assert!(domain.try_recv().is_err());
            assert!(svc.message_repo.get(&held.id).unwrap().is_some());
            assert!(svc.message_repo.get(&cross_chat.id).unwrap().is_some());
            svc.message_repo.delete(&held.id).unwrap();
            svc.message_repo.delete(&cross_chat.id).unwrap();
            for peer in [f.carol, f.bob] {
                for author in [f.me, f.carol, f.bob] {
                    let late = Message::new_text(
                        ChannelId::from(peer),
                        ConversationType::ServerDm,
                        Some(author),
                        "refused control must not queue".into(),
                        ts,
                    );
                    svc.message_repo.insert(&late).unwrap();
                    assert!(svc.message_repo.get(&late.id).unwrap().is_some());
                }
            }
        }
    }

    #[tokio::test]
    async fn queued_dm_deletes_block_either_author_catchup_without_crossing_type_or_chat() {
        let f = fixture();
        let svc = f.client.messages();
        for author in [f.me, f.carol] {
            let ts = if author == f.me { 701 } else { 702 };
            assert!(
                svc.apply_targeted_dm_delete(&f.carol, Some(&author.to_string()), ts)
                    .unwrap()
                    .is_none()
            );
            let other_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(author),
                "other".into(),
                ts,
            );
            svc.message_repo.insert(&other_chat).unwrap();
            assert!(svc.message_repo.get(&other_chat.id).unwrap().is_some());
            svc.message_repo.delete(&other_chat.id).unwrap();
            let channel = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::Channel,
                Some(author),
                "channel collision".into(),
                ts,
            );
            svc.message_repo.insert(&channel).unwrap();
            assert!(svc.message_repo.get(&channel.id).unwrap().is_some());
            svc.message_repo.delete(&channel.id).unwrap();
            let survivor = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(if author == f.me { f.carol } else { f.me }),
                "collision survivor".into(),
                ts,
            );
            svc.message_repo.insert(&survivor).unwrap();
            assert!(svc.message_repo.get(&survivor.id).unwrap().is_some());
            let dm = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(author),
                "catchup".into(),
                ts,
            );
            for _ in 0..2 {
                svc.message_repo.insert(&dm).unwrap();
                assert!(
                    svc.message_repo.get(&dm.id).unwrap().is_none(),
                    "redelivery must not resurrect deleted DM"
                );
            }
        }
    }

    #[tokio::test]
    async fn channel_delete_dispatch_is_exact_and_retains_typed_late_arrival_tombstone() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let other_channel = ChannelId::new();
        // Insert other types first to expose the old global LIMIT 1 lookup.
        for (ts, held) in [(930, true), (931, false)] {
            let mut survivors = Vec::new();
            for (conversation, kind, author) in [
                (f.channel, ConversationType::ServerDm, f.carol),
                (f.channel, ConversationType::AnonDm, f.carol),
                (f.channel, ConversationType::AnonGroup, f.carol),
                (other_channel, ConversationType::Channel, f.carol),
                (f.channel, ConversationType::Channel, f.me),
            ] {
                let row =
                    Message::new_text(conversation, kind, Some(author), "survivor".into(), ts);
                svc.message_repo.insert(&row).unwrap();
                survivors.push(row);
            }
            let target = Message::new_text(
                f.channel,
                ConversationType::Channel,
                Some(f.carol),
                "target".into(),
                ts,
            );
            if held {
                svc.message_repo.insert(&target).unwrap();
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            let author = f.carol.to_string();
            let channel = f.channel.to_string();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(author.as_str()),
                Some(&channel),
            );
            dispatch_control_content(
                content.content.unwrap(),
                &f.carol,
                1,
                "channel delete",
                2,
                &tx,
                &stats,
            );
            processor.process(&rx.try_recv().unwrap()).await;
            if held {
                match domain.try_recv().unwrap() {
                    crate::inbox::handlers::DomainEvent::MessageDeleted {
                        message_id,
                        author_id,
                        channel_id,
                        target_sent_timestamp,
                    } => {
                        assert_eq!(message_id, target.id);
                        assert_eq!(author_id, f.carol);
                        assert_eq!(channel_id, Some(f.channel));
                        assert_eq!(target_sent_timestamp, ts);
                    }
                    event => panic!("unexpected event: {event:?}"),
                }
            } else {
                assert!(domain.try_recv().is_err());
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            for _ in 0..2 {
                svc.message_repo.insert(&target).unwrap();
                assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            }
            for row in survivors {
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
                svc.message_repo.delete(&row.id).unwrap();
                svc.message_repo.insert(&row).unwrap();
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
            }
        }
    }

    #[tokio::test]
    async fn channel_delete_dispatch_rejects_forged_author_and_destination_without_tombstone() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        for (index, author, destination) in [
            (0, Some(f.me.to_string()), None),
            (1, Some(String::new()), None),
            (2, Some("invalid".into()), None),
            (3, Some(f.carol.to_string()), Some(f.me.to_string())),
            (4, None, Some(String::new())),
            (5, None, None),
        ] {
            let ts = 940 + index;
            let targets: Vec<_> = [f.me, f.carol]
                .into_iter()
                .map(|sender| {
                    Message::new_text(
                        f.channel,
                        ConversationType::Channel,
                        Some(sender),
                        "protected".into(),
                        ts,
                    )
                })
                .collect();
            for row in &targets {
                svc.message_repo.insert(row).unwrap();
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            let mut content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                author.as_deref(),
                Some(&f.channel.to_string()),
            );
            if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
                &mut content.content
            {
                dm.delete.as_mut().unwrap().destination_user_id = destination;
            }
            dispatch_control_content(
                content.content.unwrap(),
                &f.carol,
                1,
                "forged channel delete",
                2,
                &tx,
                &stats,
            );
            if let Ok(event) = rx.try_recv() {
                processor.process(&event).await;
                panic!("forged channel delete reached the processor");
            }
            assert!(domain.try_recv().is_err());
            for row in targets {
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
                svc.message_repo.delete(&row.id).unwrap();
                svc.message_repo.insert(&row).unwrap();
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
            }
        }
    }

    #[test]
    fn channel_delete_rejects_forged_target_author_before_event() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let sender = UserId::new();
        let victim = UserId::new();
        let channel = ChannelId::new().to_string();
        let (tx, mut rx) = broadcast::channel(8);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            1,
            Some(&victim.to_string()),
            Some(&channel),
        );
        dispatch_control_content(
            content.content.unwrap(),
            &sender,
            1,
            "fixture",
            1,
            &tx,
            &stats,
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exact_dm_delete_authorless_and_forged_author_admission() {
        let f = fixture();
        let svc = f.client.messages();
        let scope = ChannelId::from(f.carol);
        // A delete that names no author is refused and holds nothing back.
        assert!(svc.apply_targeted_dm_delete(&f.carol, None, 910).unwrap().is_none());
        for author in [f.me, f.carol] {
            let message = Message::new_text(
                scope,
                ConversationType::ServerDm,
                Some(author),
                "kept".into(),
                910,
            );
            svc.message_repo.insert(&message).unwrap();
            assert!(svc.message_repo.get(&message.id).unwrap().is_some());
        }
        // Explicit missing own target queues only own, preserving the peer collision.
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 911)
                .unwrap()
                .is_none()
        );
        let peer = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.carol),
            "peer".into(),
            911,
        );
        svc.message_repo.insert(&peer).unwrap();
        assert!(svc.message_repo.get(&peer.id).unwrap().is_some());
        let own = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.me),
            "own".into(),
            911,
        );
        svc.message_repo.insert(&own).unwrap();
        assert!(svc.message_repo.get(&own.id).unwrap().is_none());
        for author in [f.bob.to_string(), String::new(), "invalid".into()] {
            assert!(
                svc.apply_targeted_dm_delete(&f.carol, Some(&author), 912)
                    .unwrap()
                    .is_none()
            );
        }
        let foreign = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.bob),
            "forged".into(),
            912,
        );
        svc.message_repo.insert(&foreign).unwrap();
        assert!(svc.message_repo.get(&foreign.id).unwrap().is_some());
        let peer_message = svc
            .get_by_sender_timestamp(&f.carol, DM_TS)
            .unwrap()
            .unwrap();
        assert_eq!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.carol.to_string()), DM_TS)
                .unwrap()
                .unwrap()
                .id,
            peer_message.id
        );
    }

    #[tokio::test]
    async fn latest_preview_pages_past_typed_timer_notices_and_preserves_other_system_events() {
        use crate::domain::message::{MessageContent, SystemMessage};
        let f = fixture();
        let svc = f.client.messages();
        for ts in 1000..1040 {
            let mut notice = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(f.carol),
                String::new(),
                ts,
            );
            notice.content =
                MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 60 });
            svc.message_repo.insert(&notice).unwrap();
        }
        assert_eq!(
            f.client
                .latest_message(&ChannelId::from(f.carol), &ConversationType::ServerDm)
                .unwrap()
                .sent_at,
            DM_TS
        );
        let mut system = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.carol),
            String::new(),
            1041,
        );
        system.content = MessageContent::System(SystemMessage::ChannelCreated);
        svc.message_repo.insert(&system).unwrap();
        assert_eq!(
            f.client
                .latest_message(&ChannelId::from(f.carol), &ConversationType::ServerDm)
                .unwrap()
                .id,
            system.id
        );
        let empty_chat = ChannelId::new();
        for ts in 2000..2300 {
            let mut notice = Message::new_text(
                empty_chat,
                ConversationType::ServerDm,
                Some(f.carol),
                String::new(),
                ts,
            );
            notice.content =
                MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 0 });
            svc.message_repo.insert(&notice).unwrap();
        }
        assert!(
            f.client
                .latest_message(&empty_chat, &ConversationType::ServerDm)
                .is_none()
        );
    }

    #[tokio::test]
    async fn anonymous_dm_receiver_deletes_both_authors_and_queues_without_group_privilege() {
        use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
        let f = fixture();
        let svc = f.client.messages();
        let anon = f.client.anon_dms();
        let mut dm = AnonDm::new_initiated(f.carol);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([7; 32]);
        anon.anon_dm_repo.insert_conversation(&dm).unwrap();
        for author in [f.me, f.carol] {
            let row = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "same timestamp".into(),
                801,
            );
            svc.message_repo.insert(&row).unwrap();
        }
        for (author, ts) in [(f.me, 801), (f.carol, 801)] {
            let target = anon
                .get_message_by_sender_timestamp(&dm.id, &author, ts)
                .unwrap()
                .unwrap();
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let bytes = prost::Message::encode_to_vec(&wire);
            let content =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                anon.process_incoming_content(&dm.id, &f.bob, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
            let events = anon
                .process_incoming_content(&dm.id, &f.carol, &content)
                .unwrap();
            assert_eq!(events.len(), 1);
            if author == f.me {
                assert!(
                    anon.get_message_by_sender_timestamp(&dm.id, &f.carol, ts)
                        .unwrap()
                        .is_some()
                );
            }
            match &events[0] {
                crate::inbox::handlers::DomainEvent::MessageDeleted {
                    message_id,
                    author_id,
                    ..
                } => {
                    assert_eq!(*message_id, target.id);
                    assert_eq!(*author_id, author);
                }
                _ => panic!("expected deletion event"),
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            svc.message_repo.insert(&target).unwrap();
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
        }
        for (author, ts) in [(f.me, 803), (f.carol, 804)] {
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            let wrong_type = Message::new_text(
                dm.id.into(),
                ConversationType::AnonGroup,
                Some(author),
                "group".into(),
                ts,
            );
            svc.message_repo.insert(&wrong_type).unwrap();
            assert!(svc.message_repo.get(&wrong_type.id).unwrap().is_some());
            svc.message_repo.delete(&wrong_type.id).unwrap();
            let target = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "late".into(),
                ts,
            );
            for _ in 0..2 {
                svc.message_repo.insert(&target).unwrap();
                assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            }
        }
        // An explicit missing delete leaves the other author's collision alive.
        for (timestamp, explicit) in [(820, true)] {
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                timestamp,
                explicit.then(|| f.me.to_string()).as_deref(),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            let survivor_author = if explicit { f.carol } else { f.me };
            let survivor = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(survivor_author),
                "survivor".into(),
                timestamp,
            );
            svc.message_repo.insert(&survivor).unwrap();
            assert!(svc.message_repo.get(&survivor.id).unwrap().is_some());
        }
        let peer_message = Message::new_text(
            dm.id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "peer author".into(),
            822,
        );
        svc.message_repo.insert(&peer_message).unwrap();
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            822,
            Some(&f.carol.to_string()),
            None,
        );
        assert_eq!(
            anon.process_incoming_content(&dm.id, &f.carol, &content)
                .unwrap()
                .len(),
            1
        );
        for (author, channel) in [(f.bob, None), (f.me, Some("forged-channel"))] {
            let target = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "refused".into(),
                823,
            );
            svc.message_repo.insert(&target).unwrap();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                823,
                Some(&author.to_string()),
                channel,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
        }
        for (timestamp, conversation, kind) in [
            (824, ChannelId::new(), ConversationType::AnonDm),
            (825, dm.id.into(), ConversationType::AnonGroup),
        ] {
            let target = Message::new_text(
                conversation,
                kind,
                Some(f.me),
                "other scope".into(),
                timestamp,
            );
            svc.message_repo.insert(&target).unwrap();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                timestamp,
                Some(&f.me.to_string()),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
            let late = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(f.me),
                "actual late anon DM".into(),
                timestamp,
            );
            svc.message_repo.insert(&late).unwrap();
            assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        }
        let destination_claim_target = Message::new_text(
            dm.id.into(),
            ConversationType::AnonDm,
            Some(f.me),
            "destination forgery".into(),
            826,
        );
        svc.message_repo.insert(&destination_claim_target).unwrap();
        let mut destination_claim =
            crate::services::gateway::builders::build_targeted_delete_content(
                826,
                Some(&f.me.to_string()),
                None,
            );
        if let Some(crate::proto::content_v1::content::Content::DataMessage(data)) =
            &mut destination_claim.content
        {
            data.delete.as_mut().unwrap().destination_user_id = Some(f.bob.to_string());
        }
        assert!(
            anon.process_incoming_content(&dm.id, &f.carol, &destination_claim)
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.message_repo
                .get(&destination_claim_target.id)
                .unwrap()
                .is_some()
        );
        let catchup = crate::services::gateway::build_text_content("late peer", 804);
        let events = anon
            .process_incoming_content(&dm.id, &f.carol, &catchup)
            .unwrap();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                crate::inbox::handlers::DomainEvent::MessageReceived { .. }
            )),
            "deleted catchup cannot emit a live bubble"
        );
        // The common group backend keeps the author-only lookup.
        let group_id = crate::domain::anon_group::AnonGroupId::new();
        let own = Message::new_text(
            group_id.into(),
            ConversationType::AnonGroup,
            Some(f.me),
            "own group".into(),
            805,
        );
        svc.message_repo.insert(&own).unwrap();
        assert!(
            !f.client
                .anon_groups()
                .delete_message_by_timestamp(&group_id, &f.carol, 805)
                .unwrap()
        );
        assert!(svc.message_repo.get(&own.id).unwrap().is_some());
        let group_target = Message::new_text(
            group_id.into(),
            ConversationType::AnonGroup,
            Some(f.carol),
            "group target".into(),
            807,
        );
        let type_collision = Message::new_text(
            group_id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "type collision".into(),
            807,
        );
        svc.message_repo.insert(&group_target).unwrap();
        svc.message_repo.insert(&type_collision).unwrap();
        let control = crate::services::gateway::builders::build_targeted_delete_content(
            807,
            Some(&f.carol.to_string()),
            None,
        );
        let group_events = crate::inbox::handlers::base_anon::dispatch_common_content(
            &group_id,
            &f.carol,
            &control,
            f.client.anon_groups().as_ref(),
        );
        assert!(
            matches!(group_events, crate::inbox::handlers::base_anon::CommonDispatchOutcome::Handled(events) if events.len() == 1)
        );
        assert!(
            svc.message_repo
                .get(&group_target.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        assert!(
            !svc.message_repo
                .get(&type_collision.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        let only_collision = Message::new_text(
            group_id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "no group target".into(),
            808,
        );
        svc.message_repo.insert(&only_collision).unwrap();
        assert!(
            !f.client
                .anon_groups()
                .delete_message_by_timestamp(&group_id, &f.carol, 808)
                .unwrap()
        );
        assert!(
            !svc.message_repo
                .get(&only_collision.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        // Likewise the channel receiver cannot borrow the DM peer permission.
        let channel = Message::new_text(
            f.channel,
            ConversationType::Channel,
            Some(f.me),
            "own channel".into(),
            806,
        );
        svc.message_repo.insert(&channel).unwrap();
        assert!(
            svc.apply_channel_delete(&f.channel, &f.carol, 806)
                .unwrap()
                .is_none()
        );
        assert!(svc.message_repo.get(&channel.id).unwrap().is_some());
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

    /// Connected control dispatch -> processor -> scoped ledger reproduction.
    /// Missing display membership refuses even a locally held target; hydration
    /// changes admission only for a new delivery, without replaying the refusal.
    #[tokio::test]
    async fn channel_reaction_connected_membership_hydration_diagnosis() {
        use crate::inbox::{
            events::InboxEvent, polling::device::dispatch_control_content, state::InboxStats,
        };
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let scope =
            crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel);
        let stored_reactors = || {
            svc.message_repo
                .get_reactions(&scope, &f.carol, CH_TS)
                .unwrap()
                .into_iter()
                .map(|r| r.user_id)
                .collect::<Vec<_>>()
        };
        assert!(
            svc.record_reaction_at(&scope, &f.carol, CH_TS, &f.me, "👍", false, 1)
                .unwrap()
        );
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(8);
        let mut domain = f.client.event_bus().subscribe_domain();
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let reaction = crate::services::build_reaction_content_with_channel(
            "👍",
            false,
            &f.carol.to_string(),
            CH_TS,
            Some(&f.channel.to_string()),
        )
        .content
        .unwrap();
        dispatch_control_content(reaction.clone(), &f.bob, 1, "synthetic", 2, &tx, &stats);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, InboxEvent::ChannelReaction { .. }));
        processor.process(&event).await;
        assert_eq!(stored_reactors(), vec![f.me]);
        assert!(
            domain.try_recv().is_err(),
            "refusal emits no reaction domain event"
        );

        // Same repository operation used by get_all_channel_members on open.
        let mut roster = svc.channel_repo.list_members(&f.channel).unwrap();
        let mut member = roster[0].clone();
        member.user_id = f.bob;
        roster.push(member);
        svc.channel_repo
            .replace_memberships(&f.channel, &roster)
            .unwrap();
        assert_eq!(
            stored_reactors(),
            vec![f.me],
            "hydration cannot replay a refused reaction"
        );
        dispatch_control_content(
            reaction.clone(),
            &f.bob,
            1,
            "synthetic-replay",
            2,
            &tx,
            &stats,
        );
        processor.process(&rx.try_recv().unwrap()).await;
        let stored = stored_reactors();
        assert_eq!(stored.len(), 2);
        assert!(stored.contains(&f.me) && stored.contains(&f.bob));
        assert!(matches!(
            domain.try_recv().unwrap(),
            crate::inbox::handlers::DomainEvent::ReactionAdded { .. }
        ));
        dispatch_control_content(reaction, &f.bob, 1, "synthetic-duplicate", 2, &tx, &stats);
        processor.process(&rx.try_recv().unwrap()).await;
        assert_eq!(stored_reactors().len(), 2);
        assert!(
            domain.try_recv().is_err(),
            "duplicate does not emit a second delta"
        );
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
        let channel_scope =
            crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel);
        svc.message_repo
            .insert(&Message::new_text(
                f.channel,
                ConversationType::Channel,
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
            "own-device ambiguity is never guessed"
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
                .get_reactions(&channel_scope, &author, DM_TS)
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.record_inbound_reaction(&channel_scope, &f.carol, &author, DM_TS, "👍", false, 1)
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
                .get_by_reaction_scope(&channel_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts[0]
                .count,
            1
        );
        assert!(
            svc.record_inbound_reaction(&channel_scope, &f.carol, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_by_reaction_scope(&channel_scope, &author, DM_TS)
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
    #[cfg(feature = "media")]
    mod stable_call_delete_tests {
        include!("stable_call_delete_tests.rs");
    }
}
