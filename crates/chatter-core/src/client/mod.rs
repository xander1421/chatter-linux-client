//! SDK facade for chatter-core.
//!
//! [`ChatterClient`] is the main entry point for consumers of this library.
//! It wraps the internal [`AppContext`] and [`CoreServices`] with a clean,
//! discoverable API. The implementation is split across submodules that each
//! add a focused set of methods to `ChatterClient` via `impl` blocks:
//!
//! * `auth` — login / register / recovery / PIN flows
//! * `calls` — DM / group / anonymous call orchestration (desktop + mobile)
//! * `channels` — channel CRUD + membership
//! * `messaging` — message list / get / mark-read thin façades
//! * `profile` — user display name + friend code + QR façades
//! * `pin` — pinned-message façade
//! * `anon_group` — anonymous group membership façade
//! * `jarvis` — Jarvis AI assistant façade

mod anon_group;
mod appearance;
mod auth;
pub mod calls;
pub mod channels;
pub mod events;
#[cfg(feature = "jarvis")]
mod jarvis;
#[cfg(feature = "jarvis")]
pub use jarvis::{is_jarvis_channel, jarvis_channel_id, JARVIS_CHANNEL_NAME};
mod link_device;
pub mod membership;
mod messaging;
mod receipts;
pub use messaging::{InviteCandidate, ReplyParentMedia, SectionRowExtras};
pub use anon_group::AnonGroupInviteCandidate;

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
mod notification;
mod pin;
mod profile;
mod profile_visibility;
mod shared_content;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::AuthError;
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::{AttachmentError, AttachmentService};
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
#[cfg(feature = "jarvis")]
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    PrivacyState, UserState,
};
#[cfg(feature = "jarvis")]
use crate::state::JarvisState;
use crate::state::chat_state::PendingAttachment;
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
    /// sensible default (desktop: `DesktopNotifications`; android: no-op).
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
        let ctx = AppContext::with_config_and_sink(self.server_url, self.data_dir, self.notification_sink)?;
        let services = ctx.core_services();
        let anon_mode = services.message_service.shield_mode.clone();
        Ok(Arc::new(ChatterClient {
            ctx,
            services,
            tasks: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(0),
            push_shutdown: Mutex::new(None),
            call_signaling: std::sync::OnceLock::new(),
            svr_backup_service: std::sync::OnceLock::new(),
            message_backup_service: std::sync::OnceLock::new(),
            outgoing_typing: crate::services::outgoing_typing::OutgoingTypingManager::new(),
            link_device_service: std::sync::OnceLock::new(),
            anon_mode,
            authed_notif_started: AtomicBool::new(false),
            notif_stream_handle: Mutex::new(None),
            push_url: Mutex::new(None),
            transport_switch_lock: tokio::sync::Mutex::new(()),
            server_monitor_started: AtomicBool::new(false),
            server_contact_deferred: AtomicBool::new(false),
            composer_drafts: Mutex::new(std::collections::HashMap::new()),
            expired_messages: tokio::sync::broadcast::channel(16).0,
        }))
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
    #[error("inbox already started")]
    AlreadyStarted,
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Validation(String),
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
    tasks: Mutex<Vec<JoinHandle<()>>>,
    started: AtomicBool,
    /// Epoch millis of last user activity (for idle detection).
    last_activity_ms: AtomicU64,
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
    link_device_service: std::sync::OnceLock<Arc<crate::services::link_device_service::LinkDeviceService>>,
    /// In-memory Shield ("anon mode") flag. When set, the UI hides public
    /// channels, and a friend DM travels over the NDSS'21 mailbox instead of
    /// sealed-sender streaming — the flag is the SAME `Arc` `MessageService`
    /// routes friend DMs by (`send_friend_dm_content`). Resets to false on
    /// every client construction (not persisted by design — auto revert on
    /// restart is the privacy property).
    anon_mode: Arc<AtomicBool>,
    /// Guards the auth-gated notification streams (gRPC message stream + call
    /// → notification bridge) so they spawn exactly once, after login. They
    /// can't start at boot like the consumer because there's no authenticated
    /// user / call service yet — see `start_authed_notification_streams`.
    authed_notif_started: AtomicBool,
    /// Handle to the long-lived gRPC notification-stream consumer, held apart
    /// from the `tasks` bag so a transport switch (anon mode toggle) can abort
    /// and re-spawn just that loop — forcing it to re-dial over the new
    /// transport instead of riding the pre-switch channel until it naturally
    /// reconnects. See `respawn_notification_stream`.
    notif_stream_handle: Mutex<Option<JoinHandle<()>>>,
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
    /// Unsent composer text per conversation id, so a draft stays with the
    /// conversation it was typed in. See `swap_composer_draft`.
    // in-memory, lost on restart.
    composer_drafts: Mutex<std::collections::HashMap<String, String>>,
    /// Messages the disappearing-messages reaper just deleted. See
    /// `subscribe_expired_messages`.
    expired_messages: tokio::sync::broadcast::Sender<Vec<crate::domain::message::Message>>,
}

pub use crate::services::user::ReceivedFriendRequestProfile;

impl ChatterClient {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Server-backed account inbox / channel management presentation.
    pub async fn server_records_page(&self, kind: &str, channel: &str, cursor: &str)
        -> Result<crate::services::server_records::RecordPage, crate::error::NetworkError>
    {
        let user = self.session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?.to_string();
        crate::services::server_records::page(&self.services.grpc_pool, kind, channel, cursor, &user).await
    }

    pub async fn server_records_action(&self, kind: &str, channel: &str, id: &str, action: &str)
        -> Result<(), crate::error::NetworkError>
    {
        crate::services::server_records::action(&self.services.grpc_pool, kind, channel, id, action).await
    }

    // ── Services (return &Arc<T> so .clone() yields Arc<T>) ─────────────
    //
    // Default visibility is `pub(crate)` — chatter-ui MUST route through the
    // high-level façade methods below, not raw service accessors, per the
    // `feedback_service_layer_architecture` memory rule. Integration tests
    // (the `tests/` directory, external to the lib) enable the `testing`
    // feature, which widens these to `pub` so fixtures can wire real
    // services against a live server.
    crate::service_accessor!(auth, AuthService, auth_service);
    crate::service_accessor!(messages, MessageService, message_service);
    crate::service_accessor!(channels, ChannelService, channel_service);
    crate::service_accessor!(users, UserService, user_service);
    crate::service_accessor!(attachments, AttachmentService, attachment_service);
    crate::service_accessor!(pins, PinService, pin_service);
    crate::service_accessor!(sessions, SessionService, session_service);
    crate::service_accessor!(anon_dms, AnonDmService, anon_dm_service);
    crate::service_accessor!(anon_groups, AnonGroupService, anon_group_service);
    crate::service_accessor!(signal, SignalService, signal_service);
    crate::service_accessor!(ai, AIService, ai_service);
    #[cfg(feature = "jarvis")]
    crate::service_accessor!(jarvis, JarvisService, jarvis_service);
    #[cfg(feature = "mesh")]
    crate::service_accessor!(mesh, crate::services::mesh::MeshService, mesh_service);

    // ── bitchat mesh façade ─────────────────────────────────────────
    //
    // `MeshService` is `pub(crate)`, so the UI reaches it through here. Most of
    // these are one-line forwards; `mesh_send` is not, and that is the point —
    // choosing between the room, a cell and a DM is a routing decision, and the
    // presentation layer must not be the thing that makes it.

    /// Live mesh events: messages, peers, transport state.
    #[cfg(feature = "mesh")]
    pub fn mesh_subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::services::mesh::MeshEvent> {
        self.mesh().subscribe()
    }

    /// Radio and relay state. `connected_peers` is the mesh's connectivity
    /// signal; zero is an ordinary state, not a failure.
    #[cfg(feature = "mesh")]
    pub fn mesh_status(&self) -> crate::services::mesh::MeshStatus {
        self.mesh().status()
    }

    /// Everyone currently visible on the radio.
    #[cfg(feature = "mesh")]
    pub fn mesh_peers(&self) -> Vec<crate::services::mesh::MeshPeer> {
        self.mesh().peers()
    }

    /// Open mesh conversations, for the sidebar.
    #[cfg(feature = "mesh")]
    pub fn mesh_conversations(
        &self,
    ) -> Result<Vec<crate::domain::unified_conversation::UnifiedConversation>, crate::services::mesh::MeshError>
    {
        self.mesh().conversations()
    }

    /// Which mesh scope a sidebar row refers to.
    #[cfg(feature = "mesh")]
    pub fn mesh_conversation_for(
        &self,
        id: &str,
    ) -> Option<crate::services::mesh::MeshConversation> {
        self.mesh().conversation_for(id)
    }

    /// Stored history for one mesh conversation, oldest first.
    #[cfg(feature = "mesh")]
    pub fn mesh_history(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::mesh::MeshError> {
        self.mesh().history(conversation, before_sent_at, limit)
    }

    /// Clear a mesh conversation's unread count locally.
    ///
    /// Local only. Telling the peer costs airtime and is a separate act.
    #[cfg(feature = "mesh")]
    pub fn mesh_mark_conversation_read(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().mark_conversation_read(conversation)
    }

    /// Join a geohash cell. Public: everything said there is readable by anyone
    /// subscribed to that cell.
    #[cfg(feature = "mesh")]
    pub fn mesh_join_geohash(&self, cell: &str) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().join_geohash(cell)
    }

