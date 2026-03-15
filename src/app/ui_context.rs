//! UI context.
//!
//! The public API surface for UI code. Exposes services and state stores
//! but never repositories or low-level infrastructure directly.

use std::sync::Arc;

use crate::crypto::signal::IdentityKeyManager;
use crate::inbox::DeviceInboxProvider;
use crate::services::CallWorkerHandle;
use crate::storage::KeyStore;
use crate::events::EventBus;
use crate::mailbox::group::{GroupCoordinator, GroupFanout, GroupMailboxManager};
use crate::mailbox::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::ai::LocalAIService;
use crate::services::jarvis::JarvisService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
use crate::services::channel::ChannelService;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::draft_service::DraftService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::message_gateway::MessageGateway;
use crate::services::pin_service::PinService;
use crate::services::reaction_service::ReactionService;
use crate::services::session_service::SessionService;
use crate::services::signal_service::SignalService;
use crate::services::user::UserService;
use crate::services::typing_service::TypingService;
use crate::state::{
    AuthState, ChatState, JarvisState, MessageState, OfflineState, PinnedState, PresenceState,
    PrivacyState, StreamingState, UIState, UserState,
};
use crate::storage::Database;
use crate::system::Directories;
use crate::ui::bridge::UiBridge;

use super::infrastructure::InfrastructureContainer;
use super::services::ServiceContainer;

/// UI context providing services and state to the UI layer.
///
/// This is the primary API for UI code. It exposes:
/// - Pre-built services for business operations
/// - State stores for reactive UI binding
/// - Event bus for domain events
/// - Network pool for connection management
/// - Runtime handle for spawning async tasks from UI callbacks
pub struct UIContext {
    // Runtime handle for async spawning from sync UI callbacks
    pub runtime_handle: tokio::runtime::Handle,
    // Services
    pub auth_service: Arc<AuthService>,
    pub message_service: Arc<MessageService>,
    pub message_gateway: Arc<MessageGateway>,
    pub user_service: Arc<UserService>,
    pub channel_service: Arc<ChannelService>,
    pub session_service: Arc<SessionService>,
    pub attachment_service: Arc<AttachmentService>,
    pub dm_bootstrap_service: Arc<DmBootstrapService>,
    pub mailbox_manager: Arc<MailboxManager>,
    pub signal_service: Arc<SignalService>,
    pub channel_event_service: Arc<ChannelEventService>,
    pub ai_service: Arc<AIService>,
    pub reaction_service: Arc<ReactionService>,
    pub typing_service: Arc<TypingService>,
    pub draft_service: Arc<DraftService>,
    pub pin_service: Arc<PinService>,
    pub anon_group_service: Arc<AnonGroupService>,
    pub anon_dm_service: Arc<AnonDmService>,
    pub local_ai_service: Arc<LocalAIService>,
    pub jarvis_service: Arc<JarvisService>,

    // State stores
    pub ui_state: UIState,
    pub auth_state: AuthState,
    pub chat_state: ChatState,
    pub message_state: MessageState,
    pub user_state: UserState,
    pub presence_state: PresenceState,
    pub privacy_state: PrivacyState,
    pub offline_state: OfflineState,
    pub streaming_state: StreamingState,
    pub pinned_state: PinnedState,
    pub jarvis_state: JarvisState,

    // Event bus
    pub event_bus: EventBus,

    // Infrastructure (needed for network operations)
    pub grpc_pool: Arc<GrpcClientPool>,
    pub database: Arc<Database>,
    pub directories: Directories,
    pub group_coordinator: Arc<GroupCoordinator>,
    pub group_mailbox_manager: Arc<GroupMailboxManager>,
    pub token_manager: Arc<TokenManager>,

    // Crypto (needed for calls ephemeral key exchange)
    pub identity_manager: Arc<IdentityKeyManager>,
    pub key_store: Arc<KeyStore>,

    // Post-boot services (initialized via AppContext after grpc connect)
    pub group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
    pub call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
    pub device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
}

