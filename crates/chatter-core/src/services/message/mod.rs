//! Message service - modular implementation.
//!
//! Send, edit, delete, forward, and quote messages.
//!
//! This module is organized into focused submodules:
//! - `dm` - DM operations (Signal Protocol encryption)
//! - `group` - Group/sender key operations
//! - `core` - Outgoing message operations (send, edit, delete, reply, forward)
//! - `saved` - Saved messages (Note to Self)
//! - `history`, `read_state`, and `storage` - Local message state
//! - `modifiers` and `reactions` - Scoped incoming message changes
//! - `conversation` - Conversation classification and encryption lookup

mod history;
pub use history::retained_history_range;
mod read_state;
mod modifiers;
mod storage;
mod reactions;
mod conversation;
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
pub(crate) use channel_send::{owns as channel_send_owns, spawn_channel_send_recovery};
pub(crate) mod dm_outbox;
pub(crate) use dm_outbox::spawn_dm_send_recovery;
pub mod conversion;
pub mod core;
mod saved;
pub use saved::SavedSyncOutcome;
pub(crate) use saved::persist_saved_device_snapshot;
pub(crate) use saved::spawn_saved_sync_recovery;
mod server_dm;
mod shield_retry;
pub use call_event::ChannelCallPhase;
pub(crate) use shield_retry::spawn_shield_send_recovery;
mod receipts;
pub use receipts::{advance_delivery_status, display_delivery_status};

use std::collections::HashSet;
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

    /// No prefetched token for a Shield bootstrap. User posts are parked for
    /// the independent retry schedule; this error never triggers a refill.
    #[error("Shield is getting ready — will send automatically")]
    ShieldNotReady,

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
    pub(crate) receive_auth: Arc<std::sync::OnceLock<crate::state::AuthState>>,
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
    pub(crate) channel_decrypt_failures:
        Arc<parking_lot::Mutex<channel_inbound::DecryptFailureGate>>,
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
    /// Peer devices announced as added whose session could not be opened then
    /// (a transient failure): `(user, device)`. They count as lacking our
    /// sender key, so the next group send tries again instead of waiting for
    /// the device's own SenderKeyRequest (§125). Cleared when an SKDM reaches
    /// the device or it is revoked. In memory: a restart falls back to that
    /// request.
    pub(crate) unreached_peer_devices:
        Arc<parking_lot::Mutex<std::collections::HashSet<(String, u32)>>>,
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
mod inbound_target_tests;
