//! SDK facade for chatter-core.
//!
//! [`ChatterClient`] is the main entry point for consumers of this library.
//! It wraps the internal [`AppContext`] and [`UIContext`] with a clean,
//! discoverable API.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::ui_context::UIContext;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::Message;
use crate::error::NetworkError;
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
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self { server_url: None }
    }

    /// Set the server URL. Falls back to `CHATTER_SERVER_URL` env var, then built-in default.
    pub fn server_url(mut self, url: impl Into<String>) -> Self {
        self.server_url = Some(url.into());
        self
    }

    /// Build the client. Initializes database, crypto, and network pool.
    ///
    /// Must be called within a tokio runtime (e.g. inside `#[tokio::main]`
    /// or after `Runtime::new()`).
    pub fn build(self) -> Result<Arc<ChatterClient>, ClientError> {
        let ctx = AppContext::with_config(self.server_url)?;
        let ui = ctx.ui_context();
        Ok(Arc::new(ChatterClient {
            ctx,
            ui,
            tasks: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(0),
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
    ui: Arc<UIContext>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    started: AtomicBool,
    /// Epoch millis of last user activity (for idle detection).
    last_activity_ms: AtomicU64,
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

    /// Create a call signaling service for sending call key exchanges.
    pub fn call_signaling(&self) -> crate::services::call_signaling::CallSignalingService {
        crate::services::call_signaling::CallSignalingService::new(
            self.messages().clone(),
            self.mailbox_manager().clone(),
            self.channels().clone(),
        )
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

    // ── Delegated UIContext methods ─────────────────────────────────────

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
    pub async fn login(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        device_name: &str,
    ) -> Result<LoginResult, ClientError> {
        let result = self.auth().do_login(username, password, device_name, None).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );
        self.start().await?;
        Ok(result)
    }

    /// Register a new account and start all background services.
    pub async fn register(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        let result = self.auth().do_register(
            username, password, display_name, device_name, avatar, pin,
        ).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );
        self.start().await?;
        Ok(result)
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

    /// List all channels the user is a member of.
    pub fn list_channels(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().get_channels_for_user()
    }

    /// List all DM channels. Returns `(channel, peer_display_name)` pairs.
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
            ActiveConversation::Channel(id) | ActiveConversation::Dm { channel_id: id, .. } => {
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
        ActiveConversation::Dm { channel_id, peer_id } => Ok(OutgoingIntent::ChannelDm {
            peer_id, dm_channel_id: channel_id, text: text.to_string(),
            attachment_pointers, file_paths: vec![], quote,
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
