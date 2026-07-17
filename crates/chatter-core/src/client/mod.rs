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
mod auth;
pub mod calls;
pub mod channels;
pub mod events;
#[cfg(feature = "jarvis")]
mod jarvis;
mod link_device;
mod messaging;
pub use messaging::{InviteCandidate, ReplyParentMedia};

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
mod notification;
mod pin;
mod profile;

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
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
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
    reconnect_config: Option<crate::network::ReconnectConfig>,
    max_attachment_size: Option<u64>,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            server_url: None,
            data_dir: None,
            notification_sink: None,
            reconnect_config: None,
            max_attachment_size: None,
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

    /// Override the reconnect backoff / give-up tuning. Defaults reproduce
    /// the historical behavior exactly — override this on mobile deployments
    /// that want tighter give-up thresholds than desktop.
    pub fn reconnect_config(mut self, cfg: crate::network::ReconnectConfig) -> Self {
        self.reconnect_config = Some(cfg);
        self
    }

    /// Override the maximum attachment size in bytes. If `None`, the built-in
    /// default (1 GiB) is used. Applied at startup via
    /// `crate::crypto::set_max_attachment_size` so all upload entrypoints in
    /// `AttachmentService` enforce the cap.
    pub fn max_attachment_size(mut self, bytes: u64) -> Self {
        self.max_attachment_size = Some(bytes);
        self
    }

    /// Build the client. Initializes database, crypto, and network pool.
    ///
    /// Must be called within a tokio runtime (e.g. inside `#[tokio::main]`
    /// or after `Runtime::new()`).
    pub fn build(self) -> Result<Arc<ChatterClient>, ClientError> {
        // Apply the attachment size cap before any service is constructed so
        // every upload path sees the configured value.
        if let Some(cap) = self.max_attachment_size {
            crate::crypto::set_max_attachment_size(cap);
        }
        let ctx = AppContext::with_config_and_sink(self.server_url, self.data_dir, self.notification_sink)?;
        let services = ctx.core_services();
        let reconnect_config = self.reconnect_config.unwrap_or_default();
        Ok(Arc::new(ChatterClient {
            ctx,
            services,
            tasks: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(0),
            reconnect_config: Mutex::new(reconnect_config),
            push_shutdown: Mutex::new(None),
            call_signaling: std::sync::OnceLock::new(),
            svr_backup_service: std::sync::OnceLock::new(),
            outgoing_typing: crate::services::outgoing_typing::OutgoingTypingManager::new(),
            link_device_service: std::sync::OnceLock::new(),
            anon_mode: AtomicBool::new(false),
            authed_notif_started: AtomicBool::new(false),
            notif_stream_handle: Mutex::new(None),
            push_url: Mutex::new(None),
            transport_switch_lock: tokio::sync::Mutex::new(()),
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
    #[error("validation: {0}")]
    Validation(String),
}

/// Result of a successful recovery code usage.
///
/// After this, the user can login with their new password + new PIN.
/// SVR backup is created automatically during the subsequent login flow.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Number of unused recovery codes remaining on the account.
    pub remaining_codes: i32,
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
    /// Reconnect backoff / give-up tuning. Mutable at runtime so the FFI
    /// layer can adjust it without rebuilding the client (e.g. mobile apps
    /// tighten this on battery-saver mode).
    reconnect_config: Mutex<crate::network::ReconnectConfig>,
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
    /// Cached device-linking service (lazy because deps are read-only and only
    /// touched when the user opens the link-device flow).
    link_device_service: std::sync::OnceLock<Arc<crate::services::link_device_service::LinkDeviceService>>,
    /// In-memory "Anon Mode" UI flag. When set, the UI hides server-friend
    /// DMs + public channels and shows only anon DMs / anon groups. Pure
    /// presentation toggle — no send-path or storage gating. Resets to
    /// false on every client construction (not persisted by design — auto
    /// revert on restart is the privacy property).
    anon_mode: AtomicBool,
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
}

pub use crate::services::user::ReceivedFriendRequestProfile;

impl ChatterClient {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
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
    crate::service_accessor!(mailbox_manager, MailboxManager, mailbox_manager);
    crate::service_accessor!(local_ai, LocalAIService, local_ai_service);
    crate::service_accessor!(dm_bootstrap, DmBootstrapService, dm_bootstrap_service);
    crate::service_accessor!(
        notifications,
        crate::services::notification_service::NotificationService,
        notification_service
    );
    crate::service_accessor!(sections, SectionService, section_service);
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

    /// Get the call signaling service (persistent instance for pending state).
    pub fn call_signaling(&self) -> &crate::services::call_signaling::CallSignalingService {
        self.call_signaling.get_or_init(|| {
            crate::services::call_signaling::CallSignalingService::new(
                self.messages().clone(),
                self.mailbox_manager().clone(),
                self.channels().clone(),
                self.anon_dms().clone(),
                self.anon_groups().clone(),
                self.signal().clone(),
                self.services.grpc_pool.clone(),
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
                    Ok(DomainEvent::GroupCallKeyReceived {
                        call_id,
                        sender_id,
                        key_material,
                        key_epoch,
                        channel_id,
                    }) => {
                        if tx.send(events::CallKeyEvent::GroupKeyReceived {
                            call_id,
                            sender_id,
                            key_material,
                            key_epoch,
                            channel_id,
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
        if let crate::state::ActiveConversation::MailboxDm { peer_id, .. }
        | crate::state::ActiveConversation::ServerDm { peer_id, .. } = &conversation
        {
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

    /// Toggle the "send sealed-sender messages unrestricted" privacy setting.
    pub fn set_sealed_sender_unrestricted(
        &self,
        unrestricted: bool,
    ) -> Result<(), crate::services::session_service::SessionError> {
        self.sessions()
            .set_bool_setting("privacy.sealed_sender_unrestricted", unrestricted)
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

    /// Session-level current user id (None when logged out).
    pub fn session_current_user_id(&self) -> Option<crate::domain::ids::UserId> {
        self.sessions().current_user_id()
    }

    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<crate::services::session_service::Session>, crate::services::session_service::SessionError> {
        self.sessions().list_sessions().await
    }

    pub async fn revoke_session(
        &self,
        device_id: u32,
    ) -> Result<(), crate::services::session_service::SessionError> {
        self.sessions().revoke_session(device_id).await
    }

    pub async fn revoke_all_other_sessions(
        &self,
    ) -> Result<(), crate::services::session_service::SessionError> {
        self.sessions().revoke_all_other_sessions().await
    }

    // ── Mailbox / DM bootstrapping façade ────────────────────────────────

    /// Ensure mailbox manager initialized and token pool has capacity.
    pub async fn ensure_mailbox_ready(
        &self,
        min_tokens: usize,
    ) -> Result<(), crate::services::mailbox_manager::MailboxError> {
        let mm = self.mailbox_manager();
        if let Err(e) = mm.initialize().await {
            tracing::warn!(error = %e, "mailbox_manager initialize failed (may be already initialized)");
        }
        if mm.token_count().await < min_tokens {
            mm.acquire_tokens(min_tokens).await?;
        }
        Ok(())
    }

    /// Get or create a pairwise mailbox for a conversation with the given TTL.
    pub async fn get_or_create_mailbox(
        &self,
        conversation_id: &str,
        ttl_seconds: u64,
    ) -> Result<crate::services::mailbox_manager::ActiveMailbox, crate::services::mailbox_manager::MailboxError>
    {
        self.mailbox_manager()
            .get_or_create_mailbox(conversation_id, ttl_seconds)
            .await
    }

    /// Flush any pending UI messages for a peer after DM bootstrap completes.
    pub async fn flush_pending_dm_messages(&self, peer_id: &crate::domain::ids::UserId) {
        self.dm_bootstrap().flush_pending_ui_messages(peer_id).await;
    }

    // ── Sidebar sections (custom grouping) façade ────────────────────────

    pub fn list_sections_with_conversations(
        &self,
    ) -> Result<Vec<crate::domain::section::CustomSection>, crate::services::section_service::SectionError>
    {
        self.sections().list_sections_with_conversations()
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

    pub fn get_conversation_metadata(
        &self,
        conversation_id: &str,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Option<crate::domain::UnifiedConversation> {
        self.sections()
            .get_conversation_metadata(conversation_id, conversation_type)
            .ok()
            .flatten()
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

    /// Channels to list in the sidebar: MRU order, only Public/Private, and
    /// empty while anon mode hides regular channels. Single source of the
    /// sidebar-inclusion rule.
    pub fn sidebar_channels(&self) -> Vec<crate::domain::channel::Channel> {
        if self.anon_mode_enabled() {
            return Vec::new();
        }
        use crate::domain::channel::ChannelType;
        self.get_channels_mru()
            .into_iter()
            .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
            .collect()
    }

    /// The channel to select on load: first non-DM, non-SavedMessages channel
    /// in MRU order.
    pub fn first_selectable_channel_id(&self) -> Option<crate::domain::ids::ChannelId> {
        use crate::domain::channel::ChannelType;
        self.get_channels_mru()
            .iter()
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

    // ── Reconnect tuning façade (FFI surface for apps) ──────────────────
    //
    // Mirrors the pattern of `set_server_url` / `set_local_ai_enabled`:
    // a thin setter on the client that an FFI bridge can expose without
    // having to model the whole config struct.

    /// Get the currently-active reconnect tuning. Safe to call across threads.
    pub fn reconnect_config(&self) -> crate::network::ReconnectConfig {
        *self.reconnect_config.lock()
    }

    /// Replace the reconnect tuning. Takes effect on the next reconnect
    /// loop iteration for any component that reads this at the start of a
    /// cycle. Already-in-flight sleeps are not interrupted.
    pub fn set_reconnect_config(&self, cfg: crate::network::ReconnectConfig) {
        *self.reconnect_config.lock() = cfg;
    }

    /// Convenience wrapper for FFI bridges that can't easily pass the whole
    /// struct. Field-by-field tuning; any `None` argument leaves that field
    /// unchanged.
    ///
    /// # Arguments
    /// - `initial_delay_ms`: initial retry delay in milliseconds
    /// - `max_delay_ms`: upper cap on any retry delay
    /// - `backoff_multiplier`: multiplier applied after each failure (>= 1.0)
    /// - `jitter_ratio`: ±fraction added to each delay (0.0 = no jitter)
    /// - `max_attempts`: give up after this many failed attempts (0 = infinite)
    /// - `give_up_after_ms`: give up after this many ms of wall-clock (0 = infinite)
    pub fn tune_reconnect(
        &self,
        initial_delay_ms: Option<u64>,
        max_delay_ms: Option<u64>,
        backoff_multiplier: Option<f32>,
        jitter_ratio: Option<f32>,
        max_attempts: Option<u32>,
        give_up_after_ms: Option<u64>,
    ) {
        let mut guard = self.reconnect_config.lock();
        if let Some(v) = initial_delay_ms {
            guard.initial_delay = std::time::Duration::from_millis(v);
        }
        if let Some(v) = max_delay_ms {
            guard.max_delay = std::time::Duration::from_millis(v);
        }
        if let Some(v) = backoff_multiplier {
            guard.backoff_multiplier = v;
        }
        if let Some(v) = jitter_ratio {
            guard.jitter_ratio = v;
        }
        if let Some(v) = max_attempts {
            // 0 = sentinel for "infinite" — mirrors the Option<u32> internal rep.
            guard.max_attempts = if v == 0 { None } else { Some(v) };
        }
        if let Some(v) = give_up_after_ms {
            guard.give_up_after = if v == 0 {
                None
            } else {
                Some(std::time::Duration::from_millis(v))
            };
        }
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
    pub async fn do_logout(&self) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().do_logout().await
    }

    // ── State stores ────────────────────────────────────────────────────

    pub fn auth_state(&self) -> &AuthState { &self.services.auth_state }
    pub fn chat_state(&self) -> &ChatState { &self.services.chat_state }

    /// Resolve the current active conversation's `ConversationType`,
    /// looking up the channel row when the active conversation is a
    /// `Channel(id)` (its type depends on `require_encryption`). UI
    /// callers use this rather than
    /// `ActiveConversation::conversation_type(...)` directly so they
    /// don't need channel_repo access.
    pub fn active_conversation_type(&self) -> Option<crate::domain::message::ConversationType> {
        self.services
            .chat_state
            .get()
            .active_conversation
            .conversation_type(&self.services.channel_repo)
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
    // Pure presentation flag: when true, the UI hides server-friend DMs,
    // public/private channels, and the Friends/Saved entry points; only
    // anon DMs and anon groups remain visible. Not persisted by design —
    // every fresh client starts with anon_mode = false so privacy
    // properties auto-revert on restart.

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
        // that leaf media modules (gif/youtube/link_preview) gate on and that the
        // CDN reqwest callsites branch on. Keyed on the real transport, not the
        // request, so a failed onion switch leaves it false — no false anonymity.
        let on_onion = self.services.grpc_pool.transport_mode() == TransportMode::Onion;
        crate::network::anon_http::ANON_HTTP_ACTIVE.store(on_onion, Ordering::Release);

        // Drive the Tor UI indicator: entering onion = Connecting (until the
        // first onion RPC round-trips, when the poller flips it to Connected);
        // leaving onion = Off.
        crate::network::connection::set_tor_status(if on_onion {
            crate::network::connection::TorStatus::Connecting
        } else {
            crate::network::connection::TorStatus::Off
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

    /// Initialize the call service if not already set. Safe to call multiple times.
    #[cfg(feature = "media")]
    pub fn ensure_call_service(&self) {
        self.services.ensure_call_service()
    }

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

    /// Refresh prekeys and return the new available count.
    pub async fn refresh_prekeys(&self, count: u32) -> Result<u32, crate::error::ServiceError> {
        self.services.refresh_prekeys(count).await
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
    ) -> Vec<crate::proto::chat_v1::GifResult> {
        self.services.search_gifs(query, limit).await
    }

    /// Fetch trending GIFs.
    pub async fn trending_gifs(&self, limit: u32) -> Vec<crate::proto::chat_v1::GifResult> {
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

    /// Call-worker room-events policy listener. On `ParticipantLeft` in a
    /// channel-based group call, the initiator rotates the encryption key and
    /// redistributes it so the departed participant can't decrypt future
    /// frames. The "when" decision lives in core, not the UI crate.
    #[cfg(feature = "media")]
    fn spawn_call_room_policy_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            // Wait for the call worker to be initialized. init_call_service runs
            // before start() returns, but on Android it is deferred.
            let mut rx = loop {
                if let Some(w) = client.ctx.get_call_service() {
                    break w.subscribe_room_events();
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            };

            use crate::calls::LiveKitRoomEvent;
            loop {
                match rx.recv().await {
                    Ok(LiveKitRoomEvent::ParticipantLeft { .. }) => {
                        if let Err(e) = client.handle_participant_departed().await {
                            tracing::error!(error = %e, "Failed to handle participant departure");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "Call room-events policy lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(ClientError::AlreadyStarted);
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

        // ── Channel + DM services ───────────────────────────────────
        let h_channel_stream = self.ctx.start_channel_event_stream();
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

        // ── PreKey replenishment (runs initial check immediately) ────
        let h_prekey = self.services.spawn_prekey_replenishment();

        // ── DM mailbox key rolling (L4 anonymity rotation) ───────────
        let h_key_rolling = self.services.spawn_key_rolling();

        // ── Presence ────────────────────────────────────────────────
        self.set_presence(crate::domain::user::PresenceStatus::Online).await;
        self.touch_activity();

        // ── Store handles for shutdown ───────────────────────────────
        let mut tasks = self.tasks.lock();
        tasks.extend([h_events, h_inbox, h_skdm_retry, h_channel_stream, h_channel_bootstrap, h_dm_bootstrap, h_ai, h_prekey, h_key_rolling]);
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

        let result = self
            .send_message(conversation, text, pending_attachments, quote, options)
            .await?;

        let real_message_id = result.message_id_string();
        let (message_id_effective, delivery_status) = if result.bootstrap_initiated {
            (format!("pending-{}", result.sent_at), "sending")
        } else {
            (real_message_id, "sent")
        };

        let timestamp_display = chrono::DateTime::from_timestamp_nanos(result.sent_at as i64)
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();

        // "You: {trim(text, 50)}" — same shape as get_dm_sidebar_preview for
        // receive-side entries.
        let sidebar_preview = format!("You: {}", truncate_preview_text(text, 50));

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
        })
    }

    /// Get a [`ConversationHandle`] for per-conversation operations.
    pub fn conversation(self: &Arc<Self>, conv: ActiveConversation) -> ConversationHandle {
        ConversationHandle::new(conv, self.clone())
    }

    /// Pin or unpin a conversation in the conversation list.
    pub fn pin_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
        pinned: bool,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).pin_conversation(pinned)
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
    /// Pass `0` to disable. The value is in seconds.
    pub fn set_disappearing_timer(
        self: &Arc<Self>,
        conv: ActiveConversation,
        seconds: u32,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).set_disappearing_timer(seconds)
    }

    // ── Drafts (façade) ─────────────────────────────────────────────

    /// Save or update a draft for a conversation root (no thread).
    pub fn save_draft(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        content: String,
    ) -> Result<crate::domain::draft::Draft, crate::services::draft_service::DraftError> {
        self.services.draft_service.save_draft(channel_id, None, content, None, vec![])
    }

    /// Get the root-level draft for a channel (or None).
    pub fn get_draft(
        &self,
        channel_id: crate::domain::ids::ChannelId,
    ) -> Result<Option<crate::domain::draft::Draft>, crate::services::draft_service::DraftError> {
        self.services.draft_service.get_draft(channel_id, None)
    }

    /// Delete the root-level draft for a channel.
    pub fn delete_draft(
        &self,
        channel_id: crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::draft_service::DraftError> {
        self.services.draft_service.delete_draft(channel_id, None)
    }

    /// List every draft for the current user across all conversations.
    pub fn list_all_drafts(
        &self,
    ) -> Result<Vec<crate::domain::draft::Draft>, crate::services::draft_service::DraftError> {
        self.services.draft_service.list_all_drafts()
    }

    /// List every draft pre-joined with the owning conversation's display
    /// name + conversation type. Keeps the join + fallback logic on the
    /// client side rather than pushing it into each UI platform (Android,
    /// desktop, iOS).
    pub fn list_drafts_with_conversation_names(
        &self,
    ) -> Result<Vec<crate::domain::draft::DraftWithConversation>, crate::services::draft_service::DraftError> {
        use std::collections::HashMap;
        let drafts = self.services.draft_service.list_all_drafts()?;
        let conversations = self
            .services
            .conversation_repo
            .list_all()
            .map_err(crate::services::draft_service::DraftError::Storage)?;
        let lookup: HashMap<String, (String, crate::domain::message::ConversationType)> =
            conversations
                .into_iter()
                .map(|c| (c.id, (c.name, c.conversation_type)))
                .collect();
        Ok(drafts
            .into_iter()
            .map(|d| {
                let (name, ty) = lookup
                    .get(&d.key.channel_id.to_string())
                    .cloned()
                    .unwrap_or_else(|| {
                        (
                            "(unknown conversation)".to_string(),
                            crate::domain::message::ConversationType::Channel,
                        )
                    });
                crate::domain::draft::DraftWithConversation {
                    draft: d,
                    conversation_name: name,
                    conversation_type: ty,
                }
            })
            .collect())
    }

    // ── Conversation listing ────────────────────────────────────────

    /// List all channels the user is a member of.
    pub fn list_channels(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().get_channels_for_user()
    }

    /// Unified conversation rollup — channels, DMs, anon DMs, anon groups —
    /// used by cross-conversation screens (drafts list, etc). Returns
    /// `UnifiedConversation` rows with a `conversation_type` field already
    /// classified.
    pub fn list_all_conversations(
        &self,
    ) -> Result<Vec<crate::domain::UnifiedConversation>, crate::error::StorageError> {
        self.services.conversation_repo.list_all()
    }

    /// Total unread count across all conversation types (channels, server/mailbox
    /// DMs, anon DMs, anon groups). Used to drive sidebar/mobile badge totals.
    ///
    /// Aggregation lives here (not in the UI) so the UI doesn't have to walk
    /// per-bucket Slint models — it gets one authoritative number. Counts come
    /// from `is_read = 0` rows in the unified messages table.
    ///
    /// Returns 0 on storage error rather than propagating, since a missing
    /// badge count is preferable to a UI hard-fail on every recompute.
    pub fn total_unread_count(&self) -> u32 {
        self.services
            .conversation_repo
            .list_all()
            .map(|convs| convs.iter().map(|c| c.unread_count).sum())
            .unwrap_or(0)
    }

    /// List channel members with profile information.
    pub fn list_members_with_profiles(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<Vec<crate::domain::channel::MemberWithProfile>, crate::services::channel::ChannelError> {
        self.channels().list_members_with_profiles(channel_id)
    }

    /// List all anonymous DM conversations.
    pub fn list_anon_dms(&self) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_conversations()
    }

    /// List pending anon DM message requests (incoming).
    pub fn list_anon_dm_requests(&self) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_message_requests()
    }

    // ── Friend / social operations ──────────────────────────────────

    /// Send a friend request.
    pub async fn send_friend_request(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
        note: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::user::UserError> {
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
    /// profile for the Received-requests tab. Fetches our identity private key
    /// (the facade's only signal touch) and delegates the protocol work —
    /// ECIES decrypt, profile-key persistence, profile decrypt — to
    /// [`UserService::ingest_received_friend_request`].
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &crate::domain::ids::UserId,
        ciphertext: &[u8],
    ) -> ReceivedFriendRequestProfile {
        use zeroize::Zeroize;

        let mut secret = match self.signal().actor.get_identity_private_key().await {
            Ok(s) => s,
            Err(_) => return ReceivedFriendRequestProfile::default(),
        };
        let out = self
            .users()
            .ingest_received_friend_request(sender_id, ciphertext, &secret)
            .await;
        secret.zeroize();
        out
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

    /// Update the current user's profile.
    pub async fn update_profile(
        &self,
        display_name: Option<String>,
        status: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().update_user_profile(display_name, status).await
    }

    /// Decode + crop + resize + WebP-encode raw image bytes into a normalised
    /// avatar. Pure delegation to [`crate::media::ImageProcessor::process_avatar_from_memory`]
    /// — exposed on the client so the FFI bridge can drive it without
    /// importing internal modules.
    ///
    /// Output is byte-identical regardless of which client calls this, so
    /// Android, Linux, and any future client all produce the same processed
    /// bytes from the same source image.
    pub fn process_avatar(
        &self,
        raw: &[u8],
    ) -> Result<Vec<u8>, crate::error::MediaError> {
        crate::media::ImageProcessor::process_avatar_from_memory(raw)
    }

    /// Store the user's avatar bytes locally (never uploaded).
    pub fn set_avatar_bytes(&self, bytes: &[u8]) -> Result<(), crate::services::auth_service::AuthError> {
        self.anon_dms()
            .set_my_avatar_bytes(bytes)
            .map_err(|e| crate::services::auth_service::AuthError::InvalidInput(e.to_string()))
    }

    /// Update the user's avatar AND broadcast it to every active anon DM.
    pub async fn update_avatar_and_broadcast(
        &self,
        bytes: &[u8],
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.anon_dms()
            .update_avatar_and_broadcast(bytes)
            .await
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
        self.users().lookup_by_username(username).await
    }

    /// Get a user's profile.
    pub async fn get_user_profile(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().get_user_profile(user_id).await
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
            .get_user_local(&user_id)
            .map(|u| u.username)
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
                // Resolve from the channel row — `Channel::conversation_type()`
                // returns `AnonGroup` for encrypted channels and `Channel` for
                // plain ones. Send-path stores under the same value, so a
                // hardcoded `AnonGroup` here misses every public/unencrypted
                // channel's locally-stored messages and the chat reads empty.
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
            ActiveConversation::MailboxDm { channel_id: id, .. } => {
                self.messages().get_messages_local(id, &ConversationType::MailboxDm, None, limit)
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

    /// Get messages in a thread.
    pub async fn get_thread_messages(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        thread_root_id: &crate::domain::ids::MessageId,
        limit: u32,
        before: Option<&crate::domain::ids::MessageId>,
    ) -> Result<Vec<Message>, ClientError> {
        let mut msg_client = self.services.grpc_pool.message_client();
        self.messages()
            .get_thread_messages(&mut msg_client, channel_id, thread_root_id, limit, before)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Message forwarding ─────────────────────────────────────────

    /// Forward a message to another channel.
    pub async fn forward_message(
        &self,
        source_message_id: &crate::domain::ids::MessageId,
        target_channel_id: &crate::domain::ids::ChannelId,
        comment: Option<&str>,
    ) -> Result<Message, ClientError> {
        let mut msg_client = self.services.grpc_pool.message_client();
        let mut auth_client = self.services.grpc_pool.auth_client();
        let mut ch_client = self.services.grpc_pool.channel_client();
        self.messages()
            .forward_message(&mut msg_client, &mut auth_client, &mut ch_client, source_message_id, target_channel_id, comment)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Device management ─────────────────────────────────────────

    /// List all linked devices for the current user.
    pub async fn list_devices(&self) -> Result<Vec<crate::services::device_service::Device>, ClientError> {
        let mut auth_client = self.services.grpc_pool.auth_client();
        let response = auth_client.list_devices().await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        let mut devices: Vec<crate::services::device_service::Device> = response
            .devices
            .into_iter()
            .map(|d| crate::services::device_service::Device {
                device_id: d.device_id,
                name: d.device_name,
                info: d.device_info,
                created_at: chrono::DateTime::from_timestamp_millis(d.created_at as i64)
                    .unwrap_or(chrono::DateTime::UNIX_EPOCH),
                last_active: chrono::DateTime::from_timestamp_millis(d.last_active as i64)
                    .unwrap_or(chrono::DateTime::UNIX_EPOCH),
                is_primary: d.is_primary,
                is_current: d.is_current,
            })
            .collect();

        devices.sort_by_key(|d| d.device_id);
        Ok(devices)
    }

    /// Unlink (revoke) a specific device.
    ///
    /// After a successful revoke, every encrypted channel we own is marked for
    /// sender-key rotation so the revoked device's cached sender key cannot be
    /// used to read messages going forward. Rotation happens on next send per
    /// channel (new SenderKey generated → SKDM redistributed to remaining
    /// members' remaining devices, never to the revoked device).
    pub async fn unlink_device(&self, device_id: u32) -> Result<(), ClientError> {
        let mut auth_client = self.services.grpc_pool.auth_client();
        self.devices()
            .revoke_and_rotate(&mut auth_client, device_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Unlink all devices except the current one.
    pub async fn unlink_all_other_devices(&self) -> Result<(), ClientError> {
        let devices = self.list_devices().await?;
        let current_device_id = self.sessions().current_device_id();

        for device in devices {
            if device.is_current || Some(device.device_id) == current_device_id {
                continue;
            }
            if device.is_primary {
                continue; // Cannot revoke primary device
            }
            if let Err(e) = self.unlink_device(device.device_id).await {
                tracing::warn!(device_id = device.device_id, error = %e, "Failed to unlink device");
            }
        }

        Ok(())
    }

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

    /// Handle remote hangup: store history, disconnect locally. Accepts the
    /// hangup's `call_id` so core can gate on active-call match without the
    /// UI having to inspect call state.
    #[cfg(feature = "media")]
    pub async fn handle_remote_hangup(
        &self,
        call_id: &str,
        duration_secs: i64,
    ) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .handle_remote_hangup(call_worker, duration_secs, Some(call_id))
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

    /// Enable or disable sending read receipts.
    pub fn set_read_receipts_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions().set_bool_setting(settings_keys::SEND_READ_RECEIPTS, enabled)
    }

    /// Enable or disable typing indicators.
    pub fn set_typing_indicators_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions().set_bool_setting(settings_keys::SHOW_TYPING_INDICATORS, enabled)
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

    pub fn get_pinned_messages_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<Vec<crate::domain::Message>, crate::services::message::MessageError> {
        self.services.message_service.get_pinned_messages_local(channel_id, conversation_type)
    }

    pub fn get_anon_dm_display_name(&self) -> Option<String> {
        self.services.anon_dm_service.get_my_display_name()
    }

    pub async fn start_anon_dm_private(
        &self,
        code: &crate::domain::PrivateFriendCode,
        message: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        self.services.anon_dm_service
            .start_conversation_private(code, message, display_name)
            .await
    }

    pub fn has_attachment_locally(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> bool {
        self.services.attachment_service.has_local_content(attachment_id)
    }

    pub fn set_notification_detail_level(
        &self,
        level: crate::services::notification_service::NotificationDetailLevel,
    ) {
        self.services.notification_service.set_detail_level(level)
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

    pub fn get_peer_identity(
        &self,
        peer_id: &str,
    ) -> Result<Option<Vec<u8>>, crate::error::StorageError> {
        self.services.signal_service.get_peer_identity(peer_id)
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
        ActiveConversation::MailboxDm { channel_id, peer_id } => Ok(OutgoingIntent::ChannelDm {
            peer_id, dm_channel_id: channel_id, text: text.to_string(),
            attachment_pointers, file_paths: vec![], quote,
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

#[cfg(test)]
mod upload_attachments_tests {
    //! Regression test for the "must have text or attachments" mask: a
    //! failed pending-attachment upload must propagate its `AttachmentError`
    //! instead of being dropped, which previously left `pointers` empty and
    //! sent attachments-only messages into the empty-content validation path.
    use super::*;
    use crate::crypto::attachment_crypto::{max_attachment_size, set_max_attachment_size};
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
        let _guard = crate::crypto::attachment_crypto::SIZE_CAP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = max_attachment_size();
        set_max_attachment_size(100);

        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 200];
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), &payload).unwrap();

        let pending = vec![pending_for(input.path().to_path_buf(), payload.len() as u64)];
        let result = upload_attachments(&pending, &svc).await;

        set_max_attachment_size(original);

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

    fn build_test_client() -> Arc<ChatterClient> {
        // These tests build a real ChatterClient, which stores the DB passphrase
        // in the OS keyring. A headless/CI session has no Secret Service, so point
        // keyring at its in-memory mock backend (process-global, set once) — the
        // tests exercise transport-switch bookkeeping, not credential storage.
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });

        let dir = tempfile::tempdir().unwrap();
        ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client")
    }

    /// Entering anon mode must fire the ntfy push shutdown and clear the slot
    /// (product decision: push OFF while anon, poll-only). Idempotent: a second
    /// enter with no listener is a clean no-op.
    #[tokio::test]
    async fn anon_enter_suspends_push_and_is_idempotent() {
        let client = build_test_client();

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
        let client = build_test_client();
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
        let client = build_test_client();
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
        let client = build_test_client();
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
        let client = build_test_client();
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
}