    /// Leave a cell. History stays; only the sidebar entry goes.
    #[cfg(feature = "mesh")]
    pub fn mesh_leave_geohash(&self, cell: &str) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().leave_geohash(cell)
    }

    /// Our own mesh sender id, for telling our messages from everyone else's.
    ///
    /// Derived from the mesh peer ID and unrelated to the chatter account: the
    /// two identities are deliberately separate.
    #[cfg(feature = "mesh")]
    pub fn mesh_local_user_id(&self) -> crate::domain::ids::UserId {
        crate::services::mesh::mapping::peer_user_id(&self.mesh().local_peer())
    }

    /// The nickname behind a mesh sender id, or `None` if no known peer matches.
    ///
    /// Only meaningful for messages that came off the mesh. The ordinary
    /// display-name path cannot resolve these: a mesh peer has no row in any
    /// user repository, so `UserService::get_display_name` falls through to
    /// stringifying the id and the UI showed a raw UUID.
    #[cfg(feature = "mesh")]
    pub fn mesh_sender_display_name(
        &self,
        sender: &crate::domain::ids::UserId,
    ) -> Option<String> {
        self.mesh().sender_display_name(sender)
    }

    /// Start the mesh. Deliberately **not** part of [`start`](Self::start).
    ///
    /// `start` is the authenticated client coming up: inbox polling, channel
    /// streams, prekeys — all of which need an account. The mesh needs none of
    /// them. It makes no RPC, shares no identity with the account, and is the
    /// one part of this app that is supposed to work when there is no server,
    /// no internet, and no login.
    ///
    /// Gating it behind authentication would have broken exactly the situations
    /// it exists for: the disaster and protest scenarios bitchat targets are
    /// the ones where nobody is signing in to anything.
    ///
    /// Safe to call more than once; the service tracks whether it is running.
    #[cfg(feature = "mesh")]
    pub async fn start_mesh(&self) -> Result<(), crate::services::mesh::MeshError> {
        if !self.mesh_enabled() {
            return Ok(());
        }
        self.mesh().start().await
    }

    /// Stop the mesh: radio down, relays disconnected, runtimes joined.
    ///
    /// The service can be started again afterwards — see `CommandBus`, which
    /// exists so that a stop is not permanent. What is retained stays retained:
    /// stopping the transport does not discard the timeline, the identity, or the
    /// outbox, so anything already written is still there when it comes back.
    #[cfg(feature = "mesh")]
    pub async fn stop_mesh(&self) {
        self.mesh().shutdown().await;
    }

    /// Whether the user explicitly enabled mesh. Runtime radios default off.
    #[cfg(feature = "mesh")]
    #[must_use]
    pub fn mesh_enabled(&self) -> bool {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .get_bool(keys::MESH_ENABLED)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Record the preference and act on it now.
    ///
    /// Both halves, deliberately: a toggle that persists without taking effect
    /// until a restart is a toggle that appears broken, and one that takes effect
    /// without persisting comes back on by itself. A storage failure is reported
    /// rather than swallowed, because the setting silently not sticking is worse
    /// than being told it did not.
    #[cfg(feature = "mesh")]
    pub async fn set_mesh_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::MESH_ENABLED, enabled)?;
        if enabled {
            self.start_mesh().await
        } else {
            self.stop_mesh().await;
            Ok(())
        }
    }

    /// Whether the mesh service came up and is still up.
    ///
    /// Not "the radio is on" and not "a peer is in range" — just that the
    /// service exists, which is what decides whether there are mesh
    /// conversations to open at all.
    #[cfg(feature = "mesh")]
    #[must_use]
    pub fn mesh_running(&self) -> bool {
        self.mesh().is_running()
    }

    /// Whether this app has anything usable by someone with no account.
    ///
    /// True exactly when the mesh service is running. The mesh is serverless and
    /// account-free by construction (see [`start_mesh`](Self::start_mesh)), so a
    /// running mesh means a user who has never registered, and cannot register
    /// because there is no server to register with, still has conversations to
    /// open — and the sign-in screen has something to offer a way past itself
    /// *to*.
    ///
    /// It is deliberately the running service and not the `mesh` Cargo feature.
    /// A build with the feature whose `start` failed (no sealer key, for
    /// instance) has nothing behind the login screen, and offering a way past it
    /// would strand the user somewhere emptier than where they began.
    ///
    /// This says nothing about whether messages will be delivered. Delivery on a
    /// mesh depends on who is in radio range, is best-effort, and may be
    /// deferred by hours; the mesh surfaces say so themselves and this must not
    /// be read as a promise on their behalf.
    #[must_use]
    pub fn has_account_free_surfaces(&self) -> bool {
        #[cfg(feature = "mesh")]
        {
            self.mesh_running()
        }
        #[cfg(not(feature = "mesh"))]
        {
            false
        }
    }

    /// Start the server-reachability monitor.
    ///
    /// Deliberately **not** part of [`start`](Self::start), for the same reason
    /// [`start_mesh`](Self::start_mesh) is not: `start` is the authenticated
    /// client coming up, and the client that most needs to know whether the
    /// server is there is the one that could not sign in to it. Called from app
    /// startup regardless of login.
    ///
    /// Safe to call more than once — extra monitors would only duplicate probes,
    /// so the second call is a no-op.
    /// App startup's server contact. Signed in: connect (bounded by `budget`)
    /// and start the reachability monitor, as before. Signed out: contact
    /// nothing. The user has not chosen Shield yet, and a direct dial now —
    /// the boot connect or the monitor's probes — would show the server this
    /// IP opening the app seconds before an onion sign-up from it. Both wait
    /// for the first `connect`, which every sign-in / create-account /
    /// recovery / link-device action makes, by then on the chosen transport.
    ///
    /// Returns whether the server answered (always false when signed out).
    pub async fn startup_connect(&self, budget: std::time::Duration) -> bool {
        if !self.is_logged_in() {
            self.server_contact_deferred.store(true, Ordering::SeqCst);
            tracing::info!("signed out: no server contact until a sign-in action");
            return false;
        }
        let connected = match tokio::time::timeout(budget, self.connect()).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("Could not connect to server: {} (offline mode)", e);
                false
            }
            Err(_) => {
                tracing::warn!(
                    budget_s = budget.as_secs(),
                    "Server did not answer within the startup budget (offline mode)"
                );
                // Nothing wrote the outcome, because nothing finished. Record
                // it, or the UI would render the default until the first probe.
                self.offline_state().set_connection_state(
                    crate::network::connection::ConnectionState::Disconnected,
                );
                false
            }
        };
        self.start_server_monitor();
        connected
    }

    /// Whether the sign-in screens should say the server is unreachable. Not
    /// before anything has tried to reach it: a signed-out start defers all
    /// contact (`startup_connect`), and "unreachable" would be a guess.
    pub fn server_unreachable(&self) -> bool {
        !self.server_contact_deferred.load(Ordering::SeqCst)
            && self.offline_state().connection_state().is_unreachable()
    }

    pub fn start_server_monitor(&self) {
        if self.server_monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let handle = tokio::spawn(crate::network::server_monitor::run(
            self.services.grpc_pool.clone(),
            self.services.offline_state.clone(),
        ));
        self.tasks.lock().push(handle);
    }

    /// Panic wipe: destroy everything this device holds about the mesh.
    ///
    /// Identity keypair, retained mail, carried courier envelopes, gossip
    /// history, bridge dedup sets, every mesh message, the conversation
    /// directory, and favourites.
    ///
    /// **It does not touch the chatter account**, its Signal identity, or its
    /// messages. Those are a separate identity with a separate threat model,
    /// and destroying an account is a larger irreversible act that deserves its
    /// own confirmation rather than riding along with this one. Anyone relying
    /// on this to leave a device clean of everything needs to know that.
    #[cfg(feature = "mesh")]
    pub fn mesh_panic_wipe(&self) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().panic_wipe()
    }

    /// Send into a mesh conversation, choosing the transport from the scope.
    ///
    /// The room and a cell are broadcasts and return as soon as the packet is
    /// queued; a DM resolves to a [`MeshDelivery`] describing how far it
    /// actually got. Note that "sent" here can mean "handed to a courier who
    /// may hand it over hours from now" — further from "delivered" than
    /// anywhere else in the app.
    #[cfg(feature = "mesh")]
    pub async fn mesh_send(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
        text: &str,
    ) -> Result<crate::services::mesh::MeshDelivery, crate::services::mesh::MeshError> {
        use crate::services::mesh::{MeshConversation, MeshDelivery};
        match conversation {
            MeshConversation::Room => {
                self.mesh().send_room(text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::Geohash(cell) => {
                self.mesh().send_geohash(cell, text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::Dm(peer) => self.mesh().send_dm(peer, text).await,
        }
    }

    /// Put a file on the air for the mesh room.
    ///
    /// Room only, and there is no conversation argument for that reason: a mesh
    /// file transfer is a cleartext broadcast, while this client's mesh DMs are
    /// Noise-encrypted, so there is no directed form to send and the receiver
    /// refuses one. Returns the wire message ID; the row arrives on the mesh
    /// event stream like any other message, carrying the file as an attachment.
    ///
    /// Anything over the mesh's one-packet budget is chunked and streamed
    /// rather than truncated; past `MAX_STREAMED_FILE_BYTES` it is refused.
    #[cfg(feature = "mesh")]
    pub fn mesh_send_file(
        &self,
        path: &std::path::Path,
    ) -> Result<String, crate::services::mesh::MeshError> {
        self.mesh().send_file(path)
    }
    crate::service_accessor!(local_ai, LocalAIService, local_ai_service);
    crate::service_accessor!(
        http_local_ai,
        crate::services::local_ai_service::HttpLocalAIService,
        http_local_ai_service
    );
    crate::service_accessor!(dm_bootstrap, DmBootstrapService, dm_bootstrap_service);
    crate::service_accessor!(
        notifications,
        crate::services::notification_service::NotificationService,
        notification_service
    );
    crate::service_accessor!(sections, SectionService, section_service);
    crate::service_accessor!(
        conversation_pins,
        crate::services::conversation_pins::ConversationPins,
        conversation_pins
    );
    crate::service_accessor!(
        devices,
        crate::services::device_service::DeviceService,
        device_service
    );
    /// Get the SVR backup service (lazy-initialized, persistent instance).
    pub fn svr_backup(&self) -> &crate::services::svr_backup_service::SvrBackupService {
        self.svr_backup_service.get_or_init(|| {
            let settings = Arc::new(
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    self.services.database.clone(),
                ),
            );
            crate::services::svr_backup_service::SvrBackupService::new(
                self.services.grpc_pool.clone(),
                settings,
                self.signal().clone(),
            )
        })
    }

    /// Get the message-history backup service (lazy-initialized, persistent).
    pub fn message_backup(&self) -> &crate::services::message_backup_service::MessageBackupService {
        self.message_backup_service.get_or_init(|| {
            let settings = Arc::new(
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    self.services.database.clone(),
                ),
            );
            crate::services::message_backup_service::MessageBackupService::new(
                self.services.database.clone(),
                settings,
                self.services.grpc_pool.clone(),
            )
        })
    }

    // ── Message-history backup façade (UI entry points) ─────────────

    /// Export an encrypted message-history backup to `path` (local mode,
    /// works offline). Passphrase must clear the SVR PIN entropy floor.
    pub async fn export_message_backup(
        &self,
        path: &std::path::Path,
        passphrase: &str,
    ) -> Result<crate::services::message_backup_service::BackupStats, ClientError> {
        self.message_backup()
            .export_to_file(path, passphrase)
            .await
            .map_err(|e| ClientError::Validation(format!("backup export failed: {e}")))
    }

    /// Restore message history from an encrypted backup file. Idempotent
    /// (skip-if-exists) — never clobbers newer local rows.
    pub async fn import_message_backup(
        &self,
        path: &std::path::Path,
        passphrase: &str,
    ) -> Result<crate::storage::backup_data::ImportSummary, ClientError> {
        self.message_backup()
            .import_from_file(path, passphrase)
            .await
            .map_err(|e| ClientError::Validation(format!("backup restore failed: {e}")))
    }

    /// Create/refresh the server-side encrypted message backup (PIN-keyed,
    /// server never sees content).
    pub async fn enable_server_message_backup(
        &self,
        pin: &str,
    ) -> Result<crate::services::message_backup_service::BackupStats, ClientError> {
        self.message_backup()
            .upload_to_server(pin)
            .await
            .map_err(|e| ClientError::Validation(format!("server backup failed: {e}")))
    }

    /// Restore message history from the server-side backup (fresh install
    /// after login + PIN).
    pub async fn restore_server_message_backup(
        &self,
        pin: &str,
    ) -> Result<crate::storage::backup_data::ImportSummary, ClientError> {
        self.message_backup()
            .restore_from_server(pin)
            .await
            .map_err(|e| ClientError::Validation(format!("server restore failed: {e}")))
    }

    /// Get the call signaling service (persistent instance for pending state).
    pub fn call_signaling(&self) -> &crate::services::call_signaling::CallSignalingService {
        self.call_signaling.get_or_init(|| {
            crate::services::call_signaling::CallSignalingService::new(
                self.messages().clone(),
                self.channels().clone(),
                self.anon_dms().clone(),
                self.anon_groups().clone(),
            )
        })
    }


    // ── Safety number verification ────────────────────────────────────

    /// Get the safety number for a peer (60-digit numeric fingerprint).
    ///
    /// Both users see the same number when their identity keys match.
    /// Returns a formatted string with 12 groups of 5 digits.
    pub async fn get_safety_number(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<String, ClientError> {
        let local_user_id = self.services.auth_state.get().current_user_id
            .as_ref()
            .ok_or_else(|| ClientError::Validation("Not logged in".into()))?
            .to_string();

        let safety_number = self.services.signal_service
            .generate_safety_number(&local_user_id, &peer_user_id.to_string())
            .await
            .map_err(|e| ClientError::Validation(e.to_string()))?;

        Ok(safety_number)
    }

    /// Check if a peer has been marked as verified.
    pub fn is_peer_verified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<bool, ClientError> {
        self.services.signal_service
            .is_peer_verified(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// Mark a peer as verified with their current safety number.
    pub async fn mark_peer_verified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        let safety_number = self.get_safety_number(peer_user_id).await?;
        self.services.signal_service
            .set_peer_verified(&peer_user_id.to_string(), &safety_number, true)
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// Mark a peer as unverified.
    pub fn mark_peer_unverified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        // Get existing safety number to preserve it
        if let Ok(Some((safety_number, _))) = self.services.signal_service.key_store
            .get_peer_verification(&peer_user_id.to_string())
        {
            self.services.signal_service
                .set_peer_verified(&peer_user_id.to_string(), &safety_number, false)
                .map_err(|e| ClientError::Validation(e.to_string()))?;
        }
        Ok(())
    }

    /// Whether this peer has an identity change waiting on the user's decision,
    /// and whether the peer was cryptographically attributed when it surfaced.
    ///
    /// `Some(attributed)` means the conversation is wedged in both directions.
    /// `attributed == false` means the peer id came from server-supplied
    /// envelope metadata and nothing proved it — the UI MUST NOT present that
    /// as "X's safety number changed" (see
    /// [`crate::error::IdentityChangeSource`]).
    ///
    /// The key itself deliberately does not leave core. The UI has no use for
    /// it, and keeping it here is what lets `accept_peer_identity_change` below
    /// take no key argument at all.
    pub fn pending_identity_change(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<Option<bool>, ClientError> {
        self.services
            .signal_service
            .pending_identity_change(&peer_user_id.to_string())
            .map(|opt| opt.map(|(_key, source)| source.is_authenticated()))
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// The prompt to put in front of the user for a parked identity change, or
    /// `None` when the conversation is fine.
    ///
    /// The wording lives here, not in `chatter-ui`, because the choice it makes
    /// is a security claim and not a style one: whether the peer may be NAMED.
    /// The display name is resolved here too, and only READ when the change was
    /// cryptographically attributed — an unattributed one stays anonymous,
    /// because the only thing that tied that name to this key was the server.
    pub fn identity_change_prompt(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<Option<IdentityChangePrompt>, ClientError> {
        Ok(self.pending_identity_change(peer_user_id)?.map(|attributed| {
            IdentityChangePrompt::new(attributed, &self.display_name(peer_user_id))
        }))
    }

    /// Accept the peer's new identity key, unwedging the conversation in both
    /// directions. Returns the number of stale sessions dropped.
    ///
    /// **A trust decision. Only ever call this from an explicit user action** —
    /// a reinstall and an active MITM are indistinguishable from here.
    ///
    /// Takes no key: it reads the parked one, so the UI cannot accept a key the
    /// user was never shown, and a server that swaps a fresh key in behind the
    /// prompt still gets refused by
    /// [`crate::services::SignalService::accept_peer_identity_change`]'s own
    /// match check. Errors with `Validation` when nothing is pending, because
    /// an accept with no prompt behind it is a bug, not a no-op.
    pub fn accept_peer_identity_change(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<u32, ClientError> {
        let (new_identity_key, _source) = self
            .services
            .signal_service
            .pending_identity_change(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))?
            .ok_or_else(|| {
                ClientError::Validation("no identity change is pending for this peer".into())
            })?;

        self.services
            .signal_service
            .accept_peer_identity_change(
                peer_user_id,
                &new_identity_key,
                &self.services.channel_repo,
            )
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    // ── Events ──────────────────────────────────────────────────────────

    pub fn event_bus(&self) -> &EventBus { &self.services.event_bus }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<DomainEvent> {
        self.services.event_bus.subscribe_domain()
    }

    // ── Typed event streams (UI-facing, narrow) ─────────────────────────
    //
    // UI crates must not call `subscribe_domain()` directly. These helpers
    // filter the raw DomainEvent bus into focused payload types from
    // [`crate::client::events`] so each UI site only sees the variants it
    // cares about. Each call spawns a dedicated fan-out task tied to the
    // subscription's lifetime (tasks exit when the receiver is dropped and
    // the next filter attempt fails to publish).

    /// Subscribe to anonymous DM request / acceptance / init events.
    ///
    /// Filters `DomainEvent::AnonDmRequestReceived`, `AnonDmRequestAccepted`,
    /// and `AnonDmInitReceived` into the typed [`events::AnonDmRequestEvent`].
    pub fn subscribe_anon_dm_requests(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::AnonDmRequestEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::AnonDmRequestEvent>(32);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::AnonDmRequestReceived {
                        conversation_id,
                        sender_id,
                        sender_mailbox_pk,
                        intro_message,
                        profile_key,
                        sender_display_name,
                        sender_avatar,
                    }) => {
                        if tx.send(events::AnonDmRequestEvent::Received {
                            conversation_id,
                            sender_id,
                            sender_mailbox_pk,
                            intro_message,
                            profile_key,
                            sender_display_name,
                            sender_avatar,
                        }).is_err() {
                            break;
                        }
                    }
                    Ok(DomainEvent::AnonDmRequestAccepted {
                        conversation_id,
                        peer_id,
                        peer_mailbox_pk,
                        profile_key,
                        peer_display_name,
                        peer_avatar,
                    }) => {
                        if tx.send(events::AnonDmRequestEvent::Accepted {
                            conversation_id,
                            peer_id,
                            peer_mailbox_pk,
                            profile_key,
                            peer_display_name,
                            peer_avatar,
                        }).is_err() {
                            break;
                        }
                    }
                    Ok(DomainEvent::AnonDmInitReceived {
                        conversation_id,
                        sender_id,
                        sender_display_name,
                        sender_avatar,
                        peer_mailbox_pk,
                        peer_identity_key,
                        peer_delivery_token,
                        intro_message,
                        server_timestamp,
                    }) => {
                        if tx.send(events::AnonDmRequestEvent::Init {
                            conversation_id,
                            sender_id,
                            sender_display_name,
                            sender_avatar,
                            peer_mailbox_pk,
                            peer_identity_key,
                            peer_delivery_token,
                            intro_message,
                            server_timestamp,
                        }).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_anon_dm_requests lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to anonymous group invite events.
    pub fn subscribe_anon_group_invites(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::AnonGroupInviteEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::AnonGroupInviteEvent>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::AnonGroupInviteReceived {
                        group_id,
                        group_name,
                        inviter_id,
                        inviter_mailbox_pk,
                        profile_key,
                        inviter_display_name,
                        inviter_avatar,
                    }) => {
                        if tx.send(events::AnonGroupInviteEvent {
                            group_id,
                            group_name,
                            inviter_id,
                            inviter_mailbox_pk,
                            profile_key,
                            inviter_display_name,
                            inviter_avatar,
                        }).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_anon_group_invites lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to call key exchange events (PFS answer + group key distribution).
    pub fn subscribe_call_key_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::CallKeyEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::CallKeyEvent>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::CallKeyAnswerReceived {
                        call_id,
                        peer_id,
                        ephemeral_public_key,
                    }) => {
                        if tx.send(events::CallKeyEvent::AnswerReceived {
                            call_id,
                            peer_id,
                            ephemeral_public_key,
                        }).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_call_key_events lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to remote call hangup events (peer ended the call).
    pub fn subscribe_remote_call_hangups(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::RemoteCallHangup> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::RemoteCallHangup>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::CallHangupReceived { call_id, peer_id, reason }) => {
                        if tx.send(events::RemoteCallHangup { call_id, peer_id, reason }).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_remote_call_hangups lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Record a keystroke in `conversation`. The core state machine decides
    /// whether this fires a new STARTED, resets the pause timer, or is
    /// throttled. UI callers must NOT manage typing timers themselves.
    pub fn typing_bump(self: &Arc<Self>, conversation: crate::state::ActiveConversation) {
        // Skip typing-to-self (DM mirrors are irrelevant signal noise).
        if let crate::state::ActiveConversation::ServerDm { peer_id } = &conversation {
            if let Some(user_id) = self.services.auth_state.get().current_user_id {
                if peer_id == &user_id {
                    return;
                }
            }
        }
        if matches!(conversation, crate::state::ActiveConversation::None) {
            return;
        }
        let handle = self.conversation(conversation);
        self.outgoing_typing.bump(handle);
    }

    /// User stopped typing explicitly (compose cleared, message sent, ESC).
    /// Sends STOPPED and cancels timers if typing was active.
    pub fn typing_clear(self: &Arc<Self>) {
        let client = Arc::clone(self);
        self.outgoing_typing.clear(move |conversation| {
            Some(client.conversation(conversation.clone()))
        });
    }

    /// Compute the local Signal identity fingerprint (displayed in settings /
    /// safety-number flows).
    pub async fn my_identity_fingerprint(
        &self,
    ) -> Result<String, crate::error::ServiceError> {
        self.signal().get_identity_fingerprint().await
    }

    /// Get the current user's display name from settings (same one used for
    /// outgoing messages / calls). Falls back to an empty option.
    pub fn my_display_name(&self) -> Option<String> {
        self.anon_dms().get_my_display_name()
    }

    /// Look up an anon DM conversation by id.
    pub fn get_anon_dm_conversation(
        &self,
        id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().get_conversation(id).ok().flatten()
    }

    /// Resolve the peer user id of a regular DM channel (2-member channel).
    pub fn get_dm_peer_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Option<crate::domain::ids::UserId> {
        self.channels().get_dm_peer_local(channel_id).ok().flatten()
    }

    // ── Local AI settings façade ─────────────────────────────────────────

    pub fn set_local_ai_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai().set_enabled(enabled)
    }

    // ── Cloud AI silo façade ─────────────────────────────────────────────
    //
    // Thin delegation: every decision (policy clamp, availability, key
    // handling) lives in chatter-core services, per the architecture rule.

    /// Current cloud-silo configuration.
    pub fn cloud_ai_settings(&self) -> crate::cloud_ai::CloudSettings {
        self.http_local_ai().cloud_settings()
    }

    /// Choose between the user's own API key and the metered server broker.
    pub fn set_cloud_ai_mode(
        &self,
        mode: crate::cloud_ai::CloudMode,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_cloud_mode(mode)
    }

    /// Store (or clear, when blank) the user's own provider API key.
    pub fn set_cloud_ai_api_key(&self, key: &str) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_anthropic_api_key(key)
    }

    /// Which silo the user prefers where policy allows a choice.
    pub fn ai_silo_preference(&self) -> crate::ai_policy::channel_ai::SiloPreference {
        self.http_local_ai().silo_preference()
    }

    pub fn set_ai_silo_preference(
        &self,
        preference: crate::ai_policy::channel_ai::SiloPreference,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_silo_preference(preference)
    }

    /// Is AI switched on for this conversation? Off unless explicitly enabled.
    pub fn ai_enabled_for(&self, conversation_id: &str) -> bool {
        self.http_local_ai().ai_enabled_for(conversation_id)
    }

    pub fn set_ai_enabled_for(
        &self,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_ai_enabled_for(conversation_id, enabled)
    }

    /// What would actually run for this conversation, after the mesh/DM policy
    /// clamp. The UI uses this to explain *why* a channel shows local-only.
    pub fn resolve_ai_for(
        &self,
        conversation_id: &str,
        conversation: &crate::domain::message::ConversationType,
        delivered_via_mesh: bool,
    ) -> crate::ai_policy::channel_ai::Resolved {
        self.http_local_ai()
            .resolve_ai_for(conversation_id, conversation, delivered_via_mesh)
    }

    /// Token balance and lifetime spend on the metered broker.
    ///
    /// Only meaningful in `CloudMode::ServerBroker`; in bring-your-own-key
    /// mode the user is billed by the provider directly and we have no view of
    /// it.
    pub async fn cloud_ai_quota(&self) -> Result<(i64, i64), crate::error::NetworkError> {
        let quota = self.services.grpc_pool.ai_client().get_quota().await?;
        Ok((quota.balance, quota.spent_total))
    }

    pub fn set_local_ai_preferred_model(
        &self,
        model_id: &str,
        quantized_file: Option<&str>,
    ) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai().set_preferred_model(model_id, quantized_file)
    }

    pub async fn load_local_ai_model(
        &self,
        model_id: String,
        quantized_file: Option<String>,
    ) -> Result<crate::services::ai::ModelInfo, crate::services::ai::LocalAIError> {
        self.local_ai().load_model(model_id, quantized_file).await
    }

    pub async fn unload_local_ai_model(&self) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai().unload_model().await
    }

    /// Persisted manual presence mode ("auto" | "online" | "away" | "dnd" |
    /// "invisible"). `None` = never set, treat as "auto".
    pub fn presence_mode(
        &self,
    ) -> Result<Option<String>, crate::services::session_service::SessionError> {
        self.sessions()
            .get_string_setting(crate::storage::repositories::settings_repo::keys::PRESENCE_MODE)
    }

    /// Persist the manual presence mode chosen in the status picker.
    pub fn set_presence_mode(
        &self,
        mode: &str,
    ) -> Result<(), crate::services::session_service::SessionError> {
        self.sessions()
            .set_string_setting(crate::storage::repositories::settings_repo::keys::PRESENCE_MODE, mode)
    }

    /// Read the sealed-sender RECEIVING policy from the SERVER
    /// (users.unrestricted_unidentified_access). This is a receiving policy:
    /// `false` (default) = contacts-only (only holders of your UAK may reach
    /// you sealed), `true` = anyone may. Sealed *sending* is always on and is
    /// NOT affected by this. The value is also cached locally for offline
    /// display.
    pub async fn get_sealed_sender_policy(
        &self,
    ) -> Result<bool, crate::services::auth_service::AuthError> {
        let allow = self.auth().get_sealed_sender_policy().await?;
        // Best-effort local cache for display; failure to cache is non-fatal.
        let _ = self
            .sessions()
            .set_bool_setting("privacy.sealed_sender_allow_from_anyone", allow);
        Ok(allow)
    }

    /// Set the sealed-sender RECEIVING policy on the SERVER. Unlike a local
    /// bool this changes what the server DOES on inbound sealed messages: with
    /// `false` a non-contact's mismatched UAK is rejected; with `true` it is
    /// accepted. Returns the value the server persisted.
    pub async fn set_sealed_sender_policy(
        &self,
        allow_from_anyone: bool,
    ) -> Result<bool, crate::services::auth_service::AuthError> {
        let persisted = self
            .auth()
            .set_sealed_sender_policy(allow_from_anyone)
            .await?;
        let _ = self
            .sessions()
            .set_bool_setting("privacy.sealed_sender_allow_from_anyone", persisted);
        Ok(persisted)
    }

    /// Read "Who can find me by my username" from the SERVER
    /// (user_profiles.username_discoverable): `true` = Everyone, `false` =
    /// Nobody. The server enforces it in LookupUsername, so it holds on every
    /// device.
    pub async fn get_username_discoverability(
        &self,
    ) -> Result<bool, crate::services::user::UserError> {
        self.users().get_username_discoverability().await
    }

    /// Set "Who can find me by my username" on the SERVER. With `false`,
    /// everyone else's username lookup for this account answers "no match";
    /// friend codes and username links still reach it. Returns the value the
    /// server persisted.
    pub async fn set_username_discoverability(
        &self,
        discoverable: bool,
    ) -> Result<bool, crate::services::user::UserError> {
        self.users().set_username_discoverability(discoverable).await
    }

    /// Session-level current user id (None when logged out).
    pub fn session_current_user_id(&self) -> Option<crate::domain::ids::UserId> {
        self.sessions().current_user_id()
    }

    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<crate::services::session_service::Session>, crate::services::session_service::SessionError> {
        self.sessions().list_sessions().await
    }

    /// Revoke a session AND mark sender-key rotation for every owned channel,
    /// so the revoked device's cached sender keys cannot read future messages
    /// (rotation happens lazily on next send per channel).
    pub async fn revoke_session(
        &self,
        device_id: u32,
    ) -> Result<(), crate::services::session_service::SessionError> {
        let mut auth_client = self.services.grpc_pool.auth_client();
        self.devices()
            .revoke_and_rotate(&mut auth_client, device_id)
            .await
            .map_err(crate::services::session_service::SessionError::Device)
    }

    /// Revoke every session except the current one, with sender-key rotation
    /// per revoked device. Primary-device revokes are rejected server-side
    /// (`CannotRevokePrimary`) and skipped with a warning.
    pub async fn revoke_all_other_sessions(
        &self,
    ) -> Result<(), crate::services::session_service::SessionError> {
        let sessions = self.list_sessions().await?;
        for session in sessions {
            if session.is_current {
                continue;
            }
            if let Err(e) = self.revoke_session(session.device_id).await {
                tracing::warn!(device_id = session.device_id, error = %e, "failed to revoke session");
            }
        }
        Ok(())
    }

    // ── Sidebar sections (custom grouping) façade ────────────────────────

    /// Custom folders with their rows, already ordered by each folder's sort
    /// mode — minus anything the current mode hides.
    ///
    /// A folder is not a way around Anon Mode. The loose lists hide identified
    /// channels and friend DMs while the shield is on, and hide anon DMs while
    /// it is off; a conversation filed into a folder kept rendering — and
    /// stayed clickable — in the wrong mode.
    pub fn list_sections_with_conversations(
        &self,
    ) -> Result<Vec<crate::domain::section::CustomSection>, crate::services::section_service::SectionError>
    {
        let mut sections = self.sections().list_sections_with_conversations()?;
        for section in sections.iter_mut() {
            section.conversations.retain(|c| {
                self.visible_in_mode(&c.conversation_type)
                    && !self.is_blocked_conversation(&c.conversation_type, &c.conversation_id)
            });
            // The aggregate badge counts what the folder actually shows.
            section.unread_count = section.conversations.iter().map(|c| c.unread_count).sum();
        }
        Ok(sections)
    }

    /// Whether a conversation of this kind may be shown (listed, searched,
    /// opened) in the current mode. Shield on: anon DMs, never channels;
    /// off: never anon DMs. Friend DMs show in both — one conversation whose
    /// transport follows the mode (`send_friend_dm_content`). Anon groups and
    /// mesh are serverless and show in both.
    pub fn visible_in_mode(&self, kind: &crate::domain::message::ConversationType) -> bool {
        use crate::domain::message::ConversationType;
        let anon = self.anon_mode_enabled();
        match kind {
            ConversationType::AnonDm => anon,
            ConversationType::Channel | ConversationType::SavedMessages => !anon,
            ConversationType::ServerDm
            | ConversationType::AnonGroup
            | ConversationType::Mesh
            | ConversationType::MeshDm => true,
        }
    }

    pub fn create_section(
        &self,
        name: &str,
        emoji: &str,
    ) -> Result<crate::domain::section::CustomSection, crate::services::section_service::SectionError> {
        self.sections().create_section(name, emoji)
    }

    pub fn delete_section(
        &self,
        id: &crate::domain::ids::SectionId,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().delete_section(id)
    }

    pub fn rename_section(
        &self,
        id: &crate::domain::ids::SectionId,
        new_name: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().rename_section(id, new_name)
    }

    pub fn change_section_icon(
        &self,
        id: &crate::domain::ids::SectionId,
        new_emoji: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().change_icon(id, new_emoji)
    }

    pub fn toggle_section_expanded(
        &self,
        id: &crate::domain::ids::SectionId,
        expanded: bool,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().toggle_expanded(id, expanded)
    }

    pub fn reorder_sections(
        &self,
        ids: &[crate::domain::ids::SectionId],
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().reorder_sections(ids)
    }

    /// Move a section one place up (`delta` -1) or down (`delta` +1).
    pub fn move_section(
        &self,
        id: &crate::domain::ids::SectionId,
        delta: i32,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().move_section(id, delta)
    }

    /// Set how a section orders the conversations inside it.
    pub fn set_section_sort_mode(
        &self,
        id: &crate::domain::ids::SectionId,
        mode: crate::domain::section::SectionSortMode,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().set_sort_mode(id, mode)
    }

    // ── Conversation pins (local-only) ───────────────────────────────────

    /// Pin or unpin a conversation by its sidebar id.
    pub fn set_conversation_pinned(&self, conversation_id: &str, pinned: bool) -> bool {
        match self.conversation_pins().set_pinned(conversation_id, pinned) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(error = %e, conversation_id, pinned, "failed to persist conversation pin");
                false
            }
        }
    }

    /// Whether a conversation is pinned.
    pub fn is_conversation_pinned(&self, conversation_id: &str) -> bool {
        self.conversation_pins().is_pinned(conversation_id)
    }

    /// Every pinned conversation id, in one query.
    pub fn pinned_conversation_ids(&self) -> std::collections::HashSet<String> {
        self.conversation_pins().pinned_ids()
    }

    pub fn move_conversation_to_section(
        &self,
        conv_id: &str,
        conv_type: crate::domain::message::ConversationType,
        section_id: &crate::domain::ids::SectionId,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections()
            .move_conversation_to_section(conv_id, conv_type, section_id)
    }

    pub fn remove_conversation_from_section(
        &self,
        conv_id: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().remove_conversation_from_section(conv_id)
    }

    /// The name a conversation is shown under — the chat list's name, so a
    /// search hit and its sidebar row agree. A friend DM is channel-free
    /// (keyed by the peer, no `channels` row), so it resolves through the
    /// peer's display name; everything else through its stored metadata.
    pub fn conversation_display_name(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> String {
        use crate::domain::message::ConversationType;
        match conversation_type {
            ConversationType::ServerDm => {
                self.display_name(&crate::domain::ids::UserId::from_uuid(*conversation_id.as_uuid()))
            }
            // The stored name of the note-to-self channel is its internal
            // `saved_messages_<uuid>` id, not something to show a person.
            ConversationType::SavedMessages => "Saved Messages".to_string(),
            _ => self
                .sections()
                .get_conversation_metadata(&conversation_id.to_string(), conversation_type)
                .ok()
                .flatten()
                .map(|c| c.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Unknown".to_string()),
        }
    }

    /// Preview text for a pinned message, resolved per conversation flavor
    /// (anon groups read the unified anon store; everything else the pin
    /// service). Falls back to `[Pinned message]`.
    pub fn pinned_message_preview(
        &self,
        conversation: &crate::state::ActiveConversation,
        msg_id: &crate::domain::ids::MessageId,
    ) -> String {
        use crate::state::ActiveConversation;
        match conversation {
            ActiveConversation::AnonGroup(_) => {
                let anon_id = crate::domain::anon_group::AnonGroupMessageId::from_string(
                    msg_id.to_string(),
                );
                self.get_anon_group_message(&anon_id)
                    .and_then(|m| {
                        m.as_text()
                            .filter(|t| !t.is_empty())
                            .map(|t| self.truncate_message_preview(&t))
                    })
                    .unwrap_or_else(|| "[Pinned message]".to_string())
            }
            _ => self
                .get_pinned_message(msg_id)
                .map(|d| d.preview)
                .unwrap_or_else(|| "[Pinned message]".to_string()),
        }
    }

    /// Channels to list in the sidebar: pinned first, then MRU; only
    /// Public/Private; anything filed into a custom section is excluded (it
    /// renders inside that folder instead — it used to render in BOTH); and
    /// empty while anon mode hides regular channels. Single source of the
    /// sidebar-inclusion rule.
    pub fn sidebar_channels(&self) -> Vec<crate::domain::channel::Channel> {
        if self.anon_mode_enabled() {
            return Vec::new();
        }
        use crate::domain::channel::ChannelType;
        // A channel that lives in a folder is not also a loose channel.
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        let mut channels: Vec<_> = self
            .get_channels_mru()
            .into_iter()
            .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
            .filter(|c| !sectioned.contains(&c.id.to_string()))
            .collect();
        // Pinned to the top, MRU preserved within each group.
        self.conversation_pins()
            .sort_pinned_first(&mut channels, |c| c.id.to_string());
        let channels = channels;

        // The local AI channel is pinned to the top when local AI is on.
        #[cfg(feature = "jarvis")]
        {
            let mut channels = channels;
            if self.jarvis_channel_enabled() {
                channels.insert(0, self.jarvis_virtual_channel());
            }
            return channels;
        }
        #[cfg(not(feature = "jarvis"))]
        channels
    }

    /// The channel to select on load: first non-DM, non-SavedMessages channel
    /// in MRU order.
    pub fn first_selectable_channel_id(&self) -> Option<crate::domain::ids::ChannelId> {
        use crate::domain::channel::ChannelType;
        self.get_channels_mru()
            .iter()
            .filter(|c| !c.id.0.is_nil()) // never auto-select the local AI channel
            .find(|c| !matches!(c.channel_type, ChannelType::DirectMessage | ChannelType::SavedMessages))
            .map(|c| c.id)
    }

    // ── Auth façade ──────────────────────────────────────────────────────

    pub fn endpoint(&self) -> String {
        self.auth().endpoint()
    }

    pub fn set_server_url(&self, url: &str) -> Result<(), crate::error::NetworkError> {
        self.auth().set_server_url(url)
    }

    pub fn is_logged_in(&self) -> bool {
        self.auth().is_logged_in()
    }

    pub fn has_registered_before(&self) -> bool {
        self.auth().has_registered_before()
    }

    pub fn current_user(&self) -> Option<(crate::domain::ids::UserId, u32)> {
        self.auth().current_user()
    }

    pub fn restore_session(&self) -> bool {
        self.auth().restore_session()
    }

    pub async fn cleanup_failed_registration(
        &self,
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().cleanup_failed_registration().await
    }

    /// Local logout (best-effort server notify + clear local state).
    // ── State stores ────────────────────────────────────────────────────

    pub fn auth_state(&self) -> &AuthState { &self.services.auth_state }
    pub fn chat_state(&self) -> &ChatState { &self.services.chat_state }

    /// Resolve the current active conversation's `ConversationType`,
    /// looking up the channel row when the active conversation is a
    /// `Channel(id)` (the vault and DMs share that variant). UI
    /// callers use this rather than
    /// `ActiveConversation::conversation_type(...)` directly so they
    /// don't need channel_repo access.
    pub fn active_conversation_type(&self) -> Option<crate::domain::message::ConversationType> {
        self.conversation_type_of(&self.services.chat_state.get().active_conversation)
    }

    /// Same resolution for a conversation that isn't the active one — a handle
    /// can be built for any conversation, and `channel_repo` is private to this
    /// module.
    pub fn conversation_type_of(
        &self,
        conversation: &crate::state::ActiveConversation,
    ) -> Option<crate::domain::message::ConversationType> {
        conversation.conversation_type(&self.services.channel_repo)
    }

    /// Shortcut for `chat_state().active_conversation()`. The typed
    /// `ActiveConversation` is the single source of truth for "which
    /// conversation is currently being viewed" — use this (or the
    /// `matches_*` predicates on it) instead of string-comparing UI state.
    pub fn active_conversation(&self) -> crate::state::ActiveConversation {
        self.services.chat_state.active_conversation()
    }
    pub fn message_state(&self) -> &MessageState { &self.services.message_state }
    pub fn user_state(&self) -> &UserState { &self.services.user_state }

    /// Persistent sidebar collapsed flag (settings-backed). Defaults to false.
    pub fn sidebar_collapsed(&self) -> bool {
        use crate::storage::repositories::settings_repo::SettingsRepository;
        use crate::storage::settings_keys;
        let repo = SettingsRepository::new(self.services.database.clone());
        repo.get::<bool>(settings_keys::SIDEBAR_COLLAPSED)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Persist sidebar collapsed flag.
    pub fn set_sidebar_collapsed(&self, collapsed: bool) {
        use crate::storage::repositories::settings_repo::SettingsRepository;
        use crate::storage::settings_keys;
        let repo = SettingsRepository::new(self.services.database.clone());
        let _ = repo.set(settings_keys::SIDEBAR_COLLAPSED, &collapsed);
    }

    /// Read the user's search-shortcut preference (defaults to `Both`).
    pub fn get_search_shortcut(&self) -> crate::services::search::SearchShortcut {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        let repo = SettingsRepository::new(self.services.database.clone());
        let raw = repo
            .get_string(keys::SEARCH_SHORTCUT)
            .ok()
            .flatten()
            .unwrap_or_default();
        crate::services::search::SearchShortcut::from_str(&raw)
    }

    /// The current account's full Signal-style `nickname.discriminator` handle,
    /// chosen at registration and persisted under `keys::FULL_USERNAME`. This is
    /// the exact form `lookup_by_username` expects. `None` before registration.
    pub fn full_username(&self) -> Option<String> {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        SettingsRepository::new(self.services.database.clone())
            .get_string(keys::FULL_USERNAME)
            .ok()
            .flatten()
    }

    /// Persist the user's search-shortcut preference.
    pub fn set_search_shortcut(
        &self,
        shortcut: crate::services::search::SearchShortcut,
    ) -> Result<(), crate::error::StorageError> {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        let repo = SettingsRepository::new(self.services.database.clone());
        repo.set_string(keys::SEARCH_SHORTCUT, shortcut.as_str())
    }

    // ── Anon Mode (in-memory UI filter) ────────────────────────────────
    //
    // Shield flag: when true the UI hides channels and Saved Messages
    // (`visible_in_mode`), friend DMs go through the mailbox
    // (`send_friend_dm_content`), and the transport switches to onion. Not
    // persisted by design —
    // every fresh client starts with anon_mode = false so privacy
    // properties auto-revert on restart.

    /// Platform lifecycle report: is the app on screen (window focused /
    /// activity resumed)? While it is not, nothing is marked read or
    /// receipted — an open chat behind a lock screen has not been read.
    ///
    /// Coming back into view catches up: the open conversation, now actually
    /// on screen, is marked read (and receipted, subject to the user's
    /// receipt setting). Call from within the tokio runtime.
    pub fn set_app_visible(self: &Arc<Self>, visible: bool) {
        let was_visible = self.chat_state().set_app_visible(visible);
        if !visible || was_visible {
            return;
        }
        let active = self.chat_state().active_conversation();
        if !active.is_active() {
            return;
        }
        let client = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = client.conversation(active).mark_as_read().await {
                tracing::debug!(error = %e, "mark-read on return to foreground failed");
            }
        });
    }

    /// Read the in-memory anon-mode flag.
    pub fn anon_mode_enabled(&self) -> bool {
        self.anon_mode.load(Ordering::Acquire)
    }

    /// Flip the in-memory anon-mode flag. Callers (the UI handler) are
    /// responsible for re-running sidebar refresh helpers after flip;
    /// this method does not emit a domain event because the flag is a
    /// presentation concern, not a chatter-core state change.
    pub fn set_anon_mode_enabled(self: &Arc<Self>, enabled: bool) {
        self.anon_mode.store(enabled, Ordering::Release);

        // Anon mode also routes the network transport over Tor (onion): enabling
        // it switches the whole pool to the pinned .onion, disabling it switches
        // back to direct. If onion isn't available (feature not built, no .onion
        // pinned, or bootstrap fails), we log LOUDLY and keep the current
        // transport — the UI filter still applies, but we must NOT pretend the
        // network location is hidden when it isn't.
        use crate::network::grpc::client_pool::TransportMode;
        let mode = if enabled { TransportMode::Onion } else { TransportMode::Direct };
        let switch_result = self.services.grpc_pool.set_transport_mode(mode);

        // Reflect the pool's ACTUAL transport into the process-global HTTP flag
        // that leaf media modules (gif) gate on and that the
        // CDN reqwest callsites branch on. Keyed on the real transport, not the
        // request, so a failed onion switch leaves it false — no false anonymity.
        let on_onion = self.services.grpc_pool.transport_mode() == TransportMode::Onion;
        crate::network::anon_http::ANON_HTTP_ACTIVE.store(on_onion, Ordering::Release);

        // Drive the Tor UI indicator: entering onion = Connecting (until the
        // first onion RPC round-trips, when the poller flips it to Connected);
        // leaving onion = Off; Shield on but still direct = Unavailable, which
        // the UI must show as "network location NOT hidden" for as long as it
        // lasts — Shield is not allowed to look fully active.
        use crate::network::connection::TorStatus;
        crate::network::connection::set_tor_status(if on_onion {
            TorStatus::Connecting
        } else if enabled {
            TorStatus::Unavailable
        } else {
            TorStatus::Off
        });

        if let Err(e) = switch_result {
            tracing::warn!(
                requested = ?mode,
                error = %e,
                "anon mode: transport switch failed — UI filter applies but network location is NOT hidden"
            );
            // Transport unchanged: leave the long-lived loops alone. They are
            // already on the current (correct) transport, and tearing them down
            // would only churn connections for no anonymity gain.
            return;
        }

        // ANONYMITY-CRITICAL: set_transport_mode drops the pool's cached
        // channels so every *new* RPC and per-attempt retry rebuilds over the
        // new transport — but the long-lived loops opened BEFORE the flip hold
        // their own channel clones (pinned to the pre-switch transport) and
        // would keep egressing the client IP until they naturally reconnect.
        // Rebuild them now so they re-dial over the new transport. Runs on the
        // client runtime (this method is invoked from the UI thread, which has
        // no ambient runtime and must never block). Serialized via
        // `transport_switch_lock` so rapid toggles converge, not interleave.
        let this = Arc::clone(self);
        self.services.runtime_handle.spawn(async move {
            let _guard = this.transport_switch_lock.lock().await;
            this.rebuild_transport_loops().await;
        });
    }

    /// Tear down and rebuild every long-lived network loop so it re-dials over
    /// the transport that is now active. Called after a successful anon-mode
    /// transport switch. Runs inside the client runtime (ambient runtime
    /// required for the re-spawns) and under `transport_switch_lock`.
    async fn rebuild_transport_loops(self: &Arc<Self>) {
        // Read the LIVE anon state, not a captured param. Under rapid toggles
        // the `transport_switch_lock` grants in arrival order (not spawn order),
        // so a stale captured value could resurrect push against the final Onion
        // transport (SSE-over-direct leak). notif/inbox/fanout already read live
        // pool state; the push decision must converge with them to the final
        // transport, so it reads the same authoritative atomic.
        let anon = self.anon_mode.load(std::sync::atomic::Ordering::Acquire);

        // 1. gRPC notification stream — abort the in-flight stream (pinned to
        //    the old channel) and re-spawn so it connects over the new one.
        self.respawn_notification_stream();

        // 2. ntfy push SSE listener — egresses to the ntfy server over plain
        //    HTTP (no Tor). PRODUCT DECISION: push is OFF while anon mode is on;
        //    delivery falls back to mailbox polling (the primary path anyway).
        //    Restore the previously-configured push on exit.
        if anon {
            if let Some(tx) = self.push_shutdown.lock().take() {
                let _ = tx.send(());
                tracing::info!("anon mode: ntfy push suspended (poll-only delivery)");
            }
        } else if let Some(url) = self.push_url.lock().clone() {
            if let Err(e) = self.configure_push(&url) {
                tracing::warn!(error = %e, "anon mode: failed to restore ntfy push on exit");
            } else {
                tracing::info!("anon mode: ntfy push restored");
            }
        }

        // 3. Inbox poll loop — the provider captured its mailbox/message gRPC
        //    clients at construction (pinned to the pre-switch channel), so a
        //    bare stop/start would keep the old transport. Stop, rebuild those
        //    clients from the pool (now on the new transport), then restart.
        if let Some(provider) = self.ctx.get_device_inbox_provider() {
            if let Err(e) = provider.stop().await {
                tracing::warn!(error = %e, "anon mode: inbox provider stop failed during transport switch");
            }
            provider.refresh_clients(&self.services.grpc_pool).await;
            if let Err(e) = provider.start().await {
                tracing::warn!(error = %e, "anon mode: inbox provider restart failed during transport switch");
            } else {
                tracing::info!("anon mode: inbox poll loop re-dialed over new transport");
            }
        }

        // 4. Group fanout — the mailbox client is captured once at login
        //    (`init_group_fanout`) and pinned into a OnceLock, so every group
        //    send deposits through it. Without this swap the core group-message
        //    path keeps egressing the real IP after anon-enable. Rebuild it
        //    from the pool (now on the new transport).
        if let Some(fanout) = self.ctx.get_group_fanout() {
            fanout.refresh_client(&self.services.grpc_pool).await;
            tracing::info!("anon mode: group fanout client re-dialed over new transport");
        }

        // 5. Call worker — the signaling `CallClient` is captured for the
        //    worker's lifetime, so call create/join/leave/end would leak the
        //    real IP after anon-enable. Hand the worker a fresh client built on
        //    the new transport; it applies the swap between complete call
        //    operations, so no in-flight call is corrupted.
        #[cfg(feature = "media")]
        if let Some(handle) = self.ctx.get_call_service() {
            handle
                .swap_call_client(self.services.grpc_pool.call_client())
                .await;
            tracing::info!("anon mode: call worker client swap enqueued for new transport");
        }
    }

    /// PIN-gate for enabling anon mode. Wraps `LinkDeviceService::verify_pin`
    /// and squashes any error into `false` so the UI can render a single
    /// "wrong PIN / no PIN set / storage error" message without leaking
    /// distinctions a UI shouldn't surface differently.
    pub fn verify_pin_for_anon_mode(&self, pin: &str) -> bool {
        self.link_device().verify_pin(pin).unwrap_or(false)
    }

    /// Whether Shield can hide this device's network location in this build:
    /// the `onion` feature is compiled in and a server `.onion` is pinned
    /// (`config::PINNED_ONION_ADDR`). The sign-in and create-account screens
    /// offer Shield only when this holds.
    pub fn shield_available(&self) -> bool {
        self.services.grpc_pool.onion_available()
    }

    /// Choose Shield on the sign-in / create-account / recovery screens,
    /// before any account is signed in, so the whole of onboarding —
    /// registration challenge and proof of work, Register, OPAQUE login,
    /// recovery, key upload, SVR backup, avatar upload — reaches the server
    /// only over its onion service.
    ///
    /// Fails closed. Enabling returns an error and changes nothing when the
    /// build cannot route over Tor, and never leaves the client on a direct
    /// connection while claiming Shield. Once on, the pool has no direct path:
    /// if Tor cannot bootstrap, every call fails rather than going direct.
    ///
    /// The session that signs in stays in Shield (`anon_mode_enabled()`), the
    /// same state the chat-list toggle produces. Like that toggle it is
    /// in-memory only: the next app start is direct again (owner decision).
    ///
    /// Refused once signed in: from there the chat-list toggle and its PIN
    /// gate own the switch.
    pub fn set_shield_before_login(self: &Arc<Self>, enabled: bool) -> Result<(), ClientError> {
        if self.is_logged_in() {
            return Err(ClientError::Validation(
                "already signed in; use the Shield toggle on the chat list".into(),
            ));
        }
        if enabled && !self.shield_available() {
            return Err(ClientError::Validation(
                "Shield can't hide your network location in this build (no Tor onion service configured)".into(),
            ));
        }
        self.set_anon_mode_enabled(enabled);

        use crate::network::grpc::client_pool::TransportMode;
        if enabled && self.services.grpc_pool.transport_mode() != TransportMode::Onion {
            // Availability was checked above, so this is an endpoint that would
            // not build. Undo rather than sign in with Shield showing and the
            // traffic going direct.
            self.set_anon_mode_enabled(false);
            return Err(ClientError::Validation(
                "couldn't switch to the Tor onion service; Shield is off".into(),
            ));
        }
        Ok(())
    }

    pub fn offline_state(&self) -> &OfflineState { &self.services.offline_state }
    pub fn privacy_state(&self) -> &PrivacyState { &self.services.privacy_state }
    #[cfg(feature = "jarvis")]
    pub fn jarvis_state(&self) -> &JarvisState { &self.services.jarvis_state }

    // ── Infrastructure ──────────────────────────────────────────────────

    pub fn runtime_handle(&self) -> &tokio::runtime::Handle { &self.services.runtime_handle }
    pub fn directories(&self) -> &Directories { &self.services.directories }

    /// Post-boot call service handle (OnceLock).
    #[cfg(feature = "media")]
    pub fn call_service(&self) -> &Arc<std::sync::OnceLock<CallWorkerHandle>> {
        &self.services.call_service
    }

    /// Post-boot group fanout (OnceLock).
    pub fn group_fanout(&self) -> &Arc<std::sync::OnceLock<Arc<GroupFanout>>> {
        &self.services.group_fanout
    }

    /// Post-boot device inbox provider (OnceLock).
    pub fn device_inbox_provider(&self) -> &Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>> {
        &self.services.device_inbox_provider
    }

    // ── Delegated CoreServices methods ─────────────────────────────────────

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.services.is_connected()
    }

    /// Start the inbox provider and return a receiver for raw inbox events.
    pub async fn start_inbox_provider(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::inbox::InboxEvent>> {
        self.services.start_inbox_provider().await
    }

    /// Pre-fetch anonymous tokens for sealed sender messaging.
    pub async fn prefetch_tokens(
        &self,
        target_count: u32,
    ) -> Result<u32, crate::error::MailboxError> {
        self.services.prefetch_tokens(target_count).await
    }

    /// Decline an incoming call.
    pub async fn decline_call(&self, call_id: &str) -> Result<(), NetworkError> {
        self.services.decline_call(call_id).await
    }

    /// Search GIFs.
    pub async fn search_gifs(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, NetworkError> {
        self.services.search_gifs(query, limit).await
    }

    /// Fetch trending GIFs.
    pub async fn trending_gifs(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, NetworkError> {
        self.services.trending_gifs(limit).await
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Connect to the server (establish gRPC channel).
    ///
    /// The gRPC pool is created at `build()` but connects lazily.
    /// This forces an immediate connection attempt to verify reachability.
    /// Optional — RPCs will auto-connect on first use if skipped.
    ///
    /// Side effect: updates `offline_state` so the rest of the SDK observes
    /// `Connected` / `Disconnected` consistently. Without this, callers
    /// would have to flip the state themselves after every `connect()` —
    /// which is what `ensure_connected_for_auth()` used to do.
    pub async fn connect(&self) -> Result<(), ClientError> {
        use crate::network::connection::ConnectionState;
        // First contact after a signed-out start: the user has acted, on the
        // transport they chose, so the reachability monitor may run now (it
        // never probes while the pool is on onion).
        if self.server_contact_deferred.swap(false, Ordering::SeqCst) {
            self.start_server_monitor();
        }
        match self.services.auth_service.connect().await {
            Ok(()) => {
                self.offline_state().set_connection_state(ConnectionState::Connected);
                Ok(())
            }
            Err(e) => {
                self.offline_state().set_connection_state(ConnectionState::Disconnected);
                Err(e.into())
            }
        }
    }

    /// Start all background services after authentication.
    ///
    /// Call AFTER login/register. Call `subscribe_events()` BEFORE this
    /// to avoid missing events.
    ///
    /// This initializes:
    /// - Post-login infrastructure (device inbox, group fanout, call service)
    /// - Event handlers (DomainEvent → state store + DB persistence)
    /// - Inbox polling + protocol processor
    /// - Channel event stream, bootstrap, DM bootstrap
    /// - AI services (Jarvis, local AI)
    /// - Presence (broadcasts online status)
    /// Multi-device crypto maintenance listener. One task that:
    ///   * `SenderKeyReceived` → replays any deferred group decrypts whose SKDM
    ///     finally arrived.
    ///   * `ChannelMemberLeft` (someone else) → marks the channel for sender-key
    ///     rotation so the next send issues a new key the ex-member can't read.
    ///     Skips our own departure — `leave_channel` already rotated + purged.
    fn spawn_crypto_maintenance_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        let mut rx = self.services.event_bus.subscribe_domain();
        let own_user_id = self.services.auth_state.get().current_user_id;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(DomainEvent::SenderKeyReceived { sender_id, sender_device_id, channel_id, .. }) => {
                        client.messages().retry_pending_group_decrypts(
                            channel_id, sender_id, sender_device_id,
                        ).await;
                    }
                    Ok(DomainEvent::ChannelMemberLeft { channel_id, user_id, .. }) => {
                        if Some(user_id) == own_user_id {
                            continue;
                        }
                        if let Err(e) = client.signal().mark_channel_for_rotation(&channel_id) {
                            tracing::warn!(
                                channel_id = %channel_id,
                                user_id = %user_id,
                                error = %e,
                                "Failed to mark channel for rotation after member left"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "Multi-device crypto listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Call-worker group-call key policy listener. In a channel-based group
    /// call the designated rotator rotates the encryption key on every
    /// membership change: on `ParticipantLeft` so the departed participant
    /// can't decrypt future frames, and on `ParticipantJoined` — joins within
    /// `ROSTER_SETTLE` coalesced into one rotation — so a joiner is sent only
    /// a key that covered nothing before it joined. A received `GroupCallKey`
    /// rings, or is installed. The "when" decisions live in core, not the UI
    /// crate.
    ///
    /// The loop only dispatches: each event's network work runs in its own
    /// task with deadlines, so a slow server or a flood of keys never holds
    /// up a `ParticipantLeft`. Received keys are handled at most
    /// [`CALL_KEY_TASKS`] at a time — the rest are dropped — and need no
    /// ordering: adoption only moves forward (`CallKeyState::adopt`). If the
    /// room-event bus still lags, the roster is re-read and any departure
    /// in the gap rotates the key.
    #[cfg(feature = "media")]
    fn spawn_call_room_policy_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        let mut domain_rx = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            // Wait for the call worker to be initialized. init_call_service runs
            // before start() returns, but on Android it is deferred.
            let (worker, mut rx) = loop {
                if let Some(w) = client.ctx.get_call_service() {
                    let rx = w.subscribe_room_events();
                    break (w.clone(), rx);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            };

            use crate::calls::LiveKitRoomEvent;
            use std::collections::HashSet;
            use tokio::sync::broadcast::error::RecvError;
            let join_gate = Arc::new(calls::JoinRotationGate::default());
            let key_slots = Arc::new(tokio::sync::Semaphore::new(CALL_KEY_TASKS));
            // Who we last knew to be in the room, to find departures a lag
            // swallowed. Refreshed from the worker on every state change
            // (connect, and each participant count change).
            let mut known_roster: HashSet<String> = HashSet::new();

            let departed = |identity: String| {
                let client = Arc::clone(&client);
                tokio::spawn(async move {
                    if let Err(e) = client.handle_participant_departed(&identity).await {
                        tracing::error!(error = %e, "Failed to handle participant departure");
                    }
                });
            };
            let joined = || {
                if !join_gate.arm() {
                    return;
                }
                let client = Arc::clone(&client);
                let gate = Arc::clone(&join_gate);
                tokio::spawn(async move {
                    tokio::time::sleep(calls::ROSTER_SETTLE).await;
                    gate.fire();
                    if let Err(e) = client.handle_participants_joined().await {
                        tracing::error!(error = %e, "Failed to rotate the group call key for joiners");
                    }
                });
            };

            loop {
                tokio::select! {
                    event = rx.recv() => match event {
                        Ok(LiveKitRoomEvent::ParticipantLeft { identity }) => {
                            known_roster.remove(&identity);
                            departed(identity);
                        }
                        Ok(LiveKitRoomEvent::ParticipantJoined { identity, .. }) => {
                            known_roster.insert(identity);
                            joined();
                        }
                        Ok(LiveKitRoomEvent::StateChanged(_)) => {
                            known_roster = worker.remote_participant_identities().await.into_iter().collect();
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "Call room-events policy lagged; re-reading the roster");
                            let now: HashSet<String> = worker.remote_participant_identities().await.into_iter().collect();
                            let (gone, new) = roster_changes(&known_roster, &now);
                            known_roster = now;
                            // One rotation locks out every departure in the gap.
                            if let Some(identity) = gone.into_iter().next() {
                                departed(identity);
                            }
                            if !new.is_empty() {
                                joined();
                            }
                        }
                        Err(RecvError::Closed) => break,
                    },
                    event = domain_rx.recv() => match event {
                        Ok(DomainEvent::GroupCallKeyReceived { call_id, sender_id, key_material, key_epoch, channel_id }) => {
                            let Ok(slot) = Arc::clone(&key_slots).try_acquire_owned() else {
                                tracing::warn!(%sender_id, "Group call key dropped: key handling saturated");
                                continue;
                            };
                            let client = Arc::clone(&client);
                            tokio::spawn(async move {
                                client.handle_group_call_key(call_id, sender_id, key_material, key_epoch, channel_id).await;
                                drop(slot);
                            });
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "Call key policy lagged on domain events");
                        }
                        Err(RecvError::Closed) => break,
                    },
                }
            }
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(ClientError::AlreadyStarted);
        }

        // Login and register set AuthState themselves; a device that was
        // LINKED reaches here with it empty, and every reader of it (the
        // own-message notification gate among them) went blind until an app
        // restart. Fill it from the persisted identity.
        if self.auth_state().get().current_user_id.is_none() {
            let sessions = self.sessions();
            if let (Some(user_id), Some(device_id)) =
                (sessions.current_user_id(), sessions.current_device_id())
            {
                let name = self.display_name(&user_id);
                self.auth_state().set_logged_in(user_id, device_id, name);
            }
        }

        // ── Infrastructure init ─────────────────────────────────────
        self.ctx.init_device_inbox_provider();
        self.ctx.init_group_fanout();
        #[cfg(feature = "media")]
        self.ctx.init_call_service();

        // ── Event handling ──────────────────────────────────────────
        let h_events = self.ctx.spawn_event_handlers();

        // ── Multi-device crypto maintenance listener ────────────────
        let h_skdm_retry = self.spawn_crypto_maintenance_listener();

        // ── Call worker room-events policy listener ─────────────────
        #[cfg(feature = "media")]
        let _h_call_room_policy = self.spawn_call_room_policy_listener();

        // ── Inbox polling + protocol processor ──────────────────────
        let inbox_rx = match self.services.start_inbox_provider().await {
            Some(rx) => rx,
            None => {
                self.started.store(false, Ordering::SeqCst);
                return Err(ClientError::Inbox("failed to start inbox".into()));
            }
        };
        let processor = crate::services::InboxEventProcessor::from_client(self);
        let mut inbox_rx = inbox_rx;
        let h_inbox = tokio::spawn(async move {
            loop {
                match inbox_rx.recv().await {
                    Ok(event) => { processor.process(&event).await; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "InboxEventProcessor lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // ── Finish an interrupted profile-key rotation ──────────────
        {
            let client = self.clone();
            tokio::spawn(async move {
                if let Err(e) = client.finish_profile_key_rotation().await {
                    tracing::warn!(error = %e, "profile key rotation still pending");
                }
            });
        }

        // ── Channel + DM services ───────────────────────────────────
        let h_channel_stream = self.ctx.start_channel_event_stream();
        // Live channel messages. This is the ONLY subscriber of the
        // server-managed channel message path: it re-subscribes on every
        // connect from the server's membership list, so channels joined in a
        // previous session start delivering again after a restart (INFRA-1),
        // and it runs a durable catch-up fetch on each connect so nothing
        // depends on the stream having been up.
        let h_channel_messages = self
            .services
            .channel_stream_service
            .clone()
            .start_channel_message_stream();
        let h_channel_bootstrap = self.ctx.start_channel_bootstrap_service();
        let h_dm_bootstrap = self.ctx.spawn_dm_bootstrap_handler();

        // ── AI services ─────────────────────────────────────────────
        #[cfg(feature = "jarvis")]
        let jarvis = crate::services::jarvis::JarvisBackgroundBuilder::new(
            self.jarvis().clone(),
        )
        .with_event_bus(self.event_bus().clone())
        .spawn();

        let h_ai = tokio::spawn({
            let local_ai = self.local_ai().clone();
            async move {
                match local_ai.load_preferred_model().await {
                    Ok(Some(info)) => tracing::info!("Auto-loaded local AI model: {}", info.model_id),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("Failed to auto-load local AI model: {}", e),
                }
            }
        });

        // ── Pre-fetch sender certificate (needed for sealed sender / anon DM) ──
        {
            let signal = self.signal().clone();
            let grpc_pool = self.services.grpc_pool.clone();
            tokio::spawn(async move {
                let mut auth = grpc_pool.auth_client();
                match signal.get_or_request_sender_certificate(&mut auth).await {
                    Ok(_) => tracing::info!("Sender certificate pre-fetched"),
                    Err(e) => tracing::warn!("Failed to pre-fetch sender certificate: {}", e),
                }
            });
        }

        // ── Multi-device sync engine ─────────────────────────────────
        // Periodic pull of channels, messages, friends and the block list.
        // The block-list pass is load-bearing for security: the inbound block
        // filter (`services/gateway/block_filter.rs`) reads the `friendships`
        // rows this writes, so without this loop a block made on another
        // device is never enforced here.
        let h_sync = self.services.sync_service.start_background_sync();

        // ── PreKey replenishment (runs initial check immediately) ────
        let h_prekey = self.services.spawn_prekey_replenishment();

        // ── DM mailbox key rolling (L4 anonymity rotation) ───────────
        let h_key_rolling = self.services.spawn_key_rolling();

        // ── Mailbox-token refill (jittered, decoupled from sends) ────
        let h_token_refill = self.services.spawn_token_refill();

        // ── Disappearing-messages local reaper ───────────────────────
        // Hard-deletes every message whose timer has run out, with the
        // attachments only it carried (`services::disappearing::reap_expired`),
        // and tells the UI. The first tick fires immediately, so a message that
        // expired while the app was closed goes on the next boot (expiry times
        // persist — v015). Interval overridable via
        // `CHATTER_EPHEMERAL_REAP_SECS`; default 10s — one indexed query
        // (idx_ephemeral_expires).
        let h_ephemeral_reaper = {
            let db = self.services.database.clone();
            let expired = self.expired_messages.clone();
            let interval_secs = std::env::var("CHATTER_EPHEMERAL_REAP_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .unwrap_or(10);
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                loop {
                    ticker.tick().await;
                    match crate::services::disappearing::reap_expired(&db) {
                        Ok(gone) if !gone.is_empty() => {
                            tracing::info!(count = gone.len(), "ephemeral reaper: removed expired local messages");
                            let _ = expired.send(gone);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "ephemeral reaper: cleanup failed"),
                    }
                }
            })
        };

        // ── Presence ────────────────────────────────────────────────
        // Fire and forget: start() is on the app's startup path, and awaiting
        // this RPC held the first screen back for as long as the server took
        // (27 s once, on a Redis-starved server).
        {
            let client = self.clone();
            tokio::spawn(async move {
                client.set_presence(crate::domain::user::PresenceStatus::Online).await;
            });
        }
        self.touch_activity();

        // ── Store handles for shutdown ───────────────────────────────
        let mut tasks = self.tasks.lock();
        tasks.extend([h_events, h_inbox, h_skdm_retry, h_channel_stream, h_channel_messages, h_channel_bootstrap, h_dm_bootstrap, h_ai, h_sync, h_prekey, h_key_rolling, h_token_refill, h_ephemeral_reaper]);
        #[cfg(feature = "jarvis")]
        {
            tasks.push(jarvis.maintenance);
            if let Some(observer) = jarvis.observer {
                tasks.push(observer);
            }
        }

        Ok(())
    }


    // ── High-level messaging ─────────────────────────────────────────

    /// Send a text message (no attachments).
    pub async fn send_text(
        &self,
        conversation: ActiveConversation,
        text: &str,
    ) -> Result<SendResult, SendError> {
        self.send_message(conversation, text, &[], None, None).await
    }

    /// Send a message with optional attachments, quote, and options.
    pub async fn send_message(
        &self,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<SendResult, SendError> {
        let uploaded = upload_attachments(pending_attachments, self.attachments()).await?;
        let quote = if let Some(mut q) = quote {
            if q.target_preview.is_none() && !conversation.is_anon() {
                let attachments_svc = self.attachments();
                q.target_preview = self.messages()
                    .get_by_sender_timestamp(&q.target_author_id, q.target_timestamp)
                    .ok().flatten()
                    .map(|m| {
                        let atts: Vec<_> = m
                            .attachments
                            .iter()
                            .filter_map(|id| attachments_svc.get(id).ok().flatten())
                            .collect();
                        crate::services::gateway::shared::message_reply_preview(&m, &atts)
                    })
                    .filter(|t| !t.is_empty());
            }
            Some(q)
        } else { None };
        let intent = build_intent(conversation, text, uploaded.pointers, quote, &options.unwrap_or_default(), self.channels())?;
        let services = OwnedOutgoingServices::from_client(self);
        route_outgoing_owned(intent, &services).await
    }

    /// Send a message and return a pre-shaped UI result.
    ///
    /// Wraps [`send_message`](Self::send_message) and folds the post-send
    /// shaping (effective message id, delivery status, sidebar preview,
    /// local timestamp, resolved attachments, quoted thumbnail) into a
    /// single struct so the UI crate doesn't have to branch on
    /// `bootstrap_initiated` or N+1 fetch attachments by id.
    pub async fn send_message_ui(
        &self,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<crate::client::messaging::SendMessageUiResult, SendError> {
        use crate::client::messaging::SendMessageUiResult;
        use crate::domain::message::truncate_preview_text;

        let mut result = self
            .send_message(conversation, text, pending_attachments, quote, options)
            .await?;

        // Stored but not delivered: the bubble shows "not sent" (tap to retry,
        // `retry_failed_message`) and the reason is surfaced once.
        let failure_notice = result
            .send_error
            .take()
            .map(|e| crate::services::gateway::shared::handle_send_error(&SendError::Message(e), "send"));

        let real_message_id = result.message_id_string();
        let (message_id_effective, delivery_status) = if result.bootstrap_initiated {
            (queued_bubble_id(real_message_id, result.sent_at), "sending")
        } else if failure_notice.is_some() || result.message.as_ref().is_some_and(|m| m.delivery_status == "failed") {
            (real_message_id, "failed")
        } else {
            (real_message_id, "sent")
        };

        let timestamp_display = chrono::DateTime::from_timestamp_nanos(result.sent_at as i64)
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();

        // Same wording as every other sidebar row; built from the stored
        // message so an attachment-only send gets its type label, not "You: ".
        let sidebar_preview = match result.message.as_ref() {
            Some(message) => self.message_sidebar_preview(message),
            None => format!("You: {}", truncate_preview_text(text, 50)),
        };

        // Bootstrap sends carry attachments only after the key exchange, so
        // report an empty list for the "sending" placeholder. Non-bootstrap
        // sends resolve each attachment id against the local store.
        let attachments: Vec<_> = if result.bootstrap_initiated {
            Vec::new()
        } else {
            result
                .attachment_ids
                .iter()
                .filter_map(|id| self.attachments().get(id).ok().flatten())
                .collect()
        };

        // Resolve the quoted parent's media once so the sender's own bubble
        // renders the audio player / preview immediately (not text until reload).
        let reply_parent_media = result
            .message
            .as_ref()
            .map(|m| self.reply_parent_media(m))
            .unwrap_or_default();

        // Prefer the wire quote thumbnail; when it was capped/dropped (>12KB,
        // e.g. big images / gifs) fall back to the parent's LOCAL thumbnail so
        // the sender's own bubble shows the preview immediately instead of a
        // "📷 Photo" label until reload (matches the reloaded canonical message).
        let reply_thumbnail_bytes = result
            .message
            .as_ref()
            .and_then(|m| m.reply_to_thumbnail.clone())
            .or_else(|| reply_parent_media.thumbnail.clone());

        Ok(SendMessageUiResult {
            message_id_effective,
            sent_at: result.sent_at,
            timestamp_display,
            sidebar_preview,
            delivery_status,
            bootstrap_initiated: result.bootstrap_initiated,
            attachments,
            reply_thumbnail_bytes,
            reply_parent_media,
            failure_notice,
            failure_reason: result.message.as_ref().and_then(|m| m.failure_reason.clone()),
        })
    }

    /// Retry a failed channel message or friend DM and return the updated row.
    /// Channels acquire the server message ID; DMs retain their original identity.
    pub async fn retry_failed_message(&self, message_id: &str) -> Result<crate::domain::message::Message, SendError> {
        use crate::services::message::MessageError;
        let id = message_id.parse::<crate::domain::ids::MessageId>()
            .map_err(|_| SendError::Message(MessageError::InvalidContent("Invalid message ID".into())))?;
        let service = &self.services.message_service;
        let message = service.get_message_local(&id)?.ok_or(MessageError::MessageNotFound(id))?;
        let result = if message.conversation_type == crate::domain::message::ConversationType::Channel {
            service.retry_channel_message(&id).await
        } else {
            match service.retry_server_dm(&id).await {
                Ok(_) => service.get_message_local(&id)?.ok_or(MessageError::MessageNotFound(id)),
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(message) => Ok(message),
            Err(error) => {
                // Preparation (e.g. an attachment deleted since the first send)
                // can fail before the transport. Persist its refusal as well.
                if message.can_retry_send() {
                    let mut failed = message;
                    failed.failure_reason = error.permanent_send_failure_reason().map(str::to_owned);
                    service.update_message(&failed)?;
                }
                Err(error.into())
            }
        }
    }

    /// Get a [`ConversationHandle`] for per-conversation operations.
    pub fn conversation(self: &Arc<Self>, conv: ActiveConversation) -> ConversationHandle {
        ConversationHandle::new(conv, self.clone())
    }

    /// Mute or unmute a conversation's notifications.
    pub fn mute_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
        muted: bool,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).mute_conversation(muted)
    }

    /// Read a conversation's persisted mute state (inverse of
    /// [`Self::mute_conversation`]). Lets the UI reflect the real stored mute
    /// state (e.g. when reopening channel settings) instead of a stale bool.
    pub fn is_conversation_muted(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<bool, crate::services::conversation::ConversationError> {
        self.conversation(conv).is_muted()
    }

    /// Delete a conversation from local storage.
    pub fn delete_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).delete_conversation()
    }

    /// Set the disappearing message timer for a conversation.
    ///
    /// Pass `0` to disable. The value is in seconds. See
    /// `ConversationHandle::set_disappearing_timer`.
    pub async fn set_disappearing_timer(
        self: &Arc<Self>,
        conv: ActiveConversation,
        seconds: u32,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).set_disappearing_timer(seconds).await
    }

    /// A conversation's disappearing-messages timer in seconds (`0` = off).
    pub fn disappearing_timer(self: &Arc<Self>, conv: ActiveConversation) -> u32 {
        self.conversation(conv).disappearing_timer()
    }

    /// Whether the user can change `conv`'s timer (friend DMs only).
    pub fn can_set_disappearing_timer(self: &Arc<Self>, conv: ActiveConversation) -> bool {
        self.conversation(conv).can_set_disappearing_timer()
    }

    /// Conversations (by storage key) whose disappearing timer just changed,
    /// from either side: fires when the change's notice is filed, and when a
    /// friend-DM message arrives stating a timer that differs from the last one
    /// seen here (a lost update healed by the next message — its notice is
    /// filed by the gateway, which has no event bus to announce it on).
    pub fn subscribe_disappearing_timer_changes(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::domain::ids::ChannelId> {
        use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let mut src = self.services.event_bus.subscribe_domain();
        let settings = self.channels().settings_repo.clone();
        tokio::spawn(async move {
            let mut seen: std::collections::HashMap<crate::domain::ids::ChannelId, u32> =
                std::collections::HashMap::new();
            loop {
                match src.recv().await {
                    Ok(DomainEvent::MessageReceived { message })
                        if message.conversation_type == ConversationType::ServerDm =>
                    {
                        let peer = crate::domain::ids::UserId::from_uuid(*message.conversation_id.as_uuid());
                        let now = crate::services::disappearing::server_dm_timer(&settings, &peer);
                        let notice = matches!(
                            message.content,
                            MessageContent::System(SystemMessage::DisappearingTimerChanged { .. })
                        );
                        let changed = seen.insert(message.conversation_id, now) != Some(now);
                        if (notice || changed) && tx.send(message.conversation_id).is_err() {
                            break;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Messages the disappearing-messages reaper has just deleted from this
    /// device, one batch per sweep, for the open chat to drop.
    pub fn subscribe_expired_messages(
        &self,
    ) -> tokio::sync::broadcast::Receiver<Vec<crate::domain::message::Message>> {
        self.expired_messages.subscribe()
    }

    // ── Drafts (façade) ─────────────────────────────────────────────

    /// Conversation switch: park `text` under `leaving`, return the draft
    /// parked under `entering` (empty if none). Empty text clears the slot.
    pub fn swap_composer_draft(&self, leaving: &str, text: String, entering: &str) -> String {
        let mut drafts = self.composer_drafts.lock();
        if !leaving.is_empty() {
            if text.is_empty() {
                drafts.remove(leaving);
            } else {
                drafts.insert(leaving.to_owned(), text);
            }
        }
        drafts.remove(entering).unwrap_or_default()
    }

    // ── Conversation listing ────────────────────────────────────────

    /// List all channels the user is a member of.
    pub fn list_channels(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().get_channels_for_user()
    }

    /// Number of conversations with at least one unread message — the Chats
    /// tab and tray badge. Counted from the messages table directly
    /// (`conversation_stats`), because the channel-rooted `list_all()` left out
    /// every channel-free server DM: two unread DMs and one unread channel
    /// read "1".
    ///
    /// Only conversations the sidebar shows as a row count
    /// ([`sidebar_row_keys`](Self::sidebar_row_keys)): with the shield on an
    /// identified channel has no row, and in either mode neither does a DM
    /// with an ex-friend, a channel we left or a group we were removed from —
    /// an unread message there lit the tab with no row to clear it.
    ///
    /// Returns 0 on storage error: a missing badge beats a UI hard-fail.
    pub fn unread_conversation_count(&self) -> u32 {
        self.unread_badges(&crate::state::ActiveConversation::None).total
    }

    /// Absolute unread values for one sidebar row and the Chats total, from
    /// one read of the messages table. The UI sets these; it never adds to
    /// its own copy (the live `+1` and the rebuild counted differently).
    ///
    /// `row` is `conversation`'s own count ([`ActiveConversation::is_for_channel`]).
    /// Blocking storage read: call off the UI thread.
    pub fn unread_badges(&self, conversation: &crate::state::ActiveConversation) -> UnreadBadges {
        let Ok(stats) = self.services.conversation_repo.conversation_stats() else {
            return UnreadBadges::default();
        };
        let rows = self.sidebar_row_keys();
        let mut badges = UnreadBadges::default();
        for (key, st) in &stats {
            if st.unread_count == 0 {
                continue;
            }
            let (id, kind) = key;
            // mesh rows are desktop-only and keyed by the mesh repo;
            // they count whenever the mode shows them, as before.
            let mesh = matches!(kind, ConversationType::Mesh | ConversationType::MeshDm);
            if rows.contains(key) || (mesh && self.visible_in_mode(kind)) {
                badges.total += 1;
            }
            if id.parse().is_ok_and(|cid| conversation.is_for_channel(cid)) {
                badges.row += st.unread_count;
            }
        }
        badges
    }

    /// Unread messages in a private group, for its sidebar row — the same
    /// count `unread_badges` gives the row live, so a rebuild keeps the badge.
    pub fn anon_group_unread_count(&self, group_id: &crate::domain::anon_group::AnonGroupId) -> u32 {
        self.messages()
            .message_repo
            .get_unread_count(&(*group_id).into(), &ConversationType::AnonGroup)
            .unwrap_or(0) as u32
    }

    /// Every conversation the Chats list shows as a row in the current mode,
    /// keyed as `conversation_stats` keys them — the loose lists
    /// (`list_dm_sidebar`, `sidebar_channels`, `sidebar_private_groups`) plus
    /// the folders (`list_sections_with_conversations`). Ids only: the same
    /// inclusion rules as those builders, without their previews.
    fn sidebar_row_keys(&self) -> std::collections::HashSet<(String, ConversationType)> {
        let mut rows = std::collections::HashSet::new();
        for peer in self.dm_peers() {
            rows.insert((peer.to_string(), ConversationType::ServerDm));
        }
        if self.anon_mode_enabled() {
            let anon = self
                .list_anon_dm_conversations()
                .into_iter()
                .chain(self.list_anon_dm_message_requests())
                .chain(self.list_anon_dm_outgoing_requests());
            for dm in anon {
                rows.insert((dm.id.to_string(), ConversationType::AnonDm));
            }
        }
        for channel in self.sidebar_channels() {
            rows.insert((channel.id.to_string(), ConversationType::Channel));
        }
        for group in self.list_anon_groups().unwrap_or_default() {
            rows.insert((group.id.to_string(), ConversationType::AnonGroup));
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for c in section.conversations {
                rows.insert((c.conversation_id, c.conversation_type));
            }
        }
        rows
    }

    /// Whether `message` belongs in the conversation on screen — the one
    /// predicate for "insert it into the open view", "mark it read", "don't
    /// badge it" and "don't notify for it".
    pub fn is_showing(&self, message: &crate::domain::message::Message) -> bool {
        self.is_showing_conversation(message.conversation_id)
    }

    /// [`is_showing`](Self::is_showing) for events that carry only the
    /// conversation id (edits, deletes).
    pub fn is_showing_conversation(&self, conversation_id: crate::domain::ids::ChannelId) -> bool {
        self.chat_state()
            .active_conversation()
            .is_for_channel(conversation_id)
    }

    /// A page of channel members (fetched from the server, which also
    /// refreshes local membership) shaped for display: resolved name, known
    /// handle and avatar. The avatar comes from the member's stored profile —
    /// the same source DM rows use, refreshed when stale — so a member shows
    /// their photo wherever this device can decrypt it (friends: the profile
    /// key only travels with friendship), and initials only when it cannot.
    pub async fn list_members_with_profiles(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::channel::MemberWithProfile>, crate::services::channel::ChannelError> {
        let members = self.channels().get_channel_members(channel_id, limit, offset).await?;
        let me = self.session_current_user_id();
        // Refetch stale member profiles first, as a DM open does for its peer
        // (Signal-Desktop fetches every member's profile on conversation
        // open). Only members whose profile key we hold — a friend's, or a
        // co-member's from their SKDM: those are the profiles this device can
        // decrypt. Throttled per user; a no-op in Shield mode.
        let users = &self.messages().user_repo;
        futures::future::join_all(
            members
                .iter()
                .filter(|m| me.as_ref().is_some_and(|me| {
                    me != &m.user_id
                        && matches!(users.get_friend_profile_key(me, &m.user_id), Ok(Some(_)))
                }))
                .map(|m| self.refresh_profile_if_stale(&m.user_id)),
        )
        .await;
        // Our own role, for what we may do to each row. DMs and Saved
        // Messages have no moderation (the server refuses it).
        let actor = self.get_channel_local(channel_id).and_then(|c| {
            if c.is_dm() || matches!(c.channel_type, crate::domain::ChannelType::SavedMessages) {
                return None;
            }
            let me = me.as_ref()?;
            if c.is_owner(me) {
                return Some(crate::domain::channel::MemberRole::Owner);
            }
            members.iter().find(|m| &m.user_id == me).map(|m| m.role)
        });
        Ok(members
            .into_iter()
            .map(|membership| {
                let is_me = me.as_ref() == Some(&membership.user_id);
                let moderation = crate::domain::channel::MemberModeration::for_target(
                    actor,
                    is_me,
                    membership.role,
                );
                let unverified = self.is_unverified_channel_member(channel_id, &membership.user_id);
                let mut row = self.member_with_profile(membership);
                row.moderation = moderation;
                row.unverified = unverified;
                // Our own row reads "You", as in Signal's member list.
                if is_me {
                    row.display_name = "You".into();
                }
                row
            })
            .collect())
    }

    /// Resolve one membership's display data from local state.
    pub fn member_with_profile(
        &self,
        membership: crate::domain::channel::Membership,
    ) -> crate::domain::channel::MemberWithProfile {
        let uid = membership.user_id;
        crate::domain::channel::MemberWithProfile {
            display_name: self.display_name(&uid),
            // A peer's handle is often not known locally; none rather than
            // the user id dressed up as one.
            username: self.known_username(&uid).unwrap_or_default(),
            avatar_bytes: self.user_avatar(&uid),
            moderation: crate::domain::channel::MemberModeration::default(),
            unverified: false,
            membership,
        }
    }

    /// List all anonymous DM conversations.
    pub fn list_anon_dms(&self) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_conversations()
    }

    // ── Friend / social operations ──────────────────────────────────

    /// Send a friend request.
    pub async fn send_friend_request(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
        note: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::user::UserError> {
        // A friend request is an identified server call naming both accounts.
        // Shield mode's anon path is a private friend code, never this.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::ForbiddenInAnonMode);
        }
        // The service encrypts the intro note (ECIES to the recipient's
        // identity key) — it holds the prekey-fetch client. See
        // `UserService::send_friend_request`.
        let result = self.users().send_friend_request(user_id, note).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(result)
    }

    /// Decrypt an incoming friend-request envelope and return the sender's
    /// profile for the Received-requests tab. Opens the envelope in the
    /// Signal actor (the facade's only signal touch; the identity secret
    /// never leaves the actor) and delegates the protocol work — profile-key
    /// persistence, profile decrypt — to
    /// [`UserService::ingest_received_friend_request`].
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &crate::domain::ids::UserId,
        ciphertext: &[u8],
    ) -> ReceivedFriendRequestProfile {
        let plaintext = if ciphertext.is_empty() {
            None
        } else {
            self.signal()
                .actor
                .open_identity_hpke(crate::crypto::ecies::IdentityHpkeUse::FriendRequest, ciphertext.to_vec())
                .await
                .ok()
        };
        self.users().ingest_received_friend_request(sender_id, plaintext).await
    }

    /// Accept or decline a friend request.
    pub async fn respond_friend_request(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
        accept: bool,
    ) -> Result<crate::services::user::FriendResponseResult, crate::services::user::UserError> {
        let result = self.users().respond_friend_request(user_id, accept).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });

        // On accept, establish the DM Signal session in the background so the
        // first call offer / message skips first-contact Sesame bootstrap on
        // the call path. No-op on the higher-UUID side (Sesame tie-break).
        if accept {
            let client = Arc::clone(self);
            let peer = *user_id;
            tokio::spawn(async move {
                if let Err(e) = client.messages().prewarm_dm_session(&peer).await {
                    tracing::warn!(peer_id = %peer, error = %e, "prewarm DM session after friend accept failed");
                }
            });

            // Acceptance must exchange our profile key for every SDK caller,
            // including headless clients. The requester sends its half from
            // FriendAccepted; neither side waits for a visible message.
            let client = Arc::clone(self);
            let peer = *user_id;
            tokio::spawn(async move {
                if let Err(e) = client.messages().send_profile_key_handshake(&peer).await {
                    tracing::warn!(peer_id = %peer, error = %e, "Accept-request: profile-key handshake send failed");
                }
            });
        }

        Ok(result)
    }

    /// Remove a friend (also used to cancel a pending sent request).
    pub async fn remove_friend(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().remove_friend(user_id).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(())
    }

    /// Re-poll friend requests from the server and emit `FriendRequestsUpdated`
    /// so sidebar / profile modals refresh uniformly. Shared path for
    /// send/respond/remove callers so there's one "source of truth" shape.
    /// Re-poll friend requests and emit `FriendRequestsUpdated` so the UI's
    /// event handler rebuilds the Received/Sent tabs with decrypted sender
    /// profiles + notes. Call on load to populate immediately (the single
    /// source of truth — the UI must not build these rows itself, since the
    /// encrypted envelope only reaches it via this event).
    pub async fn refresh_friend_requests(&self) {
        self.refresh_and_emit_friend_requests().await;
    }

    pub(crate) async fn refresh_and_emit_friend_requests(&self) {
        use crate::inbox::events::FriendRequestInfo;
        use crate::proto::chat_v1::friend_requests_filter;

        let mut user_client = self.services.grpc_pool.user_client();
        let received = match user_client
            .list_friend_requests(friend_requests_filter::Filter::Received, 50, 0)
            .await
        {
            Ok(resp) => resp
                .requests
                .into_iter()
                .map(|r| FriendRequestInfo {
                    user_id: r.user_id,
                    display_name: r.display_name,
                    username: r.username,
                    encrypted_intro: r.encrypted_message,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "refresh_and_emit_friend_requests: received fetch failed");
                return;
            }
        };
        let sent = match user_client
            .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
            .await
        {
            Ok(resp) => resp
                .requests
                .into_iter()
                .map(|r| FriendRequestInfo {
                    user_id: r.user_id,
                    display_name: r.display_name,
                    username: r.username,
                    // Sent requests: we're the sender; note is for the recipient.
                    encrypted_intro: Vec::new(),
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "refresh_and_emit_friend_requests: sent fetch failed");
                return;
            }
        };
        self.event_bus()
            .emit_domain(DomainEvent::FriendRequestsUpdated { received, sent });
    }

    /// Block a user.
    pub async fn block_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().block_user(user_id).await
    }

    /// Unblock a user.
    pub async fn unblock_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().unblock_user(user_id).await
    }

    /// List friends with their profile data (display name, avatar, etc.).
    pub async fn list_friends_with_profiles(&self) -> Result<Vec<(crate::domain::user::Friendship, crate::domain::user::Profile)>, crate::services::user::UserError> {
        self.users().list_friends_with_profiles(100, 0).await
    }

    // ── Anon DM management ──────────────────────────────────────────

    /// Start an anonymous DM conversation with a user.
    pub async fn start_anon_dm(
        &self,
        peer_id: crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().start_conversation(peer_id, message).await
    }

    /// Accept an incoming anon DM request.
    pub async fn accept_anon_dm(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().accept_request(*conversation_id).await
    }

    /// Decline an incoming anon DM request.
    pub fn decline_anon_dm(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().decline_request(conversation_id)
    }

    // ── Anon group management ───────────────────────────────────────

    /// Create an anonymous group.
    pub async fn create_anon_group(
        &self,
        name: String,
        members: Vec<(crate::domain::ids::UserId, crate::domain::anon_group::AnonGroupRole)>,
    ) -> Result<crate::domain::anon_group::AnonGroup, crate::services::anon_group::AnonGroupError> {
        self.anon_groups().create_group_with_roles(name, members).await
    }

    /// Invite members to an anonymous group.
    pub async fn invite_to_anon_group(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        members: Vec<(crate::domain::ids::UserId, crate::domain::anon_group::AnonGroupRole)>,
    ) -> Result<(), crate::services::anon_group::AnonGroupError> {
        self.anon_groups().invite_members(group_id, members).await
    }

    // ── Profile ─────────────────────────────────────────────────────

    /// Store the user's avatar bytes locally (never uploaded).
    pub fn set_avatar_bytes(&self, bytes: &[u8]) -> Result<(), crate::services::auth_service::AuthError> {
        self.anon_dms()
            .set_my_avatar_bytes(bytes)
            .map_err(|e| crate::services::auth_service::AuthError::InvalidInput(e.to_string()))
    }

    /// Change password.
    pub async fn change_password(
        &self,
        current: &str,
        new: &str,
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().change_password(current, new).await
    }

    /// Permanently delete the current user's account.
    ///
    /// IRREVERSIBLE. Hits `AuthService/DeleteAccount` on the server (verifying
    /// the supplied password), consumes the deletion event stream to completion,
    /// and on success clears local auth state (auth interceptor + current
    /// user/device ids) so the app drops to a logged-out state. The UI MUST gate
    /// this behind an explicit confirmation.
    ///
    /// Façade wrapper (feedback_service_layer_architecture): chatter-ui reaches
    /// account deletion only through this method, never the raw AccountService.
    /// The service is built on demand from the same components AuthService holds
    /// (grpc pool, settings repo, auth interceptor) — mirroring the on-demand
    /// construction used for `svr_backup()` / `call_signaling()`.
    pub async fn delete_account(
        &self,
        password: &str,
    ) -> Result<(), crate::services::account_service::AccountError> {
        let auth = self.auth();
        let account = crate::services::account_service::AccountService::new(
            self.services.grpc_pool.clone(),
            auth.settings_repo.clone(),
            auth.auth_interceptor.clone(),
        );
        account.delete_acct(password).await
    }

    /// Get a user's display name.
    pub fn get_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        self.users().get_display_name(user_id)
    }

    /// Signal-style exact username lookup (returns `None` when no account).
    pub async fn lookup_by_username(
        &self,
        username: &str,
    ) -> Result<Option<crate::domain::ids::UserId>, crate::services::user::UserError> {
        // An authenticated lookup ties "who is this account looking for" to
        // our identity — exactly what shield mode exists to hide.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::HandleLookupForbiddenInAnonMode);
        }
        self.users().lookup_by_username(username).await
    }

    // ==================== Username links (Signal-style, L6) ====================

    /// Create (or rotate) this account's username link; returns the shareable
    /// `chatter://u/{link_id}#{entropy}` URL. Rotation kills old URLs.
    pub async fn create_username_link(
        &self,
    ) -> Result<String, crate::services::user::UserError> {
        self.users().create_username_link().await
    }

    /// The locally stored username-link URL, if one exists on this device.
    pub fn username_link_local(
        &self,
    ) -> Result<Option<String>, crate::services::user::UserError> {
        self.users().username_link_local()
    }

    /// Render a username-link URL as a square PNG QR code.
    pub fn username_link_qr(
        &self,
        url: &str,
        size: u32,
    ) -> Result<Vec<u8>, crate::services::user::UserError> {
        crate::services::user::UserService::username_link_qr(url, size)
    }

    /// Resolve someone's username link to their (decrypted username, account
    /// id). The decryption happens locally with the URL-fragment entropy —
    /// the server never learns the username. The account id is the link
    /// owner's, so a link reaches them even with username discovery off.
    pub async fn resolve_username_link(
        &self,
        url: &str,
    ) -> Result<(String, crate::domain::ids::UserId), crate::services::user::UserError>
    {
        // An identified server request, like `lookup_by_username`.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::HandleLookupForbiddenInAnonMode);
        }
        self.users().resolve_username_link(url).await
    }

    /// Delete this account's username link (server + local).
    pub async fn delete_username_link(&self) -> Result<(), crate::services::user::UserError> {
        self.users().delete_username_link().await
    }

    /// Get a user's profile.
    pub async fn get_user_profile(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().get_user_profile(user_id).await
    }

    /// Refetch a peer's profile when it is stale and, if refetched, emit
    /// `UserProfileUpdated` so every view patches name, bio and avatar. A
    /// peer's profile edit does not rotate their profile key, so nothing else
    /// tells us about it. Call on DM open and profile open (Signal-Desktop's
    /// `throttledGetProfiles`); throttled in `UserService`, so cheap to call.
    pub async fn refresh_profile_if_stale(&self, user_id: &crate::domain::ids::UserId) {
        // Shield: an identified fetch naming the friend the moment their DM
        // opens is the link the mailbox hides. The cached profile stands.
        if self.anon_mode_enabled() {
            return;
        }
        match self.users().refresh_profile_if_stale(user_id).await {
            Ok(Some(profile)) => {
                self.event_bus().emit_domain(DomainEvent::UserProfileUpdated {
                    user_id: *user_id,
                    display_name: profile.display_name,
                    status_message: profile.status_message.map(|s| s.text),
                    avatar_bytes: profile.avatar_bytes,
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                });
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(peer_id = %user_id, error = %e, "profile refresh failed"),
        }
    }

    /// Refetch OUR profile (throttled) and restore the local copies the UI
    /// reads — name, bio, avatar — decrypted with our own profile key
    /// (`UserService::restore_own_profile`). Only login did this, so a LINKED
    /// device, or one signed in before that fix, showed initials and a
    /// fallback name while every peer saw the photo. `Some` when refetched.
    /// Not in Shield: it is an identified fetch.
    pub async fn refresh_own_profile(&self) -> Option<crate::domain::user::Profile> {
        if self.anon_mode_enabled() {
            return None;
        }
        let me = self.session_current_user_id()?;
        match self.users().refresh_profile_if_stale(&me).await {
            Ok(Some(profile)) => {
                if let Some(device_id) = self.auth_state().get().device_id {
                    self.auth_state().set_logged_in(me, device_id, profile.display_name.clone());
                }
                Some(profile)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "own profile refresh failed");
                None
            }
        }
    }

    /// Search for users by exact `nickname.discriminator` username.
    ///
    /// Backs the friends-modal user search (F-UI-3). Because display names are
    /// profile-key-encrypted server-side, only an EXACT username resolves to an
    /// account — this mirrors the invite-member search (Signal's discovery
    /// model). Returns at most one hit (the exact match), enriched with the
    /// decrypted display name and whether they are already a friend. An empty
    /// or no-match query yields an empty vec (never an error the UI must show).
    pub async fn search_users(
        &self,
        query: &str,
    ) -> Result<Vec<UserSearchResult>, crate::services::user::UserError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(vec![]);
        }

        let user_id = match self.lookup_by_username(trimmed).await? {
            Some(id) => id,
            None => return Ok(vec![]),
        };

        // Don't surface ourselves as a search hit.
        if self.session_current_user_id().as_ref() == Some(&user_id) {
            return Ok(vec![]);
        }

        // Best-effort profile fetch: a stranger found by exact username has no
        // profile key exchanged yet, so the display name may not decrypt. Still
        // surface the hit (fall back to the username) rather than dropping it.
        let display_name = match self.get_user_profile(&user_id).await {
            Ok(p) if !p.display_name.is_empty() => p.display_name,
            _ => trimmed.to_string(),
        };
        let username = self
            .known_username(&user_id)
            .unwrap_or_else(|| trimmed.to_string());

        // is_friend: the peer id appears in any of our friendship rows.
        let friend_ids: std::collections::HashSet<crate::domain::ids::UserId> = self
            .list_friends(500, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .flat_map(|f| [f.requester_id, f.addressee_id])
            .collect();

        Ok(vec![UserSearchResult {
            is_friend: friend_ids.contains(&user_id),
            user_id,
            display_name,
            username,
        }])
    }

    // ── Message history ─────────────────────────────────────────────

    /// Get recent messages for any conversation type.
    ///
    /// Returns messages in chronological order (oldest first).
    pub fn get_messages(
        &self,
        conversation: &ActiveConversation,
        limit: u32,
    ) -> Result<Vec<Message>, crate::services::message::MessageError> {
        match conversation {
            ActiveConversation::Channel(id) => {
                // Resolve from the channel row (`Channel`, `ServerDm` or
                // `SavedMessages`); the send path stores under the same value.
                let conv_type = crate::services::message::conversion::conversation_type_for_channel(
                    &self.services.channel_repo,
                    id,
                );
                self.messages().get_messages_local(id, &conv_type, None, limit)
            }
            ActiveConversation::ServerDm { peer_id } => {
                let id: crate::domain::ids::ChannelId = (*peer_id).into();
                self.messages().get_messages_local(&id, &ConversationType::ServerDm, None, limit)
            }
            ActiveConversation::AnonDm(id) => {
                // Anon DM messages live in the anon_dm service's own store;
                // delegate so bridge consumers (Android, etc.) don't have to
                // branch on conversation type.
                self.anon_dms()
                    .get_messages(id, None, limit as i32)
                    .map_err(|e| crate::services::message::MessageError::Storage(
                        crate::error::StorageError::InvalidData(e.to_string()),
                    ))
            }
            ActiveConversation::AnonGroup(id) => {
                self.anon_groups()
                    .get_messages(id, None, limit as i32)
                    .map_err(|e| crate::services::message::MessageError::Storage(
                        crate::error::StorageError::InvalidData(e.to_string()),
                    ))
            }
            _ => Ok(vec![]),
        }
    }

    // ── Presence ─────────────────────────────────────────────────────

    /// Set presence status and broadcast to all anon DM conversations.
    pub async fn set_presence(&self, presence: crate::domain::user::PresenceStatus) {
        let conversations = match self.anon_dms().list_conversations() {
            Ok(convs) => convs,
            Err(e) => {
                tracing::warn!("Failed to list conversations for presence: {}", e);
                return;
            }
        };

        for conv in &conversations {
            if !conv.is_ready() {
                continue;
            }
            let service = self.anon_dms().clone();
            let conv_id = conv.id;
            tokio::spawn(async move {
                if let Err(e) = service.send_presence_update(&conv_id, presence).await {
                    tracing::debug!("Failed to send presence update: {}", e);
                }
            });
        }

        // ── Identified presence ──────────────────────────────────────────
        // Publish our REAL online/offline status to the server via the
        // identified UserService.UpdatePresence RPC. The server fans this out
        // to our friends (friend-scoped) so they see us come online / go
        // offline. This is separate from the anon-DM E2E presence above, which
        // stays as-is. Awaited (not spawned) so it completes reliably even
        // during shutdown, before the gRPC connection is torn down.
        let mut user_client = self.services.grpc_pool.user_client();
        let proto = crate::services::presence_service::domain_presence_to_proto(presence);
        if let Err(e) = user_client.update_presence(proto).await {
            tracing::warn!("Failed to publish identified presence to server: {}", e);
        }

        // Our own presence never comes back to us on the user-events stream —
        // the server fans it out to our friends, not to the sender. Emit it
        // locally so the places that show it (profile strip, collapsed rail
        // avatar) track every transition and not only the ones the picker
        // makes: auto-away after idle, the return to online on activity, login
        // with a persisted manual mode.
        if let Some(me) = self.session_current_user_id() {
            let _ = self.services.event_bus.domain_sender().send(
                DomainEvent::UserPresenceChanged {
                    user_id: me,
                    presence,
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                },
            );
        }
    }

    /// Record user activity (resets idle timer).
    ///
    /// Call this on any user interaction (message send, typing, etc.)
    /// to prevent automatic away status.
    pub fn touch_activity(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }

    // ── Thread support ─────────────────────────────────────────────

    // ── AI Integration ─────────────────────────────────────────────

    /// List available AI models for a provider.
    pub async fn list_ai_models(
        &self,
        provider: &str,
    ) -> Result<Vec<(String, String)>, ClientError> {
        self.ai().list_models_by_name(provider)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Create an AI integration and attach it to a channel.
    pub async fn create_ai_integration(
        &self,
        name: &str,
        provider: &str,
        model: &str,
        channel_id: &str,
    ) -> Result<(), ClientError> {
        self.ai().create_integration_for_channel(name, provider, model, channel_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Handle remote hangup: disconnect locally. Accepts the hangup's
    /// `call_id` so core can gate on active-call match without the UI having
    /// to inspect call state.
    #[cfg(feature = "media")]
    pub async fn handle_remote_hangup(&self, call_id: &str) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .handle_remote_hangup(call_worker, Some(call_id))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Data sync ───────────────────────────────────────────────────

    /// Sync channels from server and prefetch tokens.
    ///
    /// Call after `start()` / `login()` to ensure local data is fresh.
    /// The UI's `load_initial_data` calls this internally — SDK consumers
    /// should call it if they need server-fresh data.
    pub async fn sync(&self) {
        // Sync channels from server
        match self.channels().list_channels(100, 0, None).await {
            Ok(channels) => tracing::info!("Synced {} channels from server", channels.len()),
            Err(e) => tracing::warn!("Failed to sync channels: {}", e),
        }

        // Prefetch anonymous tokens for sealed sender
        if let Err(e) = self.prefetch_tokens(10).await {
            tracing::warn!("Failed to prefetch tokens: {}", e);
        }
    }

    // ── Settings ────────────────────────────────────────────────────

    /// Enable or disable sending read receipts, on every one of our devices.
    pub fn set_read_receipts_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions().set_bool_setting(settings_keys::SEND_READ_RECEIPTS, enabled)?;
        self.sync_configuration_to_own_devices();
        Ok(())
    }

    /// Enable or disable typing indicators, on every one of our devices.
    pub fn set_typing_indicators_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions().set_bool_setting(settings_keys::SHOW_TYPING_INDICATORS, enabled)?;
        self.sync_configuration_to_own_devices();
        Ok(())
    }

    /// Send the current privacy settings to our other devices
    /// (`services::configuration_sync`). Runs on the client runtime: the
    /// setters are called from the UI thread. Best effort, like every
    /// own-device sync; a device that misses it keeps the old values until
    /// the next change.
    fn sync_configuration_to_own_devices(&self) {
        let messages = Arc::clone(self.messages());
        let content = crate::services::configuration_sync::content(
            &messages.settings_repo,
            crate::util::time::TimestampNanos::now().as_u64(),
        );
        self.services.runtime_handle.spawn(async move {
            let devices = messages.sync_to_own_devices(&mut messages.auth_client(), content).await;
            tracing::info!(devices, "ConfigurationSync sent to our other devices");
        });
    }

    /// Check if read receipts are enabled (defaults to true).
    pub fn read_receipts_enabled(&self) -> Result<bool, SessionError> {
        Ok(self.sessions().get_bool_setting(settings_keys::SEND_READ_RECEIPTS)?.unwrap_or(true))
    }

    /// Check if typing indicators are enabled (defaults to true).
    pub fn typing_indicators_enabled(&self) -> Result<bool, SessionError> {
        Ok(self.sessions().get_bool_setting(settings_keys::SHOW_TYPING_INDICATORS)?.unwrap_or(true))
    }

    // ── Lifecycle ───────────────────────────────────────────────────

    /// Graceful shutdown. Stops inbox, aborts background tasks, disconnects.
    pub async fn shutdown(&self) {
        // Leave any active call first. The call worker holds capture
        // threads (audio, video, screen) and a LiveKit Room whose
        // RoomManager pumps frames into `SharedVideoFrames`, which in
        // turn pushes UI updates via `slint::invoke_from_event_loop`.
        // If we don't end the call before signalling shutdown, those
        // pushes keep saturating the Slint event queue and a posted
        // `quit_event_loop` request can't drain — the window won't
        // close. Ending the call here stops the frame pump at the
        // source so the loop falls idle and quits.
        #[cfg(feature = "media")]
        {
            if let Err(e) = self.call_leave_room().await {
                tracing::warn!("Error leaving call during shutdown: {}", e);
            }
        }

        // Signal the ntfy push listener (if any) to exit cleanly so its SSE
        // connection closes instead of being torn down via `abort()`.
        if let Some(tx) = self.push_shutdown.lock().take() {
            let _ = tx.send(());
        }

        // Stop inbox provider
        if let Some(provider) = self.ctx.get_device_inbox_provider() {
            if let Err(e) = provider.stop().await {
                tracing::warn!("Error stopping inbox provider: {}", e);
            }
        }

        // Ask the sync engine to exit at its next select point; the abort
        // below is the backstop if it's mid-cycle.
        self.services.sync_service.stop();

        // Abort background tasks
        let handles: Vec<_> = {
            let mut tasks = self.tasks.lock();
            std::mem::take(&mut *tasks)
        };
        for handle in handles {
            handle.abort();
        }

        // Abort the separately-tracked gRPC notification stream.
        if let Some(handle) = self.notif_stream_handle.lock().take() {
            handle.abort();
        }

        // Publish OFFLINE presence so our friends see us go offline. Best-effort
        // and done BEFORE the gRPC disconnect below so the RPC can still reach the
        // server (set_presence awaits the identified UpdatePresence internally).
        self.set_presence(crate::domain::user::PresenceStatus::Offline).await;

        // Disconnect gRPC
        self.services.auth_service.disconnect();

        self.started.store(false, Ordering::SeqCst);
    }

    // ── UI helpers / formatting ─────────────────────────────────────────

    /// Truncate a message body to Signal-style quote-preview length (100 chars
    /// with "..." appended when exceeded). Used by search results and quote UI.
    pub fn truncate_message_preview(&self, text: &str) -> String {
        crate::services::gateway::shared::truncate_preview(text)
    }

    /// Convert a [`SendError`] into a user-facing error string (also logs the
    /// error with the given context). Keeps UI error-formatting consistent
    /// across send call sites.
    pub fn format_send_error(&self, err: &SendError, context: &str) -> String {
        crate::services::gateway::shared::handle_send_error(err, context)
    }

    /// Password strength level (0–4). See
    /// [`crate::services::validation::password_strength`] for scoring details.
    pub fn password_strength(&self, password: &str) -> i32 {
        crate::services::validation::password_strength(password)
    }

    /// PIN strength level (0–3). See
    /// [`crate::services::validation::pin_strength`] for scoring details.
    pub fn pin_strength(&self, pin: &str) -> i32 {
        crate::services::validation::pin_strength(pin)
    }

    // ── Bridge façades ───────────────────────────────────────────────
    //
    // Thin wrappers kept here (not in bin-specific modules) so the
    // `chatter-bridge` crate can reach every operation it needs without
    // widening `pub(crate)` service accessors. Do not expand this block
    // speculatively — add an entry only when a real caller lands.

    /// Persist the notification detail level (Signal's "Show: Name and
    /// message / Name only / No name or message") and apply it now.
    ///
    /// Persist first: a level that takes effect but does not survive a restart
    /// silently reverts to the private default, and one that is saved but not
    /// applied leaks until the next launch. Startup reads the same key
    /// (`app/core_services.rs`, `NOTIFICATION_DETAIL_LEVEL`).
    pub fn set_notification_detail_level(
        &self,
        level: crate::services::notification_service::NotificationDetailLevel,
    ) -> Result<(), crate::error::StorageError> {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::NOTIFICATION_DETAIL_LEVEL, level.as_str())?;
        self.services.notification_service.set_detail_level(level);
        Ok(())
    }

    pub fn notification_detail_level(
        &self,
    ) -> crate::services::notification_service::NotificationDetailLevel {
        self.services.notification_service.detail_level()
    }

    // ── Ntfy push wake-ups ──────────────────────────────────────────────
    //
    // On Android/iOS the OS kills the long-lived gRPC stream when the app is
    // backgrounded. The self-hosted ntfy server still emits a (content-free
    // by default) wake-up SSE event. `configure_push` starts a background
    // task that subscribes to that SSE feed and, for each event, (1) emits
    // a `DomainEvent::PushWakeup` on the event bus so the UI + listeners can
    // react, and (2) pokes the inbox poller so any freshly delivered mailbox
    // messages are pulled immediately rather than waiting for the next
    // scheduled tick.

    /// Start (or replace) the self-hosted ntfy push listener for this client.
    ///
    /// * `ntfy_url` — base URL of the ntfy instance (e.g.
    ///   `https://push.example.com`). An empty string tears down any
    ///   previously-configured listener (graceful no-op if none was
    ///   running).
    ///
    /// The caller must be logged in — the current user's id is used to
    /// derive the topic (`chatter_{user_id}`) matching the server-side
    /// topic-obfuscation scheme.
    ///
    /// Idempotent: calling twice with the same URL cancels the first task
    /// and starts a fresh one (the wake broadcast channel itself is recreated
    /// so there's no stale subscriber to drain).
    pub fn configure_push(self: &Arc<Self>, ntfy_url: &str) -> Result<(), ClientError> {
        // Always shut down the previous listener first — this is the
        // "disable push" path when `ntfy_url` is empty, and also ensures
        // a reconfigure doesn't leak tasks.
        if let Some(tx) = self.push_shutdown.lock().take() {
            let _ = tx.send(());
        }

        let ntfy_url = ntfy_url.trim();
        if ntfy_url.is_empty() {
            // Explicit disable — forget the remembered URL so anon-mode exit
            // doesn't resurrect a listener the caller intentionally stopped.
            *self.push_url.lock() = None;
            tracing::info!("Push listener disabled (empty URL)");
            return Ok(());
        }
        // Remember the desired push URL so anon mode can suspend push on enter
        // and restore this exact config on exit.
        *self.push_url.lock() = Some(ntfy_url.to_string());

        // ntfy is reached over plain HTTP, never Tor. In Shield — including a
        // session that signed in with Shield on — only remember the URL;
        // `rebuild_transport_loops` starts the listener when Shield goes off.
        if self.anon_mode_enabled() {
            tracing::info!("Push listener deferred: Shield is on (poll-only delivery)");
            return Ok(());
        }

        if self.services.auth_state.get().current_user_id.is_none() {
            return Err(ClientError::Validation(
                "configure_push requires an authenticated user".into(),
            ));
        }

        // Use the opaque topic the server issued in AuthResponse (persisted at
        // login), not a locally-derived chatter_{user_id}. Empty means push is
        // disabled server-side (or we haven't logged in since the field was
        // added) — nothing to subscribe to.
        let topic = {
            use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
            SettingsRepository::new(self.services.database.clone())
                .get_string(keys::PUSH_TOPIC)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        if topic.is_empty() {
            tracing::info!("Push listener not started: no server-issued push topic");
            return Ok(());
        }

        let config = crate::network::streaming::push_listener::PushListenerConfig::new(
            ntfy_url.to_string(),
            topic.clone(),
        );

        // Fresh shutdown channel for this listener instance.
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        *self.push_shutdown.lock() = Some(shutdown_tx);

        let listener = std::sync::Arc::new(
            crate::network::streaming::push_listener::PushListener::new(config),
        );
        let wake_rx = listener.subscribe();
        let event_bus = self.services.event_bus.clone();
        let ui = self.services.clone();

        // Task A: SSE consumer. Runs the listener's reconnect loop; exits
        // when shutdown fires.
        let listen_listener = listener.clone();
        let listen_shutdown = shutdown_rx;
        let h_sse = tokio::spawn(async move {
            listen_listener.listen(listen_shutdown).await;
        });

        // Task B: wake-event router. Bridges the listener's broadcast into
        // the event bus + inbox-poller poke. A separate task so the SSE
        // consumer's lifetime isn't tied to the inbox provider being ready.
        let router_topic = topic.clone();
        let mut wake_rx = wake_rx;
        let h_router = tokio::spawn(async move {
            loop {
                match wake_rx.recv().await {
                    Ok(event) => {
                        // (1) Fan out on the domain event bus — narrow
                        // payload, no body text leaks.
                        let kind = event.kind();
                        let _ = event_bus.domain_sender().send(
                            crate::events::DomainEvent::PushWakeup {
                                topic: router_topic.clone(),
                                kind,
                            },
                        );

                        // (2) Trigger a one-shot mailbox sync. The inbox
                        // provider is self-started and idempotent — calling
                        // it when already running is a cheap no-op; when it
                        // was torn down (e.g. Android backgrounded) this
                        // restarts the poller so new messages get pulled.
                        let _ = ui.start_inbox_provider().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            dropped = n,
                            "Push wake-up router lagged — dropped events"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Store handles so Drop / shutdown aborts them.
        {
            let mut tasks = self.tasks.lock();
            tasks.push(h_sse);
            tasks.push(h_router);
        }

        tracing::info!(topic = %topic, "Ntfy push listener configured");
        Ok(())
    }

    // ── Escape hatches ──────────────────────────────────────────────────

}

impl Drop for ChatterClient {
    fn drop(&mut self) {
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

// =============================================================================
// Private helpers
// =============================================================================

async fn upload_attachments(pending: &[PendingAttachment], attachment_service: &AttachmentService) -> Result<UploadedAttachments, AttachmentError> {
    let mut pointers = Vec::with_capacity(pending.len());
    let mut attachments = Vec::with_capacity(pending.len());
    for att in pending {
        if let Some(pointer) = &att.pointer {
            pointers.push(pointer.clone());
            if let Ok(Some(full_att)) = attachment_service.get(&att.id) { attachments.push(full_att); }
            continue;
        }
        match attachment_service.upload_to_cdn_encrypted(&att.path).await {
            Ok(res) => {
                pointers.push(res.to_attachment_pointer());
                if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) { attachments.push(full_att); }
            }
            Err(e) => {
                // Propagate rather than silently dropping the attachment: if this
                // was the only content in the message, swallowing the error here
                // would leave `pointers` empty and the send would instead fail
                // with the misleading "message must have text or attachments".
                tracing::error!(path = %att.path.display(), error = %e, "Failed to upload attachment");
                return Err(e);
            }
        }
    }
    Ok(UploadedAttachments { pointers, attachments })
}

/// Received group-call keys handled at once by the call-policy listener;
/// beyond this a key is dropped rather than queued behind a stalled server.
#[cfg(feature = "media")]
const CALL_KEY_TASKS: usize = 8;

/// Who left and who joined between two views of a call's room.
#[cfg(feature = "media")]
fn roster_changes(
    before: &std::collections::HashSet<String>,
    now: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    (before.difference(now).cloned().collect(), now.difference(before).cloned().collect())
}

fn build_intent(
    conversation: ActiveConversation, text: &str, attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<QuoteIntent>, options: &SendOptions, channel_service: &ChannelService,
) -> Result<OutgoingIntent, SendError> {
    match conversation {
        ActiveConversation::AnonDm(id) => Ok(OutgoingIntent::AnonDm {
            conversation_id: id, text: text.to_string(), attachment_pointers, quote,
        }),
        ActiveConversation::AnonGroup(id) => Ok(OutgoingIntent::AnonGroup {
            group_id: id, text: text.to_string(), attachment_pointers, quote,
        }),
        ActiveConversation::ServerDm { peer_id } => Ok(OutgoingIntent::ServerDm {
            peer_id, text: text.to_string(), attachment_pointers, quote,
        }),
        ActiveConversation::Channel(id) => {
            let is_saved = channel_service.get_channel_local(&id).ok().flatten()
                .map(|c| c.channel_type == ChannelType::SavedMessages).unwrap_or(false);
            if is_saved {
                let att_ids = attachment_pointers.iter()
                    .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key)).collect();
                Ok(OutgoingIntent::SavedMessages { text: text.to_string(), attachment_ids: att_ids })
            } else {
                Ok(OutgoingIntent::Channel {
                    channel_id: id, text: text.to_string(), attachment_pointers,
                    file_paths: vec![], thread_id: options.thread_id,
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

#[cfg(test)]
mod queued_bubble_id_tests {
    /// The bubble of a DM queued behind the Shield bootstrap carries the
    /// stored message's id, the one `MessageDeliveryChanged` names.
    #[test]
    fn a_stored_queued_dm_keeps_its_real_id() {
        let id = crate::domain::ids::MessageId::new().to_string();
        assert_eq!(super::queued_bubble_id(id.clone(), 7), id);
        assert_eq!(super::queued_bubble_id(String::new(), 7), "pending-7");
    }
}

#[cfg(test)]
mod upload_attachments_tests {
    //! Regression test for the "must have text or attachments" mask: a
    //! failed pending-attachment upload must propagate its `AttachmentError`
    //! instead of being dropped, which previously left `pointers` empty and
    //! sent attachments-only messages into the empty-content validation path.
    use super::*;
    use crate::crypto::attachment_crypto::override_max_attachment_size;
    use crate::domain::AttachmentId;
    use crate::storage::database::Database;
    use crate::storage::repositories::AttachmentRepository;
    fn make_service() -> (AttachmentService, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("upload_attachments_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        let repo = Arc::new(AttachmentRepository::new(db));
        (AttachmentService::new_local(repo), temp_dir)
    }

    fn pending_for(path: std::path::PathBuf, size: u64) -> PendingAttachment {
        PendingAttachment {
            id: AttachmentId::from_cdn_key("pending-test"),
            filename: "big.bin".to_string(),
            path,
            size,
            mime_type: "application/octet-stream".to_string(),
            pointer: None,
        }
    }

    #[tokio::test]
    async fn failed_upload_propagates_error_instead_of_dropping_pointer() {
        let _cap = override_max_attachment_size(100);

        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 200];
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), &payload).unwrap();

        let pending = vec![pending_for(input.path().to_path_buf(), payload.len() as u64)];
        let result = upload_attachments(&pending, &svc).await;


        match result {
            Err(AttachmentError::EncryptionError(msg)) => {
                assert!(msg.contains("attachment too large"), "unexpected msg: {}", msg);
            }
            other => panic!(
                "expected propagated AttachmentError::EncryptionError(AttachmentTooLarge), got {:?}",
                other.map(|u| u.pointers.len())
            ),
        }
    }
}

#[cfg(test)]
mod transport_switch_tests {
    //! GAP 2 bookkeeping: toggling anon mode must stop/re-spawn the long-lived
    //! loops so they re-dial over the new transport, and must be idempotent /
    //! race-safe (the flip runs on the UI thread). A full transport switch
    //! needs the `onion` feature + a live Tor server, so these assert the
    //! stop/restart bookkeeping directly instead.
    use super::*;

    fn build_test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        // These tests build a real ChatterClient, which stores the DB passphrase
        // in the OS keyring. A headless/CI session has no Secret Service, so point
        // keyring at its in-memory mock backend (process-global, set once) — the
        // tests exercise transport-switch bookkeeping, not credential storage.
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });

        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        // The directory must outlive the client: dropping it deletes the
        // database under a live connection.
        (client, dir)
    }

    /// Entering anon mode must fire the ntfy push shutdown and clear the slot
    /// (product decision: push OFF while anon, poll-only). Idempotent: a second
    /// enter with no listener is a clean no-op.
    #[tokio::test]
    async fn anon_enter_suspends_push_and_is_idempotent() {
        let (client, _dir) = build_test_client();

        // Simulate a configured push listener with anon mode enabled — the
        // rebuild reads the LIVE atomic (Fix 3), not a param, so set it first.
        client.anon_mode.store(true, Ordering::Release);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<()>(1);
        *client.push_shutdown.lock() = Some(tx);
        *client.push_url.lock() = Some("http://push.example".into());

        client.rebuild_transport_loops().await;

        // Shutdown fired and the slot was taken.
        assert!(rx.try_recv().is_ok(), "push shutdown signal must fire on anon enter");
        assert!(client.push_shutdown.lock().is_none(), "push slot must be cleared");
        // The URL is remembered so exit can restore it.
        assert_eq!(client.push_url.lock().clone(), Some("http://push.example".into()));

        // Idempotent: second enter with no live listener does not panic.
        client.rebuild_transport_loops().await;
        assert!(client.push_shutdown.lock().is_none());
    }

    /// Fix 3 convergence: with the live atomic at `false` (final transport =
    /// direct), a rebuild must NOT suspend push even if invoked as part of a
    /// toggle burst — the push decision reads the authoritative atomic, not a
    /// stale captured param.
    #[tokio::test]
    async fn push_decision_follows_live_atomic_not_stale_param() {
        let (client, _dir) = build_test_client();
        client.anon_mode.store(false, Ordering::Release);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<()>(1);
        *client.push_shutdown.lock() = Some(tx);

        client.rebuild_transport_loops().await;

        // anon=false → push must be left intact (no shutdown fired).
        assert!(rx.try_recv().is_err(), "push must not be suspended when live anon=false");
        assert!(client.push_shutdown.lock().is_some(), "push slot must be preserved");
    }

    /// Fix 1: `refresh_client` on the group fanout runs without deadlock and is
    /// idempotent under repeated invocation (rapid toggle). Uses the live pool.
    #[tokio::test]
    async fn group_fanout_refresh_is_idempotent() {
        let (client, _dir) = build_test_client();
        client.ctx.init_group_fanout();
        let fanout = client.ctx.get_group_fanout().expect("fanout initialized");

        // Repeated refresh (simulated rapid toggle) must not deadlock the
        // tokio Mutex or panic.
        for _ in 0..5 {
            fanout.refresh_client(&client.services.grpc_pool).await;
        }
    }

    /// Re-spawning the notification stream before login is a safe no-op — the
    /// stream hasn't started, so the "started" flag stays clear and no task is
    /// installed.
    #[tokio::test]
    async fn notification_respawn_is_noop_before_login() {
        let (client, _dir) = build_test_client();
        assert!(!client.authed_notif_started.load(Ordering::SeqCst));

        client.respawn_notification_stream();
        client.respawn_notification_stream();

        assert!(!client.authed_notif_started.load(Ordering::SeqCst));
        assert!(
            client.notif_stream_handle.lock().is_none(),
            "no notification stream task should be installed before login"
        );
    }

    /// The public toggle must remain race-safe when hammered: the sync flag
    /// tracks the last value and the spawned rebuild tasks (no provider yet)
    /// converge without panicking or hanging the caller.
    #[tokio::test]
    async fn rapid_toggle_converges_without_hang() {
        let (client, _dir) = build_test_client();
        for _ in 0..10 {
            client.set_anon_mode_enabled(true);
            client.set_anon_mode_enabled(false);
        }
        assert!(!client.anon_mode_enabled());
        client.set_anon_mode_enabled(true);
        assert!(client.anon_mode_enabled());
        // Give the serialized rebuild tasks a chance to drain.
        tokio::task::yield_now().await;
    }

    /// Pre-login Shield fails closed when the build has nowhere to route over
    /// Tor: an error, and nothing changed — no Shield flag, still direct.
    #[tokio::test]
    async fn shield_before_login_refuses_without_onion() {
        use crate::network::grpc::client_pool::TransportMode;
        let (client, _dir) = build_test_client();
        client.services.grpc_pool.set_onion_address(None);

        assert!(!client.shield_available());
        assert!(client.set_shield_before_login(true).is_err());
        assert!(!client.anon_mode_enabled());
        assert_eq!(client.services.grpc_pool.transport_mode(), TransportMode::Direct);
    }

    /// Signed-out startup contacts nothing: no boot connect, no reachability
    /// monitor, no "unreachable" claim, until a sign-in action connects — so
    /// a user who then picks Shield has leaked nothing. The first `connect`
    /// starts the monitor.
    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn signed_out_startup_contacts_nothing_until_a_sign_in_action() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(sock);
            }
        });

        let (client, _dir) = build_test_client();
        client.set_server_url(&direct_url).unwrap();
        assert!(!client.is_logged_in());

        let started = std::time::Instant::now();
        assert!(!client.startup_connect(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(1), "signed-out startup must not dial");
        assert!(!client.server_unreachable(), "no contact yet: unreachable would be a guess");
        assert!(!client.server_monitor_started.load(Ordering::SeqCst), "monitor must wait");
        tokio::time::sleep(Duration::from_secs(6)).await; // past one monitor tick
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "signed-out startup dialled the server");

        // The user picks Shield, then signs up: still nothing direct.
        client
            .services
            .grpc_pool
            .set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        client.set_shield_before_login(true).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client.register("deferred", "pw-123456789012", "D", "dev", None, Some("correct-horse-9")),
        )
        .await;
        assert!(client.server_monitor_started.load(Ordering::SeqCst), "first contact starts the monitor");
        tokio::time::sleep(Duration::from_secs(6)).await; // monitor never probes on onion
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "Shield sign-up after a deferred start dialled direct");

        // Control: Shield off, a direct connect does reach the listener.
        client.set_shield_before_login(false).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), client.connect()).await;
        for _ in 0..20 {
            if accepts.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(accepts.load(Ordering::SeqCst) > 0, "listener should see the direct dial");
    }

    /// The owner requirement: with Shield chosen before sign-in, onboarding
    /// never dials the server directly. The "direct server" here is a local
    /// listener counting TCP accepts; the pinned .onion is fake, so the onion
    /// attempt can only fail or time out. Registration, sign-in and a direct
    /// URL change must all produce zero direct connections and never succeed.
    /// Turning Shield off afterwards does dial the listener — proof the
    /// counter would have caught a leak.
    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn shield_before_login_never_dials_direct() {
        use crate::network::grpc::client_pool::TransportMode;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(sock);
            }
        });

        // Built on the placeholder URL every test uses: the builder publishes
        // its URL in a process-wide env var (`app/context.rs`), so building on
        // the listener's URL would point other tests' clients at it. The URL
        // goes to this client's pool only.
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        client.set_server_url(&direct_url).unwrap();
        let fake_onion = format!("http://{}.onion:443", "a".repeat(56));
        client.services.grpc_pool.set_onion_address(Some(fake_onion));

        assert!(client.shield_available());
        client.set_shield_before_login(true).expect("Shield before login");
        assert!(client.anon_mode_enabled(), "the session must come up in Shield");
        assert_eq!(client.services.grpc_pool.transport_mode(), TransportMode::Onion);

        // The register form always re-applies its Server URL field.
        client.set_server_url(&direct_url).unwrap();
        assert!(client.endpoint().contains(".onion"), "a URL change must not leave onion");

        let budget = Duration::from_secs(5);
        let registered = tokio::time::timeout(
            budget,
            client.register("shieldtest", "pw-123456789012", "S", "dev", None, Some("correct-horse-9")),
        )
        .await;
        assert!(!matches!(registered, Ok(Ok(_))), "fake .onion cannot register");
        let logged_in = tokio::time::timeout(
            budget,
            client.login("shieldtest", "pw-123456789012", "dev", None),
        )
        .await;
        assert!(!matches!(logged_in, Ok(Ok(_))), "fake .onion cannot sign in");
        let connected = tokio::time::timeout(budget, client.connect()).await;
        assert!(!matches!(connected, Ok(Ok(()))));
        // Storage transfers (link-device archive) ride Tor too, never direct.
        let uploaded = tokio::time::timeout(
            budget,
            client.message_backup().upload_link_archive(b"archive", &format!("{direct_url}/blob")),
        )
        .await;
        assert!(!matches!(uploaded, Ok(Ok(()))));

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "Shield onboarding dialled the server directly");
        assert!(client.anon_mode_enabled(), "a failed onion dial must not drop Shield");
        assert_eq!(client.services.grpc_pool.transport_mode(), TransportMode::Onion);

        // Sanity: the counter sees a direct dial once Shield is off.
        client.set_shield_before_login(false).unwrap();
        let _ = tokio::time::timeout(budget, client.connect()).await;
        for _ in 0..20 {
            if accepts.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(accepts.load(Ordering::SeqCst) > 0, "listener should see the direct dial");
    }
}

#[cfg(test)]
mod unread_badge_tests {
    //! Badges are absolute counts from storage, filtered by the current mode.
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};
    use crate::state::ActiveConversation;

    /// The `TempDir` must outlive the client: the database's reader pool
    /// opens connections lazily BY PATH (`ReaderPool::open_reader`), so once
    /// the directory is deleted any read that needs a fresh reader fails, and
    /// `server_dm_timer` swallows a failed read as "timer 0".
    fn build_test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (client, dir)
    }

    fn receive(client: &ChatterClient, id: ChannelId, kind: ConversationType, sent_at: u64) {
        let msg = Message::new_text(id, kind, Some(UserId::new()), "hi".into(), sent_at);
        client.messages().store_message_local(&msg).expect("store");
    }

    /// Make `peer` an accepted friend of a logged-in `me`: the one kind of
    /// server DM the sidebar gives a row (`listed_friends`).
    fn befriend(client: &ChatterClient, peer: UserId) {
        let me = UserId::new();
        client.auth_state().set_logged_in(me, 1, "Me".into());
        client
            .messages()
            .settings_repo
            .set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        let users = &client.messages().user_repo;
        users.ensure_user_exists(&me).unwrap();
        users.ensure_user_exists(&peer).unwrap();
        let mut friendship = crate::domain::user::Friendship::new_request(me, peer);
        friendship.accept();
        users.insert_friendship(&friendship).unwrap();
    }

    #[tokio::test]
    async fn badges_are_absolute_and_follow_the_mode() {
        let (client, _dir) = build_test_client();
        let peer = UserId::new();
        befriend(&client, peer);
        let mut anon_dm = crate::domain::anon_dm::AnonDm::new_initiated(UserId::new());
        anon_dm.status = crate::domain::anon_dm::AnonDmStatus::Active;
        client.anon_dms().anon_dm_repo().insert_conversation(&anon_dm).unwrap();
        let anon = anon_dm.id;
        receive(&client, ChannelId::from(peer), ConversationType::ServerDm, 1);
        receive(&client, ChannelId::from(peer), ConversationType::ServerDm, 2);
        receive(&client, anon.into(), ConversationType::AnonDm, 3);

        let dm = ActiveConversation::ServerDm { peer_id: peer };
        // Two unread on the DM row; the anon DM is hidden, so the total is 1.
        assert_eq!(client.unread_badges(&dm), UnreadBadges { row: 2, total: 1 });

        // Shield on: the anon DM shows, and so does the friend DM (friend DMs
        // show in both modes — `visible_in_mode`), so the total is 2.
        client.anon_mode.store(true, Ordering::Release);
        assert_eq!(
            client.unread_badges(&ActiveConversation::AnonDm(anon)),
            UnreadBadges { row: 1, total: 2 },
        );
        assert_eq!(client.unread_conversation_count(), 2);
    }

    /// F73: the Chats total counts only conversations the list shows as a
    /// row, and call records never count.
    #[tokio::test]
    async fn rowless_conversations_and_call_records_never_badge() {
        use crate::domain::message::{CallMessage, CallMessageType, MessageContent};
        let (client, _dir) = build_test_client();
        let friend = UserId::new();
        befriend(&client, friend);

        // A DM with someone who is not a friend (unfriended, or never one)
        // and a channel we are not in: neither has a row.
        receive(&client, ChannelId::from(UserId::new()), ConversationType::ServerDm, 1);
        receive(&client, ChannelId::new(), ConversationType::Channel, 2);
        assert_eq!(client.unread_conversation_count(), 0);

        // A missed call filed in the friend's DM is history, not unread.
        let mut call = Message::new_text(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            Some(friend),
            String::new(),
            3,
        );
        call.content = MessageContent::Call(CallMessage {
            call_id: "c".into(),
            call_type: CallMessageType::Missed,
            initiator_id: friend,
            initiator_name: String::new(),
            started_at: None,
            ended_at: None,
            duration: None,
            participant_count: 2,
        });
        client.messages().store_message_local(&call).expect("store");
        assert_eq!(client.unread_conversation_count(), 0);

        receive(&client, ChannelId::from(friend), ConversationType::ServerDm, 4);
        assert_eq!(client.unread_conversation_count(), 1);
    }

    /// A lost timer update healed by the next visible message: the gateway
    /// adopts the new timer with no event bus, so the subscriber notices the
    /// changed value on that message and the header refreshes. A message that
    /// restates the same timer does not fire.
    #[tokio::test]
    async fn timer_changes_fire_when_a_message_heals_the_timer() {
        use std::time::Duration;
        let (client, _dir) = build_test_client();
        let mut changes = client.subscribe_disappearing_timer_changes();
        let peer = UserId::new();
        let dm = |sent_at| DomainEvent::MessageReceived {
            message: Message::new_text(
                ChannelId::from(peer),
                ConversationType::ServerDm,
                Some(peer),
                "hi".into(),
                sent_at,
            ),
        };
        let bus = &client.services.event_bus;

        // The subscriber reads the stored timer when it handles a message, and
        // on this current-thread runtime it only runs once the test awaits: so
        // receive each change before moving the stored timer on.
        async fn next_change(changes: &mut tokio::sync::broadcast::Receiver<ChannelId>) -> ChannelId {
            tokio::time::timeout(Duration::from_secs(2), changes.recv())
                .await
                .expect("a change")
                .expect("open")
        }

        bus.emit_domain(dm(1)); // first sight: fires
        bus.emit_domain(dm(2)); // same timer: silent
        assert_eq!(next_change(&mut changes).await, ChannelId::from(peer));

        client
            .channels()
            .settings_repo
            .set(
                &format!("conv.disappearing.server_dm.{peer}"),
                &crate::services::disappearing::DmTimer { seconds: 3_600, version: 2 },
            )
            .expect("set timer");
        bus.emit_domain(dm(3)); // healed to 1 hour: fires
        assert_eq!(next_change(&mut changes).await, ChannelId::from(peer));

        assert!(
            tokio::time::timeout(Duration::from_millis(200), changes.recv()).await.is_err(),
            "a message restating the same timer is not a change"
        );
    }
}

#[cfg(all(test, feature = "media"))]
mod call_policy_tests {
    use super::roster_changes;
    use std::collections::HashSet;

    /// A lag that swallowed a ParticipantLeft still yields the departure
    /// (and so a rotation) once the roster is re-read.
    #[test]
    fn lagged_roster_resync_finds_departures_and_joins() {
        let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let (gone, new) = roster_changes(&set(&["a", "b", "c"]), &set(&["a", "c", "d"]));
        assert_eq!(gone, vec!["b".to_string()]);
        assert_eq!(new, vec!["d".to_string()]);
        let (gone, new) = roster_changes(&set(&["a"]), &set(&["a"]));
        assert!(gone.is_empty() && new.is_empty());
    }
}