impl UIContext {
    /// Create UIContext from infrastructure and services.
    pub(crate) fn new(
        infra: &InfrastructureContainer,
        services: &ServiceContainer,
        group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
        call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
        device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
    ) -> Self {
        Self {
            // Runtime handle for async spawning from UI callbacks
            runtime_handle: tokio::runtime::Handle::current(),

            // Services
            auth_service: services.auth_service.clone(),
            message_service: services.message_service.clone(),
            message_gateway: services.message_gateway.clone(),
            user_service: services.user_service.clone(),
            channel_service: services.channel_service.clone(),
            session_service: services.session_service.clone(),
            attachment_service: services.attachment_service.clone(),
            dm_bootstrap_service: services.dm_bootstrap_service.clone(),
            mailbox_manager: services.mailbox_manager.clone(),
            signal_service: services.signal_service.clone(),
            channel_event_service: services.channel_event_service.clone(),
            ai_service: services.ai_service.clone(),
            reaction_service: services.reaction_service.clone(),
            typing_service: services.typing_service.clone(),
            draft_service: services.draft_service.clone(),
            pin_service: services.pin_service.clone(),
            anon_group_service: services.anon_group_service.clone(),
            anon_dm_service: services.anon_dm_service.clone(),
            local_ai_service: services.local_ai_service.clone(),
            jarvis_service: services.jarvis_service.clone(),

            // State stores
            ui_state: UIState::new(),
            auth_state: AuthState::new(),
            chat_state: ChatState::new(),
            message_state: MessageState::new(),
            user_state: UserState::new(),
            presence_state: PresenceState::new(),
            privacy_state: PrivacyState::new(),
            offline_state: OfflineState::new(),
            streaming_state: StreamingState::new(),
            pinned_state: PinnedState::new(),
            jarvis_state: JarvisState::new(),
            event_bus: EventBus::new(),

            // Infrastructure
            grpc_pool: infra.grpc_pool.clone(),
            database: infra.database.clone(),
            directories: infra.directories.clone(),
            group_coordinator: infra.group_coordinator.clone(),
            group_mailbox_manager: infra.group_mailbox_manager.clone(),
            token_manager: infra.token_manager.clone(),

            // Crypto
            identity_manager: infra.identity_manager.clone(),
            key_store: infra.key_store.clone(),

            // Post-boot services (shared OnceLock with AppContext)
            group_fanout,
            call_service,
            device_inbox_provider,
        }
    }

