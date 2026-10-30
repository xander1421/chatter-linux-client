//! SDK facade for chatter-core.
//!
//! [`ChatterClient`] is the main entry point for consumers of this library.
//! It wraps the internal [`AppContext`] and [`CoreServices`] with a clean,
//! discoverable API. The implementation is split across submodules that each
//! add a focused set of methods to `ChatterClient` via `impl` blocks:
//!
//! * `auth` — login / register / recovery / PIN flows
//! * `calls` — DM / group call orchestration (desktop + mobile)
//! * `channels` — channel CRUD + membership
//! * `messaging` — message list / get / mark-read thin façades
//! * `profile` — user display name + friend code + QR façades
//! * `pin` — pinned-message façade
//! * `anon_group` — anonymous group membership façade
//! * `jarvis` — Jarvis AI assistant façade

mod anon_group;
mod appearance;
mod auth;
mod avatars;
pub mod calls;
pub mod channels;
mod composer;
#[cfg(feature = "media")]
mod group_ring_lifecycle;
pub use composer::{ComposerDraft, ComposerReply, ComposerSend};
pub use picker_history::PickerHistoryScope;
pub mod events;
#[cfg(feature = "jarvis")]
mod jarvis;
#[cfg(feature = "jarvis")]
pub use jarvis::{
    JARVIS_CHANNEL_NAME, JarvisRow, JarvisSessionOwner, is_jarvis_channel, jarvis_channel_id, jarvis_channel_reply_text,
};
mod link_device;
pub mod membership;
mod messaging;
mod mentions;
mod search_visible;
mod send_attempt;
pub use search_visible::{SearchPage, VisibleSearchHit};
pub use send_attempt::await_send_with_notice;
mod receipts;
pub use anon_group::{AnonGroupInviteCandidate, PrivateGroupRoster};
pub use messaging::{ChannelInviteRow, ForwardDestination, InviteCandidate, ReplyParentMedia};
pub use profile::{FriendCodeConnection, FriendCodeConnectionError};
pub use secret_chat::{SecretChatError, SecretChatGate};
pub use crate::network::connection::ConnectionState;

/// Rows removed by a local expiry sweep and attachments no longer referenced
/// by any stored message.
#[derive(Clone, Debug)]
pub struct ExpiredMessages {
    pub messages: Vec<crate::domain::message::Message>,
    pub removed_attachment_ids: Vec<crate::domain::ids::AttachmentId>,
}

/// A user-search hit surfaced to the UI (friends modal search).
///
/// Privacy note: display names are profile-key-encrypted on the server, so the
/// only server-side discovery that can resolve a stranger is an EXACT
/// `nickname.discriminator` username lookup (Signal's model). This struct is
/// what [`ChatterClient::search_users`] returns per hit.
#[derive(Clone, Debug)]
pub struct UserSearchResult {
    pub user_id: crate::domain::ids::UserId,
    pub display_name: String,
    pub username: String,
    pub is_friend: bool,
}

/// Absolute unread badge values — see [`ChatterClient::unread_badges`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnreadBadges {
    /// Unread messages on one sidebar row.
    pub row: u32,
    /// Conversations with unread messages that the current mode shows.
    pub total: u32,
}

/// Counts returned by a completed message-history import.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportSummary {
    pub tables: usize,
    pub rows_seen: usize,
    pub rows_inserted: usize,
    pub rows_skipped: usize,
}

impl From<crate::storage::backup_data::ImportSummary> for ImportSummary {
    fn from(summary: crate::storage::backup_data::ImportSummary) -> Self {
        Self {
            tables: summary.tables,
            rows_seen: summary.rows_seen,
            rows_inserted: summary.rows_inserted,
            rows_skipped: summary.rows_skipped,
        }
    }
}

/// Public mesh trust state without storage implementation details.
#[cfg(feature = "mesh")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshBindingStatus {
    Unverified,
    BindingOffered,
    Verified,
    KeyChanged,
}

#[cfg(feature = "mesh")]
impl MeshBindingStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unverified => "Mesh peer · self-asserted name",
            Self::BindingOffered => "Binding pending",
            Self::Verified => "Verified Chatter contact",
            Self::KeyChanged => "Identity changed · verify again",
        }
    }
}

