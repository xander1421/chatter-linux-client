//! SDK facade for chatter-core.
//!
//! [`ChatterClient`] is the main entry point for consumers of this library.
//! It wraps the internal [`AppContext`] and [`CoreServices`] with a clean,
//! discoverable API.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::Message;
use crate::error::{CryptoError, NetworkError};
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::draft_service::DraftService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::gateway::shared::truncate_preview;
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::message_gateway::MessageGateway;
use crate::services::pin_service::PinService;
use crate::services::reaction_service::ReactionService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    PinnedState, PresenceState, PrivacyState, StreamingState, UIState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;

/// Builder for [`ChatterClient`].
#[derive(Default)]
pub struct ClientBuilder {
    server_url: Option<String>,
    data_dir: Option<String>,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self { server_url: None, data_dir: None }
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

    /// Build the client. Initializes database, crypto, and network pool.
    ///
    /// Must be called within a tokio runtime (e.g. inside `#[tokio::main]`
    /// or after `Runtime::new()`).
    pub fn build(self) -> Result<Arc<ChatterClient>, ClientError> {
        let ctx = AppContext::with_config(self.server_url, self.data_dir)?;
        let ui = ctx.core_services();
        Ok(Arc::new(ChatterClient {
            ctx,
            ui,
            tasks: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(0),
            call_signaling: std::sync::OnceLock::new(),
            svr_backup_service: std::sync::OnceLock::new(),
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
    ui: Arc<CoreServices>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    started: AtomicBool,
    /// Epoch millis of last user activity (for idle detection).
    last_activity_ms: AtomicU64,
    /// Cached call signaling service (persistent for pending state).
    call_signaling: std::sync::OnceLock<crate::services::call_signaling::CallSignalingService>,
    /// Cached SVR backup service.
    svr_backup_service: std::sync::OnceLock<crate::services::svr_backup_service::SvrBackupService>,
}

impl ChatterClient {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    // ── Services (return &Arc<T> so .clone() yields Arc<T>) ─────────────

    pub fn auth(&self) -> &Arc<AuthService> { &self.ui.auth_service }
    pub fn messages(&self) -> &Arc<MessageService> { &self.ui.message_service }
    pub fn message_gateway(&self) -> &Arc<MessageGateway> { &self.ui.message_gateway }
    pub fn channels(&self) -> &Arc<ChannelService> { &self.ui.channel_service }
    pub fn users(&self) -> &Arc<UserService> { &self.ui.user_service }
    pub fn attachments(&self) -> &Arc<AttachmentService> { &self.ui.attachment_service }
    pub fn reactions(&self) -> &Arc<ReactionService> { &self.ui.reaction_service }
    pub fn typing(&self) -> &Arc<TypingService> { &self.ui.typing_service }
    pub fn pins(&self) -> &Arc<PinService> { &self.ui.pin_service }
    pub fn drafts(&self) -> &Arc<DraftService> { &self.ui.draft_service }
    pub fn sessions(&self) -> &Arc<SessionService> { &self.ui.session_service }
    pub fn anon_dms(&self) -> &Arc<AnonDmService> { &self.ui.anon_dm_service }
    pub fn anon_groups(&self) -> &Arc<AnonGroupService> { &self.ui.anon_group_service }
    pub fn signal(&self) -> &Arc<SignalService> { &self.ui.signal_service }
    pub fn ai(&self) -> &Arc<AIService> { &self.ui.ai_service }
    pub fn jarvis(&self) -> &Arc<JarvisService> { &self.ui.jarvis_service }
    pub fn mailbox_manager(&self) -> &Arc<MailboxManager> { &self.ui.mailbox_manager }
    pub fn local_ai(&self) -> &Arc<LocalAIService> { &self.ui.local_ai_service }
    pub fn dm_bootstrap(&self) -> &Arc<DmBootstrapService> { &self.ui.dm_bootstrap_service }
    pub fn channel_events(&self) -> &Arc<ChannelEventService> { &self.ui.channel_event_service }
    pub fn notifications(&self) -> &Arc<crate::services::notification_service::NotificationService> { &self.ui.notification_service }

    /// Get the SVR backup service (lazy-initialized, persistent instance).
    pub fn svr_backup(&self) -> &crate::services::svr_backup_service::SvrBackupService {
        self.svr_backup_service.get_or_init(|| {
            let settings = Arc::new(
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    self.ui.database.clone(),
                ),
            );
            crate::services::svr_backup_service::SvrBackupService::new(
                self.ui.grpc_pool.clone(),
                settings,
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
            )
        })
    }

    // ── DM Call Orchestration ──────────────────────────────────────────

    /// Start a DM call: generate ephemeral key, create call on server, send offer.
    ///
    /// Returns the call ID. The UI should show "Calling..." and wait for
    /// `complete_dm_call()` when the peer answers.
    pub async fn start_dm_call(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::ids::CallId, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .start_dm_call(peer_id, channel_id, call_type, &call_worker, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start a group call in a channel: generate key, initiate on server, distribute key.
    ///
    /// Returns a `CallSetupResult` with call_id and verification emojis for the UI.
    pub async fn start_group_call(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        use crate::crypto::generate_group_call_key;
        use crate::services::call_verification::CallVerification;

        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let group_key = generate_group_call_key();

        let call_type_str = match call_type {
            crate::domain::call::CallType::Voice => "Voice Call",
            crate::domain::call::CallType::Video => "Video Call",
            crate::domain::call::CallType::Group => "Group Call",
        };

        let call = call_worker
            .initiate_call(channel_id, call_type_str, call_type, &group_key, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        // Publish microphone immediately
        if let Err(e) = call_worker.publish_microphone(None).await {
            tracing::error!("Failed to publish microphone: {}", e);
        }

        // Distribute key to all channel members (fire-and-forget)
        let call_id_str = call.id.to_string();
        let signaling = self.call_signaling().clone();
        let ch_id = channel_id;
        tokio::spawn(async move {
            match signaling.distribute_group_call_key(&ch_id, &call_id_str, group_key, 0).await {
                Ok(count) => tracing::info!(call_id = %call_id_str, sent = count, "Group call key distributed"),
                Err(e) => tracing::error!(call_id = %call_id_str, error = %e, "Failed to distribute group call key"),
            }
        });

        let verification = CallVerification::derive_verification_emojis(&group_key, &call.id.to_string());
        let emojis = CallVerification::to_slint_tuple(&verification);

        Ok(crate::domain::call::CallSetupResult {
            call_id: call.id,
            channel_id,
            verification_emojis: [emojis.0, emojis.1, emojis.2, emojis.3],
            is_video: matches!(call_type, crate::domain::call::CallType::Video),
        })
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Returns verification data (emojis) for the UI.
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .complete_dm_call(
                peer_ephemeral_pk,
                call_id,
                peer_id,
                self.signal(),
                &call_worker,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, join call.
    ///
    /// Returns verification data (emojis) for the UI.
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_dm_call(
                caller_ephemeral_pk,
                call_id,
                caller_id,
                self.signal(),
                &call_worker,
                user_id,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Anonymous Call Orchestration ─────────────────────────────────

    /// Start an anonymous DM call: acquire token, send offer, store pending state.
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &crate::domain::anon_dm::AnonDmId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_dm_call(anon_dm_id, is_video, display_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    pub async fn start_anon_group_call(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_group_call(group_id, is_video, display_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming anonymous call: acquire token, send accept, join room, connect LiveKit.
    pub async fn accept_anon_call(
        &self,
        room_name: &str,
    ) -> Result<crate::domain::call::AnonCallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_anon_call(room_name, &call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Handle an anonymous call answer (caller side): connect to LiveKit if accepted.
    pub async fn handle_anon_call_answer(
        &self,
        call_room_id: &[u8],
        accepted: bool,
        callee_display_name: &str,
    ) -> Result<Option<crate::domain::call::AnonCallSetupResult>, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .handle_anon_call_answer(call_room_id, accepted, callee_display_name, &call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Decline an incoming anonymous call: send decline answer, store missed call history.
    pub async fn decline_anon_call(
        &self,
        room_name: &str,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .decline_anon_call(room_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
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
        let local_user_id = self.ui.auth_state.get().current_user_id
            .as_ref()
            .ok_or_else(|| ClientError::Validation("Not logged in".into()))?
            .to_string();

        let safety_number = self.ui.signal_service
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
        self.ui.signal_service
            .is_peer_verified(&peer_user_id.to_string())
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// Mark a peer as verified with their current safety number.
    pub async fn mark_peer_verified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        let safety_number = self.get_safety_number(peer_user_id).await?;
        self.ui.signal_service
            .set_peer_verified(&peer_user_id.to_string(), &safety_number, true)
            .map_err(|e| ClientError::Validation(e.to_string()))
    }

    /// Mark a peer as unverified.
    pub fn mark_peer_unverified(
        &self,
        peer_user_id: &crate::domain::ids::UserId,
    ) -> Result<(), ClientError> {
        // Get existing safety number to preserve it
        if let Ok(Some((safety_number, _))) = self.ui.signal_service.key_store
            .get_peer_verification(&peer_user_id.to_string())
        {
            self.ui.signal_service
                .set_peer_verified(&peer_user_id.to_string(), &safety_number, false)
                .map_err(|e| ClientError::Validation(e.to_string()))?;
        }
        Ok(())
    }

    // ── Events ──────────────────────────────────────────────────────────

    pub fn event_bus(&self) -> &EventBus { &self.ui.event_bus }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<DomainEvent> {
        self.ui.event_bus.subscribe_domain()
    }

    // ── State stores ────────────────────────────────────────────────────

    pub fn auth_state(&self) -> &AuthState { &self.ui.auth_state }
    pub fn chat_state(&self) -> &ChatState { &self.ui.chat_state }
    pub fn message_state(&self) -> &MessageState { &self.ui.message_state }
    pub fn user_state(&self) -> &UserState { &self.ui.user_state }
    pub fn presence_state(&self) -> &PresenceState { &self.ui.presence_state }
    pub fn ui_state(&self) -> &UIState { &self.ui.ui_state }
    pub fn offline_state(&self) -> &OfflineState { &self.ui.offline_state }
    pub fn streaming_state(&self) -> &StreamingState { &self.ui.streaming_state }
    pub fn pinned_state(&self) -> &PinnedState { &self.ui.pinned_state }
    pub fn privacy_state(&self) -> &PrivacyState { &self.ui.privacy_state }
    pub fn jarvis_state(&self) -> &JarvisState { &self.ui.jarvis_state }

    // ── Infrastructure ──────────────────────────────────────────────────

    pub fn runtime_handle(&self) -> &tokio::runtime::Handle { &self.ui.runtime_handle }
    pub fn directories(&self) -> &Directories { &self.ui.directories }

    /// Post-boot call service handle (OnceLock).
    pub fn call_service(&self) -> &Arc<std::sync::OnceLock<CallWorkerHandle>> {
        &self.ui.call_service
    }

    /// Post-boot group fanout (OnceLock).
    pub fn group_fanout(&self) -> &Arc<std::sync::OnceLock<Arc<GroupFanout>>> {
        &self.ui.group_fanout
    }

    /// Post-boot device inbox provider (OnceLock).
    pub fn device_inbox_provider(&self) -> &Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>> {
        &self.ui.device_inbox_provider
    }

    // ── Delegated CoreServices methods ─────────────────────────────────────

    /// Initialize the call service if not already set. Safe to call multiple times.
    pub fn ensure_call_service(&self) {
        self.ui.ensure_call_service()
    }

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.ui.is_connected()
    }

    /// Start the inbox provider and return a receiver for raw inbox events.
    pub async fn start_inbox_provider(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::inbox::InboxEvent>> {
        self.ui.start_inbox_provider().await
    }

    /// Refresh prekeys and return the new available count.
    pub async fn refresh_prekeys(&self, count: u32) -> Result<u32, crate::error::ServiceError> {
        self.ui.refresh_prekeys(count).await
    }

    /// Pre-fetch anonymous tokens for sealed sender messaging.
    pub async fn prefetch_tokens(
        &self,
        target_count: u32,
    ) -> Result<u32, crate::error::MailboxError> {
        self.ui.prefetch_tokens(target_count).await
    }

    /// Decline an incoming call.
    pub async fn decline_call(&self, call_id: &str) -> Result<(), NetworkError> {
        self.ui.decline_call(call_id).await
    }

    /// Search GIFs.
    pub async fn search_gifs(
        &self,
        query: &str,
        limit: u32,
    ) -> Vec<crate::proto::chat_v1::GifResult> {
        self.ui.search_gifs(query, limit).await
    }

    /// Fetch trending GIFs.
    pub async fn trending_gifs(&self, limit: u32) -> Vec<crate::proto::chat_v1::GifResult> {
        self.ui.trending_gifs(limit).await
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Connect to the server (establish gRPC channel).
    ///
    /// The gRPC pool is created at `build()` but connects lazily.
    /// This forces an immediate connection attempt to verify reachability.
    /// Optional — RPCs will auto-connect on first use if skipped.
    pub async fn connect(&self) -> Result<(), ClientError> {
        self.ui.auth_service.connect().await?;
        Ok(())
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
    pub async fn start(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(ClientError::AlreadyStarted);
        }

        // ── Infrastructure init ─────────────────────────────────────
        self.ctx.init_device_inbox_provider();
        self.ctx.init_group_fanout();
        self.ctx.init_call_service();

        // ── Event handling ──────────────────────────────────────────
        let h_events = self.ctx.spawn_event_handlers();

        // ── Inbox polling + protocol processor ──────────────────────
        let inbox_rx = match self.ui.start_inbox_provider().await {
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

        // ── Presence ────────────────────────────────────────────────
        self.set_presence(crate::proto::UserPresence::Online).await;
        self.touch_activity();

        // ── Store handles for shutdown ───────────────────────────────
        let mut tasks = self.tasks.lock();
        tasks.extend([h_events, h_inbox, h_channel_stream, h_channel_bootstrap, h_dm_bootstrap, jarvis.maintenance, h_ai]);
        if let Some(observer) = jarvis.observer {
            tasks.push(observer);
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
        let uploaded = upload_attachments(pending_attachments, self.attachments()).await;
        let quote = if let Some(mut q) = quote {
            if q.preview_text.is_none() && !conversation.is_anon() {
                q.preview_text = self.messages()
                    .get_by_sender_timestamp(&q.author_id, q.timestamp)
                    .ok().flatten()
                    .and_then(|m| m.as_text().map(|t| t.to_string()))
                    .map(|t| truncate_preview(&t));
            }
            Some(q)
        } else { None };
        let intent = build_intent(conversation, text, uploaded.pointers, quote, &options.unwrap_or_default(), self.channels())?;
        let services = OwnedOutgoingServices::from_client(self);
        route_outgoing_owned(intent, &services).await
    }

    /// Get a [`ConversationHandle`] for per-conversation operations.
    pub fn conversation(self: &Arc<Self>, conv: ActiveConversation) -> ConversationHandle {
        ConversationHandle::new(conv, self.clone())
    }

    // ── Auth (high-level) ───────────────────────────────────────────

    /// Log in and start all background services.
    ///
    /// This is the one-call entry point: authenticates, sets auth state,
    /// and starts inbox polling + event handlers.
    ///
    /// If `pin` is provided and this is a new device (no local identity keys),
    /// the method attempts to restore keys from an SVR backup. The result
    /// fields `keys_restored` / `keys_missing` indicate the outcome.
    pub async fn login(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        let mut result = self.auth().do_login(username, password, device_name, pin).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );

        // Attempt SVR key restore if this is a new device (no local identity keys)
        if let Some(p) = pin {
            self.attempt_svr_restore(p, username, &mut result).await;
        }

        self.start().await?;
        Ok(result)
    }

    /// Register a new account and start all background services.
    ///
    /// PIN must be exactly 6 ASCII digits if provided.
    pub async fn register(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        if let Some(p) = pin {
            if p.len() != 6 || !p.chars().all(|c| c.is_ascii_digit()) {
                return Err(ClientError::Validation("PIN must be exactly 6 digits".into()));
            }
        }
        let result = self.auth().do_register(
            username, password, display_name, device_name, avatar, pin,
        ).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );

        // Create SVR backup after successful registration (non-blocking on failure)
        if let Some(p) = pin {
            self.perform_svr_backup(p, username).await;
        }

        self.start().await?;
        Ok(result)
    }

    /// Use a one-time recovery code to reset password and SVR credentials.
    ///
    /// This is called when the user has lost their password but has a recovery code
    /// from registration. It resets the password and SVR key material on the server.
    ///
    /// After this succeeds, the user should login with `new_password` + `new_pin`.
    /// The SVR backup will be created automatically during that login flow.
    ///
    /// # Arguments
    ///
    /// * `username` - The plaintext username (will be hashed before sending)
    /// * `recovery_code` - 8-character alphanumeric recovery code (A-Z, 2-9)
    /// * `new_password` - The new password to set
    /// * `new_pin` - The new 6-digit PIN for SVR key derivation
    pub async fn use_recovery_code(
        &self,
        username: &str,
        recovery_code: &str,
        new_password: &str,
        new_pin: &str,
    ) -> Result<RecoveryResult, ClientError> {
        use sha2::{Digest, Sha256};
        use zeroize::Zeroize;

        // Validate new_pin is exactly 6 digits
        if new_pin.len() != 6 || !new_pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(ClientError::Validation("PIN must be exactly 6 digits".into()));
        }

        // Validate recovery_code is 8 characters (A-Z, 2-9 per proto pattern)
        if recovery_code.len() != 8 || !recovery_code.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(ClientError::Validation(
                "Recovery code must be exactly 8 alphanumeric characters".into(),
            ));
        }

        // Hash new password with SHA256 (matching existing auth pattern)
        let mut password_hash = Sha256::digest(new_password.as_bytes()).to_vec();

        // Derive new SVR keypair from new_pin + username
        let keypair = crate::crypto::svr::SvrKeypair::new_from_pin(new_pin, username)
            .map_err(|e| ClientError::Auth(AuthError::Crypto(e)))?;
        let svr_public_key = keypair.public_key_bytes();

        // Derive new registration lock from new_pin + username
        let mut registration_lock = crate::crypto::svr::derive_registration_lock(new_pin, username)
            .map_err(|e| ClientError::Auth(AuthError::Crypto(e)))?;

        // Hash username for privacy (need profile key derived from NEW password)
        // Use the same derivation as auth_service: HKDF(password_hash, username) -> profile_key
        // Then HMAC-SHA256(profile_key, normalized_username) -> username_hash
        let username_hash = {
            use crate::network::grpc::auth_client::hash_username;
            use hkdf::Hkdf;

            let hk = Hkdf::<Sha256>::new(None, &password_hash);
            let info = format!("chatter_profile_key_{}", username.trim().to_lowercase());
            let mut profile_key = [0u8; 32];
            hk.expand(info.as_bytes(), &mut profile_key)
                .map_err(|_| ClientError::Auth(AuthError::Crypto(
                    CryptoError::KeyDerivationFailed("HKDF expand failed for profile key".into())
                )))?;

            let hash = hash_username(username, &profile_key);
            profile_key.zeroize();
            hash
        };

        // Get auth client from gRPC pool
        let mut auth_client = self.auth().try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;

        // Call server RPC
        let result = auth_client
            .use_recovery_code(
                &username_hash,
                recovery_code,
                &password_hash,
                &svr_public_key,
                &registration_lock,
            )
            .await;

        // SECURITY: Zeroize sensitive intermediates immediately (both success and error paths)
        password_hash.zeroize();
        registration_lock.zeroize();

        let remaining_codes = result.map_err(|e| ClientError::Connection(e))?;

        Ok(RecoveryResult { remaining_codes })
    }

    /// Regenerate recovery codes for the authenticated user.
    ///
    /// Deletes all existing codes and generates 8 new ones.
    /// Requires password confirmation for security.
    ///
    /// # Arguments
    ///
    /// * `password` - The user's current password (will be hashed before sending)
    pub async fn regenerate_recovery_codes(&self, password: &str) -> Result<Vec<String>, ClientError> {
        use sha2::{Digest, Sha256};
        use zeroize::Zeroize;

        let mut password_hash = Sha256::digest(password.as_bytes()).to_vec();

        let mut auth_client = self.auth().try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;

        let result = auth_client
            .regenerate_recovery_codes(&password_hash)
            .await;

        // SECURITY: Zeroize password hash immediately
        password_hash.zeroize();

        let codes = result.map_err(|e| ClientError::Connection(e))?;
        Ok(codes)
    }

    /// Create an SVR encrypted backup of identity keys and profile key.
    ///
    /// Called after registration to back up key material for future device recovery.
    /// Failures are logged but never block the caller -- registration/login success
    /// is more important than backup success.
    async fn perform_svr_backup(&self, pin: &str, username: &str) {
        use crate::crypto::svr::derive_svr_secret;
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        // 1. Derive svr_secret transiently from PIN + username
        let mut svr_secret = match derive_svr_secret(pin, username) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("SVR backup skipped: failed to derive svr_secret: {}", e);
                return;
            }
        };

        // 2. Get identity keypair bytes (32 private + 32 public)
        let private_bytes = match self.signal().actor.get_identity_private_key().await {
            Ok(b) => b,
            Err(e) => {
                svr_secret.zeroize();
                tracing::warn!("SVR backup skipped: failed to get identity private key: {}", e);
                return;
            }
        };
        let public_bytes_33 = match self.signal().actor.get_identity_public_key().await {
            Ok(b) => b,
            Err(e) => {
                svr_secret.zeroize();
                tracing::warn!("SVR backup skipped: failed to get identity public key: {}", e);
                return;
            }
        };
        // Strip the 0x05 type prefix to get raw 32-byte Curve25519 public key
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);

        // 3. Get profile key from settings
        let mut profile_key = match self.signal().settings_repo.get_bytes(keys::PROFILE_KEY) {
            Ok(Some(bytes)) if bytes.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&bytes);
                pk
            }
            Ok(_) => {
                svr_secret.zeroize();
                identity_keypair.zeroize();
                tracing::warn!("SVR backup skipped: profile key not found or invalid length");
                return;
            }
            Err(e) => {
                svr_secret.zeroize();
                identity_keypair.zeroize();
                tracing::warn!("SVR backup skipped: failed to read profile key: {}", e);
                return;
            }
        };

        // 4. Upload encrypted backup
        match self
            .svr_backup()
            .create_and_upload_backup(&svr_secret, &identity_keypair, &profile_key)
            .await
        {
            Ok(version) => {
                tracing::info!(version, "SVR backup created successfully after registration");
            }
            Err(e) => {
                tracing::warn!("SVR backup failed (non-fatal): {}", e);
            }
        }

        // 5. Zeroize sensitive intermediates
        svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();
    }

    /// Retry SVR backup creation if it failed during registration.
    ///
    /// This is the public counterpart of [`perform_svr_backup`] — it derives
    /// all key material from the raw credentials so the user can retry without
    /// re-registering. Sensitive intermediates are zeroized on all paths.
    ///
    /// # Arguments
    ///
    /// * `pin` - Exactly 6 ASCII digits.
    /// * `username` - Plaintext username (used for HKDF info and SVR secret derivation).
    /// * `password` - Plaintext password (used to derive the profile key).
    pub async fn retry_svr_backup(
        &self,
        pin: &str,
        username: &str,
        password: &str,
    ) -> Result<(), ClientError> {
        use crate::crypto::svr::derive_svr_secret;
        use hkdf::Hkdf;
        use sha2::{Digest, Sha256};
        use zeroize::Zeroize;

        // 1. Validate PIN (6 digits)
        if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(ClientError::Validation("PIN must be exactly 6 digits".into()));
        }

        // 2. Derive profile key from password + username
        //    (mirrors auth_service::derive_profile_key_from_password)
        let password_hash = Sha256::digest(password.as_bytes());
        let hk = Hkdf::<Sha256>::new(None, &password_hash);
        let info = format!("chatter_profile_key_{}", username.trim().to_lowercase());
        let mut profile_key = [0u8; 32];
        hk.expand(info.as_bytes(), &mut profile_key)
            .map_err(|_| ClientError::Auth(AuthError::Crypto(
                CryptoError::KeyDerivationFailed("HKDF expand failed for profile key".into())
            )))?;

        // 3. Get identity keypair from signal store
        let private_bytes = self.signal().actor.get_identity_private_key().await
            .map_err(|e| {
                profile_key.zeroize();
                ClientError::Validation(format!("failed to get identity private key: {}", e))
            })?;
        let public_bytes_33 = self.signal().actor.get_identity_public_key().await
            .map_err(|e| {
                profile_key.zeroize();
                ClientError::Validation(format!("failed to get identity public key: {}", e))
            })?;
        // Strip the 0x05 type prefix to get raw 32-byte Curve25519 public key
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);

        // 4. Derive SVR secret from PIN + username
        let mut svr_secret = derive_svr_secret(pin, username).map_err(|e| {
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive svr_secret: {}", e))
        })?;

        // 5. Create and upload backup
        let result = self
            .svr_backup()
            .create_and_upload_backup(&svr_secret, &identity_keypair, &profile_key)
            .await;

        // 6. Zeroize all sensitive intermediates (both success and error paths)
        svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();

        result.map_err(|e| ClientError::Validation(format!("SVR backup upload failed: {}", e)))?;
        tracing::info!("SVR backup retry succeeded");
        Ok(())
    }

    /// Change the user's SVR PIN.
    ///
    /// Re-encrypts the identity key backup with a new PIN-derived secret and
    /// atomically updates the server-side backup, registration lock, and SVR
    /// public key. Both old and new PINs must be exactly 6 ASCII digits.
    ///
    /// All sensitive intermediates (secrets, keys) are zeroized on every path.
    ///
    /// # Arguments
    ///
    /// * `old_pin` - Current 6-digit PIN.
    /// * `new_pin` - New 6-digit PIN.
    /// * `username` - Plaintext username (salt for Argon2id derivation).
    pub async fn change_pin(
        &self,
        old_pin: &str,
        new_pin: &str,
        username: &str,
    ) -> Result<(), ClientError> {
        use crate::crypto::svr::{derive_registration_lock, derive_svr_secret, SvrKeypair};
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        // 1. Validate both PINs (6 digits) and ensure they differ
        if old_pin.len() != 6 || !old_pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(ClientError::Validation("Old PIN must be exactly 6 digits".into()));
        }
        if new_pin.len() != 6 || !new_pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(ClientError::Validation("New PIN must be exactly 6 digits".into()));
        }
        if old_pin == new_pin {
            return Err(ClientError::Validation("New PIN must be different from current PIN".into()));
        }

        // 2. Get identity keypair from signal store (private 32 bytes + public 33 bytes)
        let private_bytes = self.signal().actor.get_identity_private_key().await
            .map_err(|e| ClientError::Validation(format!("failed to get identity private key: {}", e)))?;
        let public_bytes_33 = self.signal().actor.get_identity_public_key().await
            .map_err(|e| ClientError::Validation(format!("failed to get identity public key: {}", e)))?;
        // Strip the 0x05 type prefix to get raw 32-byte Curve25519 public key
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);

        // 3. Get profile key from settings
        let mut profile_key = match self.signal().settings_repo.get_bytes(keys::PROFILE_KEY) {
            Ok(Some(bytes)) if bytes.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&bytes);
                pk
            }
            Ok(_) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation("profile key not found or invalid length".into()));
            }
            Err(e) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation(format!("failed to read profile key: {}", e)));
            }
        };

        // 4. Derive old and new SVR secrets
        let mut old_svr_secret = derive_svr_secret(old_pin, username).map_err(|e| {
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive old svr_secret: {}", e))
        })?;
        let mut new_svr_secret = derive_svr_secret(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive new svr_secret: {}", e))
        })?;

        // 5. Derive new registration lock
        let mut new_reg_lock = derive_registration_lock(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive registration lock: {}", e))
        })?;

        // 6. Derive new SVR public key
        let svr_keypair = SvrKeypair::new_from_pin(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            new_reg_lock.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive SVR keypair: {}", e))
        })?;
        let new_svr_public_key = svr_keypair.public_key_bytes();
        // svr_keypair is dropped here (zeroizes secret via Drop impl)

        // 7. Call service to re-encrypt and upload
        //    If no backup exists yet, fall back to creating a new one with the new PIN.
        let result = self
            .svr_backup()
            .change_pin_backup(
                &old_svr_secret,
                &new_svr_secret,
                &identity_keypair,
                &profile_key,
                &new_reg_lock,
                &new_svr_public_key,
            )
            .await;

        let final_result = match &result {
            Err(crate::services::svr_backup_service::SvrBackupError::BackupNotFound)
            | Err(crate::services::svr_backup_service::SvrBackupError::Network(
                crate::error::NetworkError::NotFound(_),
            )) => {
                // No existing backup — create one with the new PIN instead
                tracing::info!("No existing SVR backup found, creating new backup with new PIN");
                self.svr_backup()
                    .create_and_upload_backup(&new_svr_secret, &identity_keypair, &profile_key)
                    .await
                    .map(|_| ())
            }
            _ => result.map(|_| ()),
        };

        // 8. Zeroize ALL sensitive intermediates (both success and error paths)
        old_svr_secret.zeroize();
        new_svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();
        new_reg_lock.zeroize();

        final_result.map_err(|e| ClientError::Validation(format!("PIN change failed: {}", e)))?;
        tracing::info!("PIN changed successfully");
        Ok(())
    }

    /// Attempt to restore identity keys from SVR backup on a new device.
    ///
    /// Checks whether local identity keys exist. If they do, this is an
    /// existing device and no restore is needed. If they are missing, derives
    /// the SVR secret from PIN + username and fetches the encrypted backup.
    ///
    /// On success, hydrates the local identity key store and profile key.
    /// On failure, logs a warning and sets `keys_missing = true` so the UI
    /// can inform the user. Restore failure NEVER blocks login.
    async fn attempt_svr_restore(
        &self,
        pin: &str,
        username: &str,
        result: &mut LoginResult,
    ) {
        use crate::crypto::svr::derive_svr_secret;
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        // 1. Check if local identity keys already exist
        let has_identity = match self.signal().key_store.has_local_identity() {
            Ok(exists) => exists,
            Err(e) => {
                tracing::warn!("SVR restore: failed to check local identity: {}", e);
                // Assume keys exist to avoid accidentally overwriting
                return;
            }
        };

        if has_identity {
            tracing::debug!("SVR restore skipped: local identity keys already exist");
            return;
        }

        tracing::info!("New device detected (no local identity keys) — attempting SVR restore");

        // 2. Derive svr_secret transiently from PIN + username
        let mut svr_secret = match derive_svr_secret(pin, username) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("SVR restore failed: could not derive svr_secret: {}", e);
                result.keys_missing = true;
                return;
            }
        };

        // 3. Fetch and decrypt backup from server
        let restore_result = match self.svr_backup().fetch_and_restore_backup(&svr_secret).await {
            Ok(r) => r,
            Err(e) => {
                svr_secret.zeroize();
                tracing::warn!("SVR restore failed (non-fatal): {}", e);
                result.keys_missing = true;
                return;
            }
        };

        svr_secret.zeroize();

        // 4. Hydrate identity keypair into local key store
        //    Reconstruct libsignal IdentityKeyPair from raw 32-byte private + 32-byte public
        let mut restored_keypair = restore_result.identity_keypair; // [u8; 64]
        let mut restored_profile_key = restore_result.profile_key;  // [u8; 32]

        let hydrate_ok = (|| -> std::result::Result<(), String> {
            use libsignal_protocol::{IdentityKey, IdentityKeyPair, PrivateKey};

            let private_key = PrivateKey::deserialize(&restored_keypair[..32])
                .map_err(|e| format!("invalid private key: {}", e))?;

            // IdentityKey expects 33 bytes: 0x05 type prefix + 32-byte public key
            let mut public_with_prefix = [0u8; 33];
            public_with_prefix[0] = 0x05;
            public_with_prefix[1..].copy_from_slice(&restored_keypair[32..]);
            let identity_key = IdentityKey::try_from(public_with_prefix.as_slice())
                .map_err(|e| format!("invalid public key: {}", e))?;

            let key_pair = IdentityKeyPair::new(identity_key, private_key);
            let serialized = key_pair.serialize();

            // Generate a registration ID for this new device (14-bit, 1-16383)
            let registration_id = (rand::random::<u32>() % 0x3FFF) + 1;

            self.signal()
                .key_store
                .set_local_identity(registration_id, &serialized)
                .map_err(|e| format!("failed to store identity: {}", e))?;

            // Store identity public key in settings for NDSS'21 friend codes
            let identity_public_full = key_pair.identity_key().serialize();
            self.signal()
                .settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, identity_public_full.as_ref())
                .map_err(|e| format!("failed to store identity public key: {}", e))?;

            Ok(())
        })();

        if let Err(e) = hydrate_ok {
            tracing::warn!("SVR restore: failed to hydrate identity keypair: {}", e);
            restored_keypair.zeroize();
            restored_profile_key.zeroize();
            result.keys_missing = true;
            return;
        }

        // 5. Hydrate profile key into settings
        if let Err(e) = self
            .signal()
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &restored_profile_key)
        {
            tracing::warn!("SVR restore: failed to store profile key: {}", e);
            // Identity was stored but profile key failed — partial restore
            restored_keypair.zeroize();
            restored_profile_key.zeroize();
            result.keys_restored = true;
            result.keys_missing = false;
            return;
        }

        // 6. Zeroize sensitive intermediates
        restored_keypair.zeroize();
        restored_profile_key.zeroize();

        result.keys_restored = true;
        result.keys_missing = false;

        tracing::info!(
            remaining_attempts = restore_result.remaining_attempts,
            "SVR key restore completed successfully"
        );
    }

    /// Log out and shut down all background services.
    pub async fn logout(&self) -> Result<(), ClientError> {
        self.auth().do_logout().await?;
        self.shutdown().await;
        self.auth_state().set_logged_out();
        Ok(())
    }

    // ── Conversation listing ────────────────────────────────────────

    // ── Channel operations ────────────────────────────────────────

    /// Create a channel and set up group encryption (mailbox + sender keys).
    ///
    /// Handles the full protocol: create on server, announce mailbox,
    /// distribute SKDM to members. The caller doesn't need to know about
    /// mailbox setup or sender key distribution.
    pub async fn create_channel(
        &self,
        name: &str,
        channel_type: ChannelType,
        config: Option<crate::domain::channel::ChannelConfig>,
        description: Option<&str>,
        password: Option<&str>,
        initial_members: Vec<crate::domain::ids::UserId>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let (channel, mailbox_setup) = self.channels()
            .create_channel(name, channel_type, config, description, None, password, initial_members, true)
            .await?;
        self.finalize_mailbox_setup(&channel.id, mailbox_setup).await;
        Ok(channel)
    }

    /// Join a channel and set up group encryption.
    ///
    /// Handles mailbox announcement and SKDM distribution automatically.
    pub async fn join_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        password: Option<&str>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let (channel, mailbox_setup) = self.channels()
            .join_channel(channel_id, password, true)
            .await?;
        self.finalize_mailbox_setup(&channel.id, mailbox_setup).await;
        Ok(channel)
    }

    /// Kick a user from a channel and re-distribute sender keys.
    ///
    /// After kicking, a new SKDM is distributed so the kicked user
    /// can no longer decrypt future messages.
    pub async fn kick_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        let skdm_opt = self.channels().kick_user(channel_id, user_id, reason).await?;
        if let Some(skdm) = skdm_opt {
            if let Err(e) = self.messages().distribute_skdm(channel_id, &skdm).await {
                tracing::error!(channel_id = %channel_id, error = %e, "Failed to distribute SKDM after kick");
            }
        }
        Ok(())
    }

    /// Update a channel's name and/or description.
    pub async fn update_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        self.channels().update_channel(channel_id, name, description, None, None).await
    }

    /// Archive or unarchive a channel.
    pub async fn archive_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        archive: bool,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        if archive {
            self.channels().archive_channel(channel_id).await
        } else {
            self.channels().unarchive_channel(channel_id).await
        }
    }

    /// Leave a channel.
    pub async fn leave_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().leave_channel(channel_id).await
    }

    /// Search public channels.
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().search_channels(query, limit, 0, vec![ChannelType::Public]).await
    }

    // ── Conversation listing ────────────────────────────────────────

    /// List all channels the user is a member of.
    pub fn list_channels(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().get_channels_for_user()
    }

    /// List all DM channels.
    pub fn list_dms(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        let all = self.channels().get_channels_for_user()?;
        Ok(all.into_iter().filter(|c| c.channel_type == ChannelType::DirectMessage).collect())
    }

    /// List all anonymous DM conversations.
    pub fn list_anon_dms(&self) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_conversations()
    }

    /// List all anonymous group conversations.
    pub fn list_anon_groups(&self) -> Result<Vec<crate::domain::anon_group::AnonGroup>, crate::services::anon_group::AnonGroupError> {
        self.anon_groups().list_groups()
    }

    /// List pending anon DM message requests (incoming).
    pub fn list_anon_dm_requests(&self) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_message_requests()
    }

    // ── Friend / social operations ──────────────────────────────────

    /// Send a friend request.
    pub async fn send_friend_request(
        &self,
        user_id: &crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::user::UserError> {
        self.users().send_friend_request(user_id, message).await
    }

    /// Accept or decline a friend request.
    pub async fn respond_friend_request(
        &self,
        user_id: &crate::domain::ids::UserId,
        accept: bool,
    ) -> Result<crate::services::user::FriendResponseResult, crate::services::user::UserError> {
        self.users().respond_friend_request(user_id, accept).await
    }

    /// Remove a friend.
    pub async fn remove_friend(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().remove_friend(user_id).await
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

    /// List friends.
    pub async fn list_friends(&self) -> Result<Vec<crate::domain::user::Friendship>, crate::services::user::UserError> {
        self.users().list_friends(100, 0).await
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
        display_name: Option<&str>,
        avatar_url: Option<&str>,
        status: Option<&crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().update_user_profile(display_name, avatar_url, status).await
    }

    /// Change password.
    pub async fn change_password(
        &self,
        current: &str,
        new: &str,
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().change_password(current, new).await
    }

    /// Get a user's display name.
    pub fn get_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        self.users().get_display_name(user_id)
    }

    /// Get a user's profile.
    pub async fn get_user_profile(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().get_user_profile(user_id).await
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
            ActiveConversation::Channel(id) | ActiveConversation::MailboxDm { channel_id: id, .. } | ActiveConversation::ServerDm { channel_id: id, .. } => {
                self.messages().get_messages_local(id, None, limit)
            }
            // Anon paths use their own message types — return empty for now.
            // SDK consumers should use anon_dms().get_messages() / anon_groups().get_messages()
            // for anonymous conversations which have different message schemas.
            _ => Ok(vec![]),
        }
    }

    // ── Presence ─────────────────────────────────────────────────────

    /// Set presence status and broadcast to all anon DM conversations.
    pub async fn set_presence(&self, presence: crate::proto::UserPresence) {
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

    // ── Private channel helpers ────────────────────────────────────

    /// Announce mailbox and distribute SKDM after channel create/join.
    async fn finalize_mailbox_setup(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        mailbox_setup: Option<crate::mailbox::group::GroupMailboxSetupResult>,
    ) {
        let Some(setup) = mailbox_setup else { return };
        if let Err(e) = self.messages().announce_group_mailbox(channel_id, setup.announcement).await {
            tracing::warn!(channel_id = %channel_id, error = %e, "Failed to announce group mailbox");
        }
        if let Some(skdm) = setup.skdm {
            if let Err(e) = self.messages().distribute_skdm(channel_id, &skdm).await {
                tracing::error!(channel_id = %channel_id, error = %e, "Failed to distribute SKDM");
            }
        }
    }

    // ── Call history ──────────────────────────────────────────────────

    /// Store a call history message for a standard DM/channel call.
    pub fn store_call_history(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        call_id: &str,
        initiator_id: &crate::domain::ids::UserId,
        initiator_name: &str,
        _is_outgoing: bool,
        duration_secs: i64,
        is_missed: bool,
    ) {
        use crate::domain::message::*;

        let call_type = if is_missed { CallMessageType::Missed } else { CallMessageType::Ended };
        let duration = if duration_secs > 0 {
            Some(chrono::Duration::seconds(duration_secs))
        } else {
            None
        };

        let call_msg = CallMessage {
            call_id: call_id.to_string(),
            call_type,
            initiator_id: *initiator_id,
            initiator_name: initiator_name.to_string(),
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            duration,
            participant_count: 2,
        };

        let message = Message {
            id: crate::domain::ids::MessageId::new(),
            channel_id: *channel_id,
            sender_id: Some(*initiator_id),
            content: MessageContent::Call(call_msg),
            thread_id: None,
            reply_to: None,
            attachments: vec![],
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            sent_at: crate::util::time::TimestampNanos::now().as_u64(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            delivery_status: String::new(),
        };

        if let Err(e) = self.messages().store_message_local(&message) {
            tracing::error!(error = %e, "Failed to save call history");
        }
    }

    /// Store a call history entry in an anon DM conversation.
    pub fn store_anon_call_history(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        is_outgoing: bool,
        duration_seconds: i64,
    ) {
        let duration_str = if duration_seconds > 0 {
            format!("{}:{:02}", duration_seconds / 60, duration_seconds % 60)
        } else {
            "0:00".to_string()
        };
        let label = if is_outgoing { "Outgoing call" } else { "Incoming call" };
        let text = format!("\u{1F4DE} {} \u{00B7} {}", label, duration_str);
        let content_json = format!(r#"{{"text":"{}"}}"#, text);
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let user_id = self.auth_state().get().current_user_id
            .unwrap_or_else(crate::domain::ids::UserId::new);

        if let Err(e) = self.anon_dms().store_message(
            conversation_id, &user_id, 1, &content_json, sent_at,
        ) {
            tracing::error!(error = %e, "Failed to store anon call history");
        }
    }

    // ── Call lifecycle ─────────────────────────────────────────────

    /// End an active DM/channel call: send hangup, store history, leave.
    ///
    /// `duration_secs` is the call duration from the UI timer.
    pub async fn end_dm_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_dm_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End an active anonymous call: send hangup, store history, leave.
    ///
    /// Returns the conversation_id for UI bubble insertion.
    pub async fn end_anon_call(&self, duration_secs: i64) -> Result<Option<crate::domain::anon_dm::AnonDmId>, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_anon_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Handle a participant departing from a group call.
    ///
    /// If we are the initiator of a channel-based group call, generates a new
    /// random encryption key, rotates it locally, and redistributes to remaining
    /// members so the departed participant can't decrypt future frames.
    ///
    /// Only the initiator rotates to avoid race conditions.
    pub async fn handle_participant_departed(&self) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;

        let call = match call_worker.active_call().await {
            Some(c) => c,
            None => return Ok(()), // No active call
        };

        // Only rotate for channel-based group calls (not anon, not DM).
        // Anonymous calls have a random channel_id that won't resolve.
        let channel = match self.channels().get_channel_local(&call.channel_id) {
            Ok(Some(c)) => c,
            _ => return Ok(()),
        };

        use crate::domain::channel::ChannelType;
        if matches!(channel.channel_type, ChannelType::DirectMessage | ChannelType::SavedMessages) {
            return Ok(());
        }

        // Only the initiator rotates to prevent race conditions
        let my_id = self.sessions().current_user_id()
            .ok_or_else(|| ClientError::Inbox("not logged in".into()))?;
        if my_id != call.initiator_id {
            return Ok(());
        }

        // Generate new random key and rotate locally
        let new_key = crate::crypto::generate_group_call_key();
        let epoch = call_worker.rotate_key(new_key).await
            .map_err(|e| ClientError::Inbox(format!("key rotation failed: {e}")))?;

        // Brief delay for LiveKit key propagation before peers receive new-key frames
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Redistribute the new key to remaining channel members
        let call_id = call.id.to_string();
        match self.call_signaling()
            .distribute_group_call_key(&call.channel_id, &call_id, new_key, epoch)
            .await
        {
            Ok(count) => {
                tracing::info!(epoch, sent = count, "Rotated group call key after participant departure");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to redistribute rotated group call key");
            }
        }

        Ok(())
    }

    // ── AI Integration ─────────────────────────────────────────────

    /// Map a provider string to the proto IntegrationType.
    fn ai_provider_type(provider: &str) -> crate::proto::chat_v1::IntegrationType {
        use crate::proto::chat_v1::IntegrationType;
        match provider {
            "claude" => IntegrationType::Claude,
            "openai" => IntegrationType::Openai,
            _ => IntegrationType::Custom,
        }
    }

    /// List available AI models for a provider.
    ///
    /// Returns `(id, display_name)` pairs so the UI doesn't need proto types.
    pub async fn list_ai_models(
        &self,
        provider: &str,
    ) -> Result<Vec<(String, String)>, ClientError> {
        let models = self.ai().list_models(Self::ai_provider_type(provider))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;
        Ok(models.into_iter().map(|m| (m.id, m.display_name)).collect())
    }

    /// Create an AI integration and attach it to a channel.
    pub async fn create_ai_integration(
        &self,
        name: &str,
        provider: &str,
        model: &str,
        channel_id: &str,
    ) -> Result<(), ClientError> {
        use crate::proto::chat_v1::IntegrationConfig;

        let config = IntegrationConfig {
            model: model.to_string(),
            ..Default::default()
        };

        let integration = self.ai().create(name, Self::ai_provider_type(provider), Some(config))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        self.ai().attach_to_channel(&integration.id, channel_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        Ok(())
    }

    /// Handle remote hangup: store history, disconnect locally.
    pub async fn handle_remote_hangup(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .handle_remote_hangup(call_worker, duration_secs)
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

        // Disconnect gRPC
        self.ui.auth_service.disconnect();

        self.started.store(false, Ordering::SeqCst);
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

async fn upload_attachments(pending: &[PendingAttachment], attachment_service: &AttachmentService) -> UploadedAttachments {
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
            Err(e) => tracing::error!(path = %att.path.display(), error = %e, "Failed to upload attachment"),
        }
    }
    UploadedAttachments { pointers, attachments }
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
        ActiveConversation::ServerDm { channel_id, .. } => Ok(OutgoingIntent::Channel {
            channel_id, text: text.to_string(), attachment_pointers,
            file_paths: vec![], thread_id: options.thread_id,
            mentioned_integrations: options.mentioned_integrations.clone(),
            targets_ai_bot: options.targets_ai_bot,
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
                })
            }
        }
        ActiveConversation::None => Err(SendError::UnsupportedAction),
    }
}