    /// Creates a UiBridge for Slint integration.
    pub fn ui_bridge(&self) -> UiBridge {
        UiBridge::new(
            self.ui_state.clone(),
            self.auth_state.clone(),
            self.event_bus.clone(),
        )
    }

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.grpc_pool.try_channel().is_some()
    }

    /// Create a GIF client for server-side GIF search.
    pub fn gif_client(&self) -> Option<crate::network::grpc::GifClient> {
        self.grpc_pool.try_channel().map(|channel| {
            crate::network::grpc::GifClient::new(channel, self.grpc_pool.auth_clone())
        })
    }

    /// Pre-fetch anonymous tokens for sealed sender messaging.
    ///
    /// Ensures tokens are available before the first SKDM distribution.
    /// Returns the number of tokens fetched, or 0 if already sufficient.
    pub async fn prefetch_tokens(&self, target_count: u32) -> Result<u32, crate::error::MailboxError> {
        let current = self.token_manager.available_token_count()?;
        if current >= target_count {
            tracing::debug!(
                "Sufficient anonymous tokens available: {} (target: {})",
                current,
                target_count
            );
            return Ok(0);
        }

        let needed = target_count - current;
        tracing::info!(
            "Pre-fetching anonymous tokens (current: {}, target: {})",
            current,
            target_count
        );

        let mut mailbox_client = crate::network::grpc::mailbox_client::MailboxClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        let fetched = self.token_manager.request_tokens(&mut mailbox_client, needed).await?;
        tracing::info!("Pre-fetched {} anonymous tokens", fetched);
        Ok(fetched)
    }

    /// Decline an incoming call.
    ///
    /// Notifies the server that the call was declined without joining.
    pub async fn decline_call(&self, call_id: &str) -> Result<(), crate::error::NetworkError> {
        let mut call_client = crate::network::grpc::CallClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        call_client.leave_call(call_id).await
    }

    /// Refresh prekeys and return the new available count.
    ///
    /// Generates and uploads new one-time prekeys, then queries for the updated count.
    /// Returns the number of prekeys now available on the server.
    pub async fn refresh_prekeys(&self, count: u32) -> Result<u32, crate::error::ServiceError> {
        let mut auth_client = crate::network::grpc::auth_client::AuthClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        let uploaded = self.signal_service.refresh_prekeys(&mut auth_client, count).await?;
        tracing::info!("Uploaded {} new prekeys", uploaded);

        // Get updated count from server
        match auth_client.get_pre_key_stats(0).await {
            Ok(stats) => {
                let count = stats.available_count as u32;
                tracing::info!("Now have {} prekeys available on server", count);
                Ok(count)
            }
            Err(e) => {
                tracing::warn!("Failed to get prekey stats after refresh: {}", e);
                // Return an estimate based on what we uploaded
                Ok(uploaded)
            }
        }
    }

    /// Initialize the call service if not already set.
    ///
    /// Safe to call multiple times — uses `get_or_init` internally.
    pub fn ensure_call_service(&self) {
        use crate::services::call_worker::CallWorkerHandle;

        self.call_service.get_or_init(|| {
            let call_client = crate::network::grpc::CallClient::new(
                self.grpc_pool.channel(),
                self.grpc_pool.auth_clone(),
            );
            CallWorkerHandle::spawn(call_client)
        });
    }

    /// Initialize the device inbox provider if not already set.
    ///
    /// This is safe to call multiple times — uses `get_or_init` internally.
    /// Called automatically by `start_inbox_provider()`.
    pub fn ensure_device_inbox_provider(&self) {
        use crate::inbox::DeviceInboxProvider;
        use crate::mailbox::AnonDmPoller;

        self.device_inbox_provider.get_or_init(|| {
            let message_client = crate::network::grpc::message_client::MessageClient::new(
                self.grpc_pool.channel(),
                self.grpc_pool.auth_clone(),
            );
            let mailbox_client = crate::network::grpc::mailbox_client::MailboxClient::new(
                self.grpc_pool.channel(),
                self.grpc_pool.auth_clone(),
            );
            let pipeline = std::sync::Arc::new(crate::network::validation::MessageValidationPipeline::new());

            let anon_dm_poller = std::sync::Arc::new(AnonDmPoller::new(
                self.anon_dm_service.anon_dm_repo(),
            ));

            std::sync::Arc::new(
                DeviceInboxProvider::with_event_bus(
                    crate::inbox::InboxConfig::default(),
                    std::sync::Arc::new(tokio::sync::Mutex::new(message_client)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(mailbox_client)),
                    pipeline,
                    self.message_gateway.clone(),
                    self.database.clone(),
                    self.grpc_pool.clone(),
                    self.attachment_service.attachment_repo(),
                    Some(self.event_bus.clone()),
                )
                .with_anon_dm_polling(anon_dm_poller, self.anon_dm_service.pairwise_service()),
            )
        });
    }

    /// Start the inbox provider and return a receiver for inbox events.
    ///
    /// This should be called after login/register to enable real-time events
    /// (friend requests, DM messages, etc.).
    ///
    /// Auto-initializes the provider if it hasn't been set up yet (e.g., when
    /// the initial server connection failed but login/register succeeded later).
    pub async fn start_inbox_provider(&self) -> Option<tokio::sync::broadcast::Receiver<crate::inbox::InboxEvent>> {
        // Auto-initialize if not yet set (handles dev mode + offline startup)
        self.ensure_device_inbox_provider();
        let provider = self.device_inbox_provider.get()?.clone();

        match provider.start().await {
            Ok(()) => {
                tracing::info!("Inbox provider started");
                Some(provider.subscribe())
            }
            Err(e) => {
                // AlreadyRunning is not an error - just return the subscriber
                if e.to_string().contains("already running") {
                    tracing::debug!("Inbox provider already running, returning subscriber");
                    Some(provider.subscribe())
                } else {
                    tracing::error!("Failed to start inbox provider: {}", e);
                    None
                }
            }
        }
    }

    /// Start the inbox provider with the InboxEventProcessor.
    ///
    /// This is the recommended way to handle inbox events. It:
    /// 1. Starts the inbox provider
    /// 2. Creates an InboxEventProcessor that converts InboxEvent → DomainEvent
    /// 3. Spawns a background task to process events
    ///
    /// UI code should subscribe to DomainEvent via `event_bus.subscribe_domain()`.
    ///
    /// Returns true if started successfully, false if already running or failed.
    pub async fn start_inbox_with_processor(&self) -> bool {
        use crate::services::InboxEventProcessor;

        let inbox_rx = match self.start_inbox_provider().await {
            Some(rx) => rx,
            None => return false,
        };

        // Create the processor
        let processor = InboxEventProcessor::new(
            self.event_bus.domain_sender(),
            self.message_service.clone(),
            self.dm_bootstrap_service.clone(),
            self.mailbox_manager.clone(),
            self.key_store.clone(),
        );

        // Spawn the processing task
        let mut inbox_rx = inbox_rx;
        tokio::spawn(async move {
            tracing::info!("InboxEventProcessor started");
            loop {
                match inbox_rx.recv().await {
                    Ok(event) => {
                        processor.process(event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "InboxEventProcessor lagged, events skipped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("InboxEventProcessor stopped (channel closed)");
                        break;
                    }
                }
            }
        });

        tracing::info!("Inbox provider started with InboxEventProcessor");
        true
    }
}