#[cfg(feature = "mesh")]
impl From<crate::storage::mesh_binding_store::PeerBinding> for MeshBindingStatus {
    fn from(value: crate::storage::mesh_binding_store::PeerBinding) -> Self {
        use crate::storage::mesh_binding_store::PeerBinding;
        match value {
            PeerBinding::Unverified => Self::Unverified,
            PeerBinding::BindingOffered => Self::BindingOffered,
            PeerBinding::Verified { .. } => Self::Verified,
            PeerBinding::KeyChanged => Self::KeyChanged,
        }
    }
}

mod notification;
mod activity;
pub use activity::ActivityStatus;
mod picker_history;
mod pin;
pub use pin::PinnedDisplayItem;
mod profile;
mod profile_visibility;
mod presence;
pub mod preview;
mod push_config;
mod secret_chat;
mod shared_content;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai::LocalAIService;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::{AttachmentError, AttachmentService};
use crate::services::auth_service::AuthError;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::conversation::ConversationHandle;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OutgoingIntent, OwnedOutgoingServices, QuoteIntent, SendError, SendResult, route_outgoing_owned,
};
#[cfg(feature = "jarvis")]
use crate::services::jarvis::JarvisService;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::section_service::SectionService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal::SignalService;
use crate::services::user::UserService;
#[cfg(feature = "jarvis")]
use crate::state::JarvisState;
use crate::state::chat_state::PendingAttachment;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState, UserState,
};
use crate::storage::settings_keys;
use crate::system::Directories;

/// Builder for [`ChatterClient`].
#[derive(Default)]
pub struct ClientBuilder {
    server_url: Option<String>,
    data_dir: Option<String>,
    notification_sink: Option<Arc<dyn crate::system::notification_sink::NotificationSink>>,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            server_url: None,
            data_dir: None,
            notification_sink: None,
        }
    }

    /// Set the server URL. Falls back to `CHATTER_SERVER_URL` env var, then built-in default.
    /// It is passed to the network pool explicitly, never via the environment.
    pub fn server_url(mut self, url: impl Into<String>) -> Self {
        self.server_url = Some(url.into());
        self
    }

    /// Set the base data directory. Required on Android (`context.filesDir`).
    /// On desktop, XDG directories are used by default.
    pub fn data_dir(mut self, dir: impl Into<String>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Inject a platform-specific notification sink. If not set, core picks a
    /// sensible default (with `desktop-native`: `DesktopNotifications`; otherwise no-op).
    pub fn notification_sink(
        mut self,
        sink: Arc<dyn crate::system::notification_sink::NotificationSink>,
    ) -> Self {
        self.notification_sink = Some(sink);
        self
    }

    /// Build the client. Initializes database, crypto, and network pool.
    ///
    /// Must be called within a tokio runtime (e.g. inside `#[tokio::main]`
    /// or after `Runtime::new()`).
    pub fn build(self) -> Result<Arc<ChatterClient>, ClientError> {
        let ctx = AppContext::with_config_and_sink(
            self.server_url,
            self.data_dir,
            self.notification_sink,
        )?;
        let services = ctx.core_services();
        let activity = Arc::new(crate::services::activity_service::ActivityService::new(services.database.clone()));
        let anon_mode = services.message_service.shield_mode.clone();
        let client = Arc::new(ChatterClient {
            ctx,
            services,
            activity,
            activity_listener_started: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            pending_receipts: Mutex::new(Vec::new()),
            sent_receipts: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            channel_delivery_shutdown: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(0),
            presence_controller: presence::PresenceController::new(),
            push_shutdown: Mutex::new(None),
            call_signaling: std::sync::OnceLock::new(),
            svr_backup_service: std::sync::OnceLock::new(),
            message_backup_service: std::sync::OnceLock::new(),
            outgoing_typing: crate::services::outgoing_typing::OutgoingTypingManager::new(),
            link_device_service: std::sync::OnceLock::new(),
            anon_mode,
            pending_update: Mutex::new(None),
            update_transport_epoch: tokio::sync::watch::channel(0u64).0,
            authed_notif_started: AtomicBool::new(false),
            auth_lifecycle_lock: tokio::sync::Mutex::new(()),
            authed_notif_session: Mutex::new(None),
            #[cfg(feature = "media")]
            call_notification_bridge: Mutex::new(None),
            notif_stream_handle: Mutex::new(None),
            mailbox_wake_task: Mutex::new(None),
            push_url: Mutex::new(push_config::configured_push_url()),
            transport_switch_lock: tokio::sync::Mutex::new(()),
            server_monitor_started: AtomicBool::new(false),
            server_contact_deferred: AtomicBool::new(false),
            composer_drafts: Mutex::new(std::collections::HashMap::new()),
            composer_draft_revision: Mutex::new(0),
            composer_draft_owner: Mutex::new(None),
            parked_sends: Mutex::new(Vec::new()),
            composer_recovery_unread: AtomicBool::new(false),
            composer_drafts_unread: AtomicBool::new(false),
            composer_drafts_dirty: AtomicBool::new(false),
            composer_recovery_dirty: AtomicBool::new(false),
            composer_owner: Mutex::new(String::new()),
            expired_messages: tokio::sync::broadcast::channel(16).0,
        });
        client.cleanup_staged_upload_temps().map_err(|e| {
            ClientError::Validation(format!("Cannot clean staged upload files: {e}"))
        })?;
        client.discard_legacy_staging_all();
        client.install_session_recovery();
        Ok(client)
    }
}

/// Errors from [`ChatterClient`] operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("initialization failed: {0}")]
    Init(#[from] ContextError),
    #[error("connection failed: {0}")]
    Connection(#[from] NetworkError),
    #[error("not connected")]
    NotConnected,
    #[error("inbox error: {0}")]
    Inbox(String),
    #[error("Call ended")]
    CallEnded,
    #[error("inbox already started")]
    AlreadyStarted,
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Validation(String),
    /// Preserve restore failures so callers can identify a newer backup.
    #[error("backup restore failed: {0}")]
    BackupRestore(crate::services::message_backup_service::MessageBackupError),
    /// Login authenticated, but this device has no Signal identity key and one
    /// could not be recovered.
    ///
    /// Deliberately an error and not a flag on a successful result: the
    /// previous design returned `Ok` with `keys_missing = true`, every caller
    /// ignored it, and total key loss looked exactly like a normal login. The
    /// payload says which of "retype the PIN", "wait", or "accept a new
    /// identity" applies — see [`KeyRecoveryNeed::user_message`].
    #[error("{}", .0.user_message())]
    IdentityRecoveryRequired(crate::services::key_recovery::KeyRecoveryNeed),
}

/// Safe, typed failure shown before the main window can be created.
#[derive(Clone, Copy, Debug)]
pub enum StartupErrorKind {
    Storage,
    Keyring,
    Network,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct StartupError {
    pub kind: StartupErrorKind,
}

impl StartupError {
    pub fn message(self) -> &'static str {
        match self.kind {
            StartupErrorKind::Storage => "Chatter could not open its local data.",
            StartupErrorKind::Keyring => "Chatter could not unlock its local data.",
            StartupErrorKind::Network => "Chatter could not set up its network connection.",
            StartupErrorKind::Other => "Chatter could not finish starting.",
        }
    }

    pub fn details(self) -> &'static str {
        match self.kind {
            StartupErrorKind::Storage => "Local storage initialization failed. Check available space and access to the app data folder, then retry.",
            StartupErrorKind::Keyring => "The system credential store did not provide the database key. Unlock the credential store, then retry.",
            StartupErrorKind::Network => "Network setup failed. Check your connection and server settings, then retry.",
            StartupErrorKind::Other => "An initialization step failed. Retry, or restart the app if the problem continues.",
        }
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).message())
    }
}

impl From<ClientError> for StartupError {
    fn from(error: ClientError) -> Self {
        use crate::app::ContextError;
        use crate::error::SystemError;
        let kind = match error {
            ClientError::Init(ContextError::Storage(_)) => StartupErrorKind::Storage,
            ClientError::Init(ContextError::System(SystemError::Keyring(_))) => StartupErrorKind::Keyring,
            ClientError::Init(ContextError::Network(_)) | ClientError::Connection(_) => StartupErrorKind::Network,
            _ => StartupErrorKind::Other,
        };
        Self { kind }
    }
}

impl ClientError {
    /// Whether a message-history backup requires a newer app to restore.
    pub fn is_unsupported_backup(&self) -> bool {
        matches!(
            self,
            Self::BackupRestore(
                crate::services::message_backup_service::MessageBackupError::Storage(
                    crate::error::StorageError::UnsupportedBackup(_)
                )
            )
        )
    }
}

/// Result of a successful recovery code usage.
///
/// After this, the user can login with their new password + new PIN.
/// SVR backup is created automatically during the subsequent login flow.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Number of unused recovery codes remaining on the account.
    pub remaining_codes: i32,
    /// The account's SVR key backup was dropped by this recovery.
    ///
    /// Always true, and carried explicitly rather than left implicit because
    /// the consequence is not recoverable: the backup was encrypted and indexed
    /// by a secret derived from the OLD PIN alone, so a recovery-code login
    /// makes it unreadable by everyone, including its owner. Signal's SVR does
    /// the same thing whenever the PIN can no longer open the data — it
    /// destroys it rather than leaving a row nobody can decrypt.
    ///
    /// If this device has no local identity keys, they were in that backup and
    /// are now gone. The caller must say so; swallowing this is what made the
    /// loss invisible.
    pub key_backup_invalidated: bool,
}

/// What to put in front of the user when a peer's identity key changed and the
/// conversation is wedged pending their decision.
///
/// Built by [`ChatterClient::identity_change_prompt`]. The reason this is core
/// data and not UI copy is `attributed`: it decides whether the peer may be
/// named at all. Presenting a server-asserted identity change as though it had
/// been cryptographically attributed is the exact failure
/// [`crate::error::IdentityChangeSource`] exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityChangePrompt {
    /// True only when the peer id was proven cryptographically (a sender
    /// certificate validated against the pinned trust root).
    pub attributed: bool,
    /// One-line headline. Names the peer only when `attributed`.
    pub title: String,
    /// What it means and what to do before accepting.
    pub detail: String,
}

impl IdentityChangePrompt {
    fn new(attributed: bool, peer_display_name: &str) -> Self {
        // Signal's phrasing ("Your safety number with X has changed"), kept
        // because it is the wording users have already been taught elsewhere.
        if attributed {
            return Self {
                attributed,
                title: format!("Your safety number with {peer_display_name} has changed"),
                detail: "They most likely reinstalled the app or switched devices. It can \
                         also mean someone is intercepting this conversation. Messages are \
                         blocked in both directions until you accept the new safety number \
                         — compare it with them over a call or in person first."
                    .to_string(),
            };
        }
        // Unattributed: the ONLY thing that named this peer was the server, so
        // the headline must not state that it was them.
        Self {
            attributed,
            title: "A safety number in this conversation has changed".to_string(),
            detail: "The server says this came from your contact, but nothing proved that, \
                     so it may not be them at all. Messages are blocked in both directions \
                     until you accept the new safety number — confirm it with them over a \
                     call or in person before you do."
                .to_string(),
        }
    }
}

/// Actions available in the open conversation header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConversationActions {
    pub can_call: bool,
    pub can_invite: bool,
    pub can_open_settings: bool,
    pub can_open_pinned_list: bool,
}

/// The main entry point for the chatter-core SDK.
///
/// # Example
/// ```rust,no_run
/// use chatter_core::prelude::*;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let client = ChatterClient::builder()
///         .server_url("https://chat.example.com")
///         .build()?;
///
///     // Subscribe BEFORE starting inbox to avoid missing events
///     let mut events = client.subscribe_events();
///
///     client.connect().await?;
///     // client.auth().login(...).await?;
///     client.start().await?;
///     Ok(())
/// }
/// ```
///
/// # Shutdown
///
/// Call [`shutdown()`](Self::shutdown) before dropping for graceful cleanup.
/// The `Drop` impl aborts background tasks but cannot await async cleanup.
pub struct ChatterClient {
    ctx: AppContext,
    services: Arc<CoreServices>,
    activity: Arc<crate::services::activity_service::ActivityService>,
    activity_listener_started: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Failed ephemeral receipts for this process and auth session only.
    pub(crate) pending_receipts: Mutex<Vec<crate::services::conversation::PendingReceipt>>,
    pub(crate) sent_receipts: Mutex<Vec<crate::services::conversation::PendingReceipt>>,
    started: AtomicBool,
    channel_delivery_shutdown: AtomicBool,
    /// Epoch millis of last user activity (for idle detection).
    last_activity_ms: AtomicU64,
    presence_controller: presence::PresenceController,
    /// Shutdown channel for the active ntfy push listener task, if any.
    /// Reset / replaced on every `configure_push` call so the caller can
    /// swap URLs (or disable push) idempotently.
    push_shutdown: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
    /// Cached call signaling service (persistent for pending state).
    call_signaling: std::sync::OnceLock<crate::services::call_signaling::CallSignalingService>,
    /// Outgoing typing-indicator state machine (Signal refresh/pause timers).
    outgoing_typing: crate::services::outgoing_typing::OutgoingTypingManager,
    /// Cached SVR backup service.
    svr_backup_service: std::sync::OnceLock<crate::services::svr_backup_service::SvrBackupService>,
    /// Cached message-history backup service (L7 message-backup).
    message_backup_service:
        std::sync::OnceLock<crate::services::message_backup_service::MessageBackupService>,
    /// Cached device-linking service (lazy because deps are read-only and only
    /// touched when the user opens the link-device flow).
    link_device_service:
        std::sync::OnceLock<Arc<crate::services::link_device_service::LinkDeviceService>>,
    /// In-memory Shield ("anon mode") flag. When set, the UI hides public
    /// channels, and a friend DM travels over the NDSS'21 mailbox instead of
    /// sealed-sender streaming — the flag is the SAME `Arc` `MessageService`
    /// routes friend DMs by (`send_friend_dm_content`). Resets to false on
    /// every client construction (not persisted by design — auto revert on
    /// restart is the privacy property).
    anon_mode: Arc<AtomicBool>,
    pending_update: Mutex<Option<crate::update::UpdateManifest>>,
    update_transport_epoch: tokio::sync::watch::Sender<u64>,
    /// Guards the auth-gated notification streams (gRPC message stream + call
    /// → notification bridge) so they spawn exactly once, after login. They
    /// can't start at boot like the consumer because there's no authenticated
    /// user / call service yet — see `start_authed_notification_streams`.
    authed_notif_started: AtomicBool,
    auth_lifecycle_lock: tokio::sync::Mutex<()>,
    authed_notif_session: Mutex<Option<u64>>,
    #[cfg(feature = "media")]
    call_notification_bridge: Mutex<
        Option<(
            u64,
            crate::services::call_notification_bridge::SessionBridge,
        )>,
    >,
    /// Handle to the long-lived gRPC notification-stream consumer, held apart
    /// from the `tasks` bag so a transport switch (anon mode toggle) can abort
    /// and re-spawn just that loop — forcing it to re-dial over the new
    /// transport instead of riding the pre-switch channel until it naturally
    /// reconnects. See `respawn_notification_stream`.
    notif_stream_handle: Mutex<Option<JoinHandle<()>>>,
    /// Owns the dynamic device/mailbox subscription supervisor.
    mailbox_wake_task: Mutex<Option<tokio::task::AbortHandle>>,
    /// Last non-empty ntfy push URL configured via `configure_push`, remembered
    /// so anon mode can suspend push (poll-only) on enter and restore it on
    /// exit. `None` means push was never configured / explicitly disabled.
    push_url: Mutex<Option<String>>,
    /// Serializes the async loop teardown/rebuild that runs on every anon-mode
    /// toggle, so two rapid toggles can't interleave their inbox stop/start.
    /// Each rebuild reads live pool transport state, so any ordering converges
    /// to the final transport.
    transport_switch_lock: tokio::sync::Mutex<()>,
    /// Guards the server-reachability monitor so it spawns exactly once.
    /// See `start_server_monitor`.
    server_monitor_started: AtomicBool,
    /// Signed-out startup has not contacted the server yet, and won't until a
    /// sign-in action calls `connect` — see `startup_connect`.
    server_contact_deferred: AtomicBool,
    /// Unsent composer text and reply target per conversation id, so a draft
    /// stays with the conversation it was typed in. See `swap_composer_draft`.
    /// Mirrored to the signed-in account's settings row on every change
    /// (`persist_composer_drafts`), so it survives a restart.
    composer_drafts: Mutex<std::collections::HashMap<String, ComposerDraft>>,
    composer_draft_revision: Mutex<u64>,
    /// The account whose persisted drafts the map holds, deleted at sign-out.
    composer_draft_owner: Mutex<Option<crate::domain::UserId>>,
    /// Failed composed sends kept until their composer has room for them.
    /// See `recover_composer_send`. In memory, like the drafts.
    parked_sends: Mutex<Vec<ComposerSend>>,
    /// The signed-in account's recovery row could not be read this session,
    /// so it is not written over. See `restore_composer_recovery`.
    composer_recovery_unread: AtomicBool,
    composer_drafts_unread: AtomicBool,
    composer_drafts_dirty: AtomicBool,
    composer_recovery_dirty: AtomicBool,
    /// The conversation whose draft the composer holds: the `entering` of the
    /// last `swap_composer_draft`. Lags the active conversation until the
    /// UI's draft watcher has run.
    composer_owner: Mutex<String>,
    /// Messages the disappearing-messages reaper just deleted. See
    /// `subscribe_expired_messages`.
    expired_messages: tokio::sync::broadcast::Sender<ExpiredMessages>,
}

pub use crate::services::user::ReceivedFriendRequestProfile;

mod core_access;
mod call_facade;
mod mesh;
mod backup;
mod trust;
mod event_streams;
mod ai_settings;
mod account_settings;
mod sidebar;
mod auth_state;
pub use auth_state::{ConversationGuard, SessionGuard};
mod thread;
mod shield;
mod infrastructure;
mod startup;
mod outgoing;
mod conversation_listing;
pub use conversation_listing::{member_roster_rows, MemberRosterRow};
mod social;
mod profile_facade;
mod presence_facade;
mod integration_facade;
mod sync_facade;
mod messaging_settings;
mod shutdown;
mod bridges;
mod push_facade;
mod server_records;

#[cfg(test)]
mod facade_tests;

impl Drop for ChatterClient {
    fn drop(&mut self) {
        crate::system::messaging_background::set_signed_in(false);
        crate::network::streaming::mailbox_wakes::clear();
        let tasks = self.tasks.lock();
        for handle in tasks.iter() {
            handle.abort();
        }
    }
}

// =============================================================================
// SDK types
// =============================================================================

#[derive(Debug, Default)]
pub struct SendOptions {
    pub thread_id: Option<crate::domain::ids::MessageId>,
    pub mentioned_integrations: Vec<crate::domain::ids::IntegrationId>,
    pub targets_ai_bot: bool,
}

pub struct UploadedAttachments {
    pub pointers: Vec<AttachmentPointer>,
    pub attachments: Vec<Attachment>,
}

/// Destination resolved from an account-scoped Android notification key.
pub enum NotificationTarget {
    Conversation(crate::state::ActiveConversation),
    SecretRequest,
}

// =============================================================================
// Private helpers
// =============================================================================

async fn upload_attachments(
    pending: &[PendingAttachment],
    attachment_service: &AttachmentService,
    client: Option<&ChatterClient>,
) -> Result<UploadedAttachments, AttachmentError> {
    let mut pointers = Vec::with_capacity(pending.len());
    let mut attachments = Vec::with_capacity(pending.len());
    for att in pending {
        if let Some(pointer) = &att.pointer {
            pointers.push(pointer.clone());
            if let Ok(Some(full_att)) = attachment_service.get(&att.id) {
                attachments.push(full_att);
            }
            continue;
        }
        let staged = match client {
            Some(client) => client.staged_upload_file(&att.path).await?,
            None => None,
        };
        let upload_path = staged.as_ref().map(|(_, path)| path.as_path()).unwrap_or(&att.path);
        match attachment_service.upload_to_cdn_encrypted(upload_path).await {
            Ok(res) => {
                pointers.push(res.to_attachment_pointer());
                if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) {
                    attachments.push(full_att);
                }
            }
            Err(e) => {
                // Propagate rather than silently dropping the attachment: if this
                // was the only content in the message, swallowing the error here
                // would leave `pointers` empty and the send would instead fail
                // with the misleading "message must have text or attachments".
                tracing::error!(error = %e, "Failed to upload attachment");
                return Err(e);
            }
        }
    }
    Ok(UploadedAttachments {
        pointers,
        attachments,
    })
}

/// Received group-call keys handled at once by the call-policy listener;
/// beyond this a key is dropped rather than queued behind a stalled server.
#[cfg(feature = "media")]
const CALL_KEY_TASKS: usize = 8;

/// Rotate the group-call key for a join (or a key request) NOW: no settle
/// wait — the joiner hears nothing until it has the key. Single-flight: a
/// join arriving while a rotation runs makes it run once more when done
/// (that run reads the roster afresh), so a burst of joins costs at most two
/// rotations and none is missed (§124 1k device check, 2026-10-05).
#[cfg(feature = "media")]
fn schedule_join_rotation(
    client: &Arc<ChatterClient>,
    gate: &Arc<calls::JoinRotationGate>,
    children: &mut tokio::task::JoinSet<()>,
) {
    if !gate.request() {
        return;
    }
    let client = Arc::clone(client);
    let gate = Arc::clone(gate);
    children.spawn(run_join_rotation(client, gate));
}

#[cfg(feature = "media")]
async fn run_join_rotation(client: Arc<ChatterClient>, gate: Arc<calls::JoinRotationGate>) {
    loop {
        if let Err(e) = client.handle_participants_joined().await {
            tracing::error!(error = %e, "Failed to rotate the group call key for joiners");
        }
        if !gate.finish() {
            break;
        }
    }
}

/// Who left and who joined between two views of a call's room.
#[cfg(feature = "media")]
fn roster_changes(
    before: &std::collections::HashSet<String>,
    now: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    (
        before.difference(now).cloned().collect(),
        now.difference(before).cloned().collect(),
    )
}

/// Whether `id` is this user's Saved Messages (Note to Self) channel.
fn is_saved_channel(id: &crate::domain::ids::ChannelId, channel_service: &ChannelService) -> bool {
    channel_service
        .get_channel_local(id)
        .ok()
        .flatten()
        .is_some_and(|c| c.channel_type == ChannelType::SavedMessages)
}

fn build_intent(
    conversation: ActiveConversation,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<QuoteIntent>,
    options: &SendOptions,
    channel_service: &ChannelService,
) -> Result<OutgoingIntent, SendError> {
    match conversation {
        ActiveConversation::AnonDm(id) => Ok(OutgoingIntent::AnonDm {
            conversation_id: id,
            text: text.to_string(),
            attachment_pointers,
            quote,
        }),
        ActiveConversation::AnonGroup(id) => Ok(OutgoingIntent::AnonGroup {
            group_id: id,
            text: text.to_string(),
            attachment_pointers,
            quote,
        }),
        ActiveConversation::ServerDm { peer_id } => Ok(OutgoingIntent::ServerDm {
            peer_id,
            text: text.to_string(),
            attachment_pointers,
            quote,
        }),
        ActiveConversation::Channel(id) => {
            if is_saved_channel(&id, channel_service) {
                let att_ids = attachment_pointers
                    .iter()
                    .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key))
                    .collect();
                Ok(OutgoingIntent::SavedMessages {
                    text: text.to_string(),
                    attachment_ids: att_ids,
                    quote,
                })
            } else {
                Ok(OutgoingIntent::Channel {
                    channel_id: id,
                    text: text.to_string(),
                    attachment_pointers,
                    file_paths: vec![],
                    thread_id: options.thread_id,
                    mentioned_integrations: options.mentioned_integrations.clone(),
                    targets_ai_bot: options.targets_ai_bot,
                    quote,
                })
            }
        }
        ActiveConversation::None => Err(SendError::UnsupportedAction),
    }
}

/// The id of a bubble queued behind a DM bootstrap: the stored message's id
/// when there is one. Receipts, reactions and message info find a bubble by
/// that id (`MessageDeliveryChanged`), so a synthetic id left the first
/// Shield DM at "sent" after its peer's Receipt had made it "delivered".
/// Only an unstored send keeps the `pending-{sent_at}` placeholder.
fn queued_bubble_id(stored_id: String, sent_at: u64) -> String {
    if stored_id.is_empty() {
        format!("pending-{sent_at}")
    } else {
        stored_id
    }
}

/// Whether looking at a peer may trigger the global authenticated profile
/// fetch (`GetUserProfile` naming that peer). Never in Shield — an identified
/// fetch the moment a friend's DM opens is the link the mailbox hides — and
/// never from a secret chat, whatever the transport: even over Tor it would
/// tie the account to the peer it is secretly talking to. A secret chat shows
/// only the profile data carried inside it.
fn may_refresh_global_profile(
    shield: bool,
    conversation: &crate::state::ActiveConversation,
) -> bool {
    !shield && !conversation.is_anon()
}
