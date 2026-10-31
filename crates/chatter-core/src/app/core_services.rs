//! Core services container.
//!
//! Aggregates all services, state stores, and the event bus into a single
//! container. Used internally by `ChatterClient` — never exposed to UI directly.

use std::sync::Arc;

use crate::events::EventBus;
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::TokenManager;
use crate::mailbox::group::GroupFanout;
use crate::network::grpc::client_pool::GrpcClientPool;
#[cfg(feature = "media")]
use crate::services::CallWorkerHandle;
use crate::services::ai::LocalAIService;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
use crate::services::channel::ChannelService;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::conversation_pins::ConversationPins;
use crate::services::device_service::DeviceService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::ephemeral_service::EphemeralService;
use crate::services::gateway::MessageGateway;
#[cfg(feature = "jarvis")]
use crate::services::jarvis::{JarvisService, SharedJarvisEngine};
use crate::services::local_ai_service::HttpLocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::notification_service::NotificationService;
use crate::services::pin_service::PinService;
use crate::services::section_service::SectionService;
use crate::services::semantic_search::SemanticSearchService;
use crate::services::session_service::SessionService;
use crate::services::signal::SignalService;
use crate::services::user::UserService;
#[cfg(feature = "jarvis")]
use crate::state::JarvisState;
use crate::state::{AuthState, ChatState, MessageState, OfflineState, UserState};
use crate::storage::Database;
use crate::system::Directories;
use crate::system::notification_sink::NotificationSink;

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
pub struct CoreServices {
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
    pub pin_service: Arc<PinService>,
    /// Ephemeral (disappearing-message) local-expiry engine — drives the
    /// periodic reaper started in `ChatterClient::start`.
    pub ephemeral_service: Arc<EphemeralService>,
    pub anon_group_service: Arc<AnonGroupService>,
    pub anon_dm_service: Arc<AnonDmService>,
    pub local_ai_service: Arc<LocalAIService>,
    pub http_local_ai_service: Arc<HttpLocalAIService>,
    pub semantic_search_service: Arc<SemanticSearchService>,
    #[cfg(feature = "jarvis")]
    pub jarvis_service: Arc<JarvisService>,
    /// The bitchat mesh. Serverless: it shares no identity and makes no RPC.
    #[cfg(feature = "mesh")]
    pub mesh_service: Arc<crate::services::mesh::MeshService>,
    #[cfg(feature = "jarvis")]
    pub jarvis_engine: SharedJarvisEngine,
    pub notification_service: Arc<NotificationService>,
    pub section_service: Arc<SectionService>,
    pub conversation_pins: Arc<ConversationPins>,
    pub device_service: Arc<DeviceService>,
    /// Unified conversation repository.
    pub conversation_repo: Arc<crate::storage::repositories::UnifiedConversationRepository>,
    /// Multi-device sync engine wrapper (channels, messages, friends, block
    /// list). Started by `ChatterClient::start` — nothing else drives
    /// `SyncEngine`, so this field going unused means cross-device sync is
    /// dead, which is exactly how the block list silently stopped propagating
    /// between devices once before.
    pub sync_service: Arc<crate::services::sync_service::SyncService>,
    /// Live channel-message stream (server-managed channels). Started by
    /// `ChatterClient::start`; it is the ONLY thing that delivers channel
    /// messages live, and it re-subscribes on every connect from the server's
    /// own membership list, which is what makes a restart recover (INFRA-1).
    /// `tests/production_wiring.rs` pins its start call.
    pub channel_stream_service: Arc<crate::services::channel_stream::ChannelStreamService>,

    // State stores
    pub auth_state: AuthState,
    pub chat_state: ChatState,
    pub message_state: MessageState,
    pub user_state: UserState,
    pub offline_state: OfflineState,
    #[cfg(feature = "jarvis")]
    pub jarvis_state: JarvisState,

    // Event bus
    pub event_bus: EventBus,

    // Infrastructure (internal — UI code uses service methods instead)
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) database: Arc<Database>,
    pub directories: Directories,
    pub(crate) token_manager: Arc<TokenManager>,
    /// Channel lookups for client-level flows. UI code should still route
    /// through ChannelService, not this field directly.
    pub(crate) channel_repo: Arc<crate::storage::ChannelRepository>,

    // Note: key_store is accessed via SignalService — not stored here.

    // Post-boot services (initialized via AppContext after grpc connect)
    pub group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
    #[cfg(feature = "media")]
    pub call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
    pub device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
}

impl CoreServices {
    /// Create CoreServices from infrastructure and services.
    pub(crate) fn new(
        infra: &InfrastructureContainer,
        services: &ServiceContainer,
        group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
        #[cfg(feature = "media")] call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
        device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
        notification_sink: Option<Arc<dyn NotificationSink>>,
    ) -> Self {
        let notification_sink: Arc<dyn NotificationSink> = notification_sink.unwrap_or_else(|| {
            #[cfg(all(
                feature = "desktop-native",
                not(any(target_os = "android", target_os = "ios"))
            ))]
            {
                Arc::new(crate::system::notifications::DesktopNotifications::new())
            }
            #[cfg(any(
                not(feature = "desktop-native"),
                target_os = "android",
                target_os = "ios"
            ))]
            {
                Arc::new(crate::system::notification_sink::NoopNotificationSink)
            }
        });
        // Shared decrypt -> persist path for server-managed channel messages.
        // Both the live stream and the durable catch-up fetch go through it,
        // so a message reads the same whichever way it arrived.
        let channel_inbound = Arc::new(
            crate::services::message::channel_inbound::ChannelInbound::new(
                services.message_service.clone(),
                services.attachment_service.clone(),
                services.user_service.clone(),
            ),
        );

        let sync_service = Arc::new(crate::services::sync_service::SyncService::new(
            infra.grpc_pool.clone(),
            infra.channel_repo.clone(),
            infra.user_repo.clone(),
            infra.settings_repo.clone(),
            infra.event_bus.clone(),
            channel_inbound.clone(),
            services.channel_event_service.clone(),
            services.message_service.clone(),
        ));

        let auth_state = AuthState::new();
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
            pin_service: services.pin_service.clone(),
            ephemeral_service: services.ephemeral_service.clone(),
            anon_group_service: services.anon_group_service.clone(),
            anon_dm_service: services.anon_dm_service.clone(),
            local_ai_service: services.local_ai_service.clone(),
            http_local_ai_service: services.http_local_ai_service.clone(),
            semantic_search_service: services.semantic_search_service.clone(),
            #[cfg(feature = "jarvis")]
            jarvis_service: services.jarvis_service.clone(),
            #[cfg(feature = "mesh")]
            mesh_service: services.mesh_service.clone(),
            #[cfg(feature = "jarvis")]
            jarvis_engine: services.jarvis_engine.clone(),
            notification_service: {
                let svc = NotificationService::with_account_keys(
                    notification_sink,
                    infra.settings_repo.clone(),
                    auth_state.clone(),
                );
                // Load privacy setting from local DB
                if let Ok(Some(level_str)) = infra.settings_repo.get::<String>(
                    crate::storage::repositories::settings_repo::keys::NOTIFICATION_DETAIL_LEVEL,
                ) {
                    svc.set_detail_level(
                        crate::services::notification_service::NotificationDetailLevel::from_str(
                            &level_str,
                        ),
                    );
                }
                svc.set_sound_enabled(
                    infra
                        .settings_repo
                        .get_bool(
                            crate::storage::repositories::settings_repo::keys::NOTIFICATION_SOUND,
                        )
                        .ok()
                        .flatten()
                        .unwrap_or(true),
                );
                Arc::new(svc)
            },
            section_service: services.section_service.clone(),
            conversation_pins: services.conversation_pins.clone(),
            device_service: services.device_service.clone(),
            conversation_repo: services.conversation_repo.clone(),
            sync_service: sync_service.clone(),
            channel_stream_service: Arc::new(
                crate::services::channel_stream::ChannelStreamService::new(
                    infra.grpc_pool.clone(),
                    infra.settings_repo.clone(),
                    channel_inbound,
                    sync_service,
                    services.message_service.clone(),
                    infra.event_bus.clone(),
                ),
            ),

            // State stores
            auth_state,
            chat_state: ChatState::new(),
            message_state: MessageState::new(),
            user_state: UserState::new(),
            offline_state: OfflineState::new(),
            #[cfg(feature = "jarvis")]
            jarvis_state: JarvisState::new(),
            event_bus: infra.event_bus.clone(),

            // Infrastructure
            grpc_pool: infra.grpc_pool.clone(),
            database: infra.database.clone(),
            directories: infra.directories.clone(),
            token_manager: infra.token_manager.clone(),
            channel_repo: infra.channel_repo.clone(),

            // Crypto
            // key_store accessed via SignalService

            // Post-boot services (shared OnceLock with AppContext)
            group_fanout,
            #[cfg(feature = "media")]
            call_service,
            device_inbox_provider,
        }
    }

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.grpc_pool.try_channel().is_some()
    }

    /// Create a GIF client for server-side GIF search.
    pub fn gif_client(&self) -> Option<crate::network::grpc::GifClient> {
        if self.grpc_pool.is_connected() {
            Some(self.grpc_pool.gif_client())
        } else {
            None
        }
    }

    /// Fetch trending GIFs.
    ///
    /// Plain Basic-auth RPC: GifService has no anonymous-credential path on the
    /// server, so a mailbox token here bought nothing and burned the daily
    /// budget (OPEN-ITEMS §120).
    pub async fn trending_gifs(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, crate::error::NetworkError> {
        match self.gif_client() {
            Some(mut client) => client.trending(limit as i32).await,
            None => Err(crate::error::NetworkError::Unavailable),
        }
    }

    /// Search GIFs. Basic-auth, no mailbox token — see [`Self::trending_gifs`].
    pub async fn search_gifs(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, crate::error::NetworkError> {
        match self.gif_client() {
            Some(mut client) => client.search(query, limit as i32).await,
            None => Err(crate::error::NetworkError::Unavailable),
        }
    }

    /// Pre-fetch anonymous tokens for sealed sender messaging.
    ///
    /// Ensures tokens are available before the first SKDM distribution.
    /// Returns the number of mailbox tokens fetched, or 0 if already sufficient.
    pub async fn prefetch_tokens(
        &self,
        target_count: u32,
    ) -> Result<u32, crate::error::MailboxError> {
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

        let mut mailbox_client = self.grpc_pool.mailbox_client();
        let mut kt = self.grpc_pool.anonymous_kt_client();

        let fetched = self
            .token_manager
            .request_tokens(&mut mailbox_client, &mut kt, needed)
            .await?;
        tracing::info!("Pre-fetched {} anonymous tokens", fetched);
        Ok(fetched)
    }

    /// Spawn the token refill loop and the parked-send retry loop.
    ///
    /// Sends spend only from the pre-fetched pool
    /// (`TokenManager::claim_prefetched_token`) and never request tokens
    /// themselves; this loop is the only thing that tops the pool up, at a
    /// random interval, so the authenticated token request is not tied in
    /// time to the anonymous send that spends it (NDSS'21 §VI-F).
    ///
    /// Anon-group posts parked because the batch ran dry are retried by a
    /// second loop on its own independent jitter, for the same reason: a
    /// retry right after a refill would spend the fresh tokens next to the
    /// request that fetched them. Aborting the returned handle stops both.
    pub fn spawn_token_refill(&self) -> tokio::task::JoinHandle<()> {
        /// Refill to this many claimable tokens…
        ///
        /// Sized for the largest burst a send makes: an anon-group post
        /// spends one token per member it has no session with yet (first
        /// contact), so a fresh join of an N-member group spends N at once.
        /// Members past the batch are parked and retried below.
        // fixed batch; size to the group roster if the parked
        // backlog bites. Server daily budget is 48 + 2/conversation.
        const BATCH: u32 = 20;
        /// …once fewer than this remain.
        const LOW_WATER: u32 = 10;

        let token_manager = self.token_manager.clone();
        let grpc_pool = self.grpc_pool.clone();
        let refill = async move {
            // Session startup, never Shield entry or a send, schedules the
            // first refill. Avoid a 5–15 minute empty pool after sign-in.
            initial_token_refill_sleep().await;
            loop {
                let mut client = grpc_pool.mailbox_client();
                let mut kt = grpc_pool.anonymous_kt_client();
                match token_manager.available_token_count() {
                    Ok(current) if current < LOW_WATER => {
                        let now =
                            top_up(&token_manager, &mut client, &mut kt, current, BATCH).await;
                        tracing::info!(had = current, now, "Token refill: pool topped up");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        trace_token_refill_failure(&e);
                        tracing::warn!("Token refill: could not count tokens: {}", e);
                    }
                }
                jittered_sleep().await;
            }
        };

        let anon_group_service = self.anon_group_service.clone();
        let retry_parked = async move {
            // Retained join posts survive restart; inspect them soon after
            // startup instead of waiting for the first 5–15 minute poll.
            initial_token_refill_sleep().await;
            loop {
                if let Err(e) = anon_group_service.retry_parked_sends().await {
                    tracing::warn!("Parked anon-group send retry failed: {}", e);
                }
                jittered_sleep().await;
            }
        };

        tokio::spawn(async move {
            tokio::join!(refill, retry_parked);
        })
    }

    /// Decline an incoming call.
    ///
    /// Notifies the server that the call was declined without joining.
    pub async fn decline_call(&self, call_id: &str) -> Result<(), crate::error::NetworkError> {
        let mut call_client = self.grpc_pool.call_client();
        call_client.leave_call(call_id).await
    }

    /// Spawn the prekey replenishment background task.
    ///
    /// Periodically checks the server for available prekey count and
    /// generates + uploads new prekeys when below threshold. Also checks
    /// signed prekey age and rotates signed + Kyber prekeys when older
    /// than 48 hours (Signal standard). This maintains forward secrecy
    /// by ensuring one-time prekeys are always available and signed
    /// prekeys are fresh.
    ///
    /// Returns a JoinHandle that should be stored for graceful shutdown.
    pub fn spawn_prekey_replenishment(&self) -> tokio::task::JoinHandle<()> {
        use crate::crypto::signal::PreKeyManager;
        use std::time::Duration;

        /// How often to check prekey count (6 hours).
        const REPLENISH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
        /// Prekey count threshold — refill when below this.
        const PREKEY_THRESHOLD: i32 = 20;
        /// Number of new prekeys to generate per refill.
        const PREKEY_BATCH_SIZE: u32 = 100;
        /// Maximum backoff on failure.
        const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
        /// Signed prekey rotation threshold — 48 hours (Signal standard).
        const SIGNED_PREKEY_MAX_AGE_HOURS: u64 = 48;

        let grpc_pool = self.grpc_pool.clone();
        let signal_service = self.signal_service.clone();
        let settings_repo = signal_service.settings_repo.clone();
        let key_store = signal_service.key_store.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(30);

            loop {
                // Get device ID from settings (follows sender cert pre-fetch pattern)
                let device_id: Option<u32> = settings_repo
                    .get(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
                    .ok()
                    .flatten();

                let Some(did) = device_id else {
                    tracing::debug!("PreKey replenishment: no device ID set, skipping cycle");
                    tokio::time::sleep(REPLENISH_INTERVAL).await;
                    continue;
                };

                // Check server stats (authoritative source per CTO)
                let mut auth_client = grpc_pool.auth_client();
                match auth_client.get_pre_key_stats(did).await {
                    Ok(stats) => {
                        if stats.should_refill || stats.available_count < PREKEY_THRESHOLD {
                            tracing::info!(
                                available = stats.available_count,
                                threshold = PREKEY_THRESHOLD,
                                should_refill = stats.should_refill,
                                "PreKey count low, generating new batch"
                            );

                            match signal_service
                                .refresh_prekeys(&mut auth_client, PREKEY_BATCH_SIZE)
                                .await
                            {
                                Ok(uploaded) => {
                                    tracing::info!(uploaded, "PreKey replenishment complete");
                                }
                                Err(e) => {
                                    tracing::warn!("PreKey refresh failed: {}", e);
                                    tokio::time::sleep(backoff).await;
                                    backoff = (backoff * 2).min(MAX_BACKOFF);
                                    continue;
                                }
                            }
                        } else {
                            tracing::debug!(
                                available = stats.available_count,
                                "PreKey count healthy, no replenishment needed"
                            );
                        }

                        // Check signed prekey age — rotate if older than 48h
                        let prekey_manager = PreKeyManager::new(key_store.clone());
                        match prekey_manager
                            .needs_signed_prekey_rotation(SIGNED_PREKEY_MAX_AGE_HOURS)
                        {
                            Ok(true) => {
                                tracing::info!(
                                    max_age_hours = SIGNED_PREKEY_MAX_AGE_HOURS,
                                    "Signed prekey stale, rotating signed + Kyber prekey pair"
                                );
                                let mut rotation_client = grpc_pool.auth_client();
                                match signal_service
                                    .rotate_signed_prekey(&mut rotation_client)
                                    .await
                                {
                                    Ok(()) => {
                                        tracing::info!("Signed prekey rotation complete");
                                    }
                                    Err(e) => {
                                        tracing::warn!("Signed prekey rotation failed: {}", e);
                                        // Don't block the main cycle — retry next iteration
                                    }
                                }
                            }
                            Ok(false) => {
                                tracing::debug!("Signed prekey still fresh, no rotation needed");
                            }
                            Err(e) => {
                                tracing::warn!("Failed to check signed prekey age: {}", e);
                            }
                        }

                        backoff = Duration::from_secs(30); // Reset backoff on success
                    }
                    Err(e) => {
                        tracing::warn!("Failed to check prekey stats: {}", e);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }

                // Wait for next cycle
                tokio::time::sleep(REPLENISH_INTERVAL).await;
            }
        })
    }

    /// Spawn the mailbox key-rolling background task (L4 anonymity).
    ///
    /// One loop, one interval ([`DEFAULT_CHECK_INTERVAL_SECS`], the same sleep
    /// DMs already used — no separate group timer). Each tick rotates every
    /// mailbox [`KeyRollingService::scheduler_due`] returns: DM and group.
    /// A fresh token-bound mailbox is minted anonymously and saved alongside
    /// the old one. Sends target the newest; the old one is polled until its
    /// TTL.
    ///
    /// Returns a JoinHandle that should be stored for graceful shutdown.
    pub fn spawn_key_rolling(&self) -> tokio::task::JoinHandle<()> {
        use crate::mailbox::dm::DmConversationManager;
        use crate::mailbox::group::GroupMailboxManager;
        use crate::mailbox::key_rolling::{
            DEFAULT_CHECK_INTERVAL_SECS, KeyRollingService, RotationTarget,
        };
        use crate::proto::content_v1;
        use crate::services::message::SendDmContentParams;
        use crate::util::time::TimestampNanos;
        use std::time::Duration;

        let grpc_pool = self.grpc_pool.clone();
        let database = self.database.clone();
        let token_manager = self.token_manager.clone();
        let message_service = self.message_service.clone();
        let mailbox_manager = self.mailbox_manager.clone();
        let channel_repo = self.channel_repo.clone();
        let signal_service = self.signal_service.clone();

        tokio::spawn(async move {
            let dm_manager = Arc::new(DmConversationManager::new(database.clone()));
            let group_manager = Arc::new(GroupMailboxManager::new(database.clone()));
            let svc =
                KeyRollingService::new(token_manager, dm_manager, group_manager, grpc_pool.clone());

            loop {
                tokio::time::sleep(Duration::from_secs(DEFAULT_CHECK_INTERVAL_SECS)).await;

                for target in svc.scheduler_due() {
                    match target {
                        RotationTarget::Dm(peer_id) => {
                            let notif = match svc.rotate_dm_mailbox(&peer_id).await {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!(peer_id = %peer_id, "KeyRolling: DM rotation failed: {}", e);
                                    continue;
                                }
                            };

                            // Announce the new mailbox to the peer's CURRENT mailbox so
                            // they switch to depositing into the rotated one.
                            let content = content_v1::Content {
                                content: Some(content_v1::content::Content::DmMailboxRotation(
                                    content_v1::DmMailboxRotation {
                                        new_mailbox_pk: notif.new_mailbox_public_key.to_vec(),
                                        old_mailbox_pk: vec![],
                                        timestamp: TimestampNanos::now().as_u64(),
                                    },
                                )),
                                ephemeral_reply_to: None,
                                timestamp: TimestampNanos::now().as_u64(),
                                padding: vec![],
                            };

                            let mut auth_client = grpc_pool.auth_client();
                            match message_service
                                .send_dm_content(SendDmContentParams {
                                    auth_client: &mut auth_client,
                                    peer_id: &peer_id,
                                    content,
                                    mailbox_manager: &mailbox_manager,
                                    grpc_pool: Some(&grpc_pool),
                                    attachment_ids: None,
                                    channel_id: None,
                                })
                                .await
                            {
                                Ok(_) => {
                                    tracing::info!(peer_id = %peer_id, "KeyRolling: rotation announced to peer");
                                }
                                Err(e) => {
                                    tracing::warn!(peer_id = %peer_id, "KeyRolling: failed to announce rotation: {}", e);
                                }
                            }
                        }
                        RotationTarget::Group(group_id) => {
                            let notif = match svc.rotate_group_mailbox(&group_id).await {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!(group_id = %group_id, "KeyRolling: group rotation failed: {}", e);
                                    continue;
                                }
                            };
                            announce_rotated_group_mailbox(
                                &group_id,
                                &notif.new_mailbox_public_key,
                                &svc,
                                &channel_repo,
                                &signal_service,
                                &message_service,
                                &grpc_pool,
                            )
                            .await;
                        }
                    }
                }
            }
        })
    }

    /// Initialize the device inbox provider if not already set.
    ///
    /// This is safe to call multiple times — uses `get_or_init` internally.
    /// Called automatically by `start_inbox_provider()`.
    pub fn ensure_device_inbox_provider(&self) {
        use crate::inbox::DeviceInboxProvider;
        use crate::mailbox::AnonDmPoller;

        self.device_inbox_provider.get_or_init(|| {
            let message_client = self.grpc_pool.message_client();
            let mailbox_client = self.grpc_pool.anonymous_mailbox_client();
            let pipeline =
                std::sync::Arc::new(crate::network::validation::MessageValidationPipeline::new());

            let anon_dm_poller =
                std::sync::Arc::new(AnonDmPoller::new(self.anon_dm_service.anon_dm_repo()));

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
                .with_anon_dm_polling(anon_dm_poller, self.anon_dm_service.pairwise_service())
                .with_content_handoff(
                    self.anon_dm_service.clone(),
                    self.anon_group_service.clone(),
                    self.event_bus.clone(),
                    self.database.clone(),
                ),
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
    pub async fn start_inbox_provider(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::inbox::InboxEvent>> {
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
}

/// Tell every device of every member who is still in `group_id` the new
/// mailbox address. A departed member is not a recipient: local membership
/// is the entitlement, and their leftover mailbox row is deleted first.
///
/// No identified RPC. Encryption uses the cached sender certificate and
/// sessions we already hold (`encrypt_message_anonymous`); a missing session
/// or certificate skips that device rather than fetching a prekey bundle.
/// Delivery is the anonymous mailbox deposit (no account on the wire) and,
/// when we hold the member's profile key, a UAK sealed send to that device's
/// inbox so every device converges. A rejected or missing UAK is not retried
/// over Basic auth — that would stamp our user id on the rotation.
async fn announce_rotated_group_mailbox(
    group_id: &crate::domain::ChannelId,
    new_mailbox_pk: &[u8; 32],
    svc: &crate::mailbox::key_rolling::KeyRollingService,
    channel_repo: &crate::storage::ChannelRepository,
    signal_service: &crate::services::signal::SignalService,
    message_service: &crate::services::message::MessageService,
    grpc_pool: &crate::network::grpc::client_pool::GrpcClientPool,
) {
    use crate::mailbox::key_rolling::group_mailbox_rotation_plaintext;
    use crate::proto::chat_v1::SealedSenderMessage;
    use crate::proto::mailbox_v1::EnvelopeType;
    use crate::util::time::TimestampNanos;

    let me = match message_service.settings_repo.current_user_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(group_id = %group_id, error = %e, "KeyRolling: not announcing group rotation; no current user");
            return;
        }
    };
    let members = match channel_repo.list_members(group_id) {
        Ok(members) => members,
        Err(e) => {
            tracing::warn!(group_id = %group_id, error = %e, "KeyRolling: not announcing group rotation; membership unknown");
            return;
        }
    };
    if members.is_empty() {
        tracing::warn!(group_id = %group_id, "KeyRolling: not announcing group rotation; no current members");
        return;
    }
    let member_ids: Vec<crate::domain::UserId> = members.iter().map(|m| m.user_id).collect();
    if let Err(e) = svc.drop_departed_member_mailboxes(group_id, &member_ids) {
        tracing::warn!(group_id = %group_id, error = %e, "KeyRolling: could not drop departed member mailboxes");
    }

    let mut session_devices = Vec::new();
    for member_id in &member_ids {
        if *member_id == me {
            continue;
        }
        match signal_service.session_devices(&member_id.to_string()).await {
            Ok(devices) => {
                for device_id in devices {
                    session_devices.push((*member_id, device_id));
                }
            }
            Err(e) => {
                tracing::warn!(group_id = %group_id, member_id = %member_id, error = %e, "KeyRolling: could not list local sessions");
            }
        }
    }
    let recipients =
        crate::mailbox::key_rolling::group_rotation_recipients(&me, &member_ids, &session_devices);
    if recipients.is_empty() {
        tracing::info!(group_id = %group_id, "KeyRolling: group rotated; no remaining member device to tell");
        return;
    }

    let mailboxes = match svc.member_mailbox_keys(group_id) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!(group_id = %group_id, error = %e, "KeyRolling: could not load member mailboxes");
            std::collections::HashMap::new()
        }
    };
    let plaintext = group_mailbox_rotation_plaintext(group_id, new_mailbox_pk);
    let mut anon_mailbox = grpc_pool.anonymous_mailbox_client();
    let mut anon_messages = grpc_pool.anonymous_message_client();
    let mut told = 0usize;

    for recipient in recipients {
        let ciphertext = match signal_service
            .encrypt_message_anonymous(
                &recipient.user_id.to_string(),
                recipient.device_id,
                &plaintext,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(
                    group_id = %group_id,
                    member_id = %recipient.user_id,
                    device_id = recipient.device_id,
                    error = %e,
                    "KeyRolling: skipped a member device; not fetching a prekey bundle"
                );
                continue;
            }
        };

        let mut reached = false;
        if let Some(mailbox_pk) = mailboxes.get(&recipient.user_id) {
            match anon_mailbox
                .send_to_ephemeral_mailbox(
                    mailbox_pk.to_vec(),
                    ciphertext.clone(),
                    EnvelopeType::SealedSender as i32,
                )
                .await
            {
                Ok(_) => reached = true,
                Err(e) => {
                    tracing::warn!(
                        group_id = %group_id,
                        member_id = %recipient.user_id,
                        device_id = recipient.device_id,
                        error = %e,
                        "KeyRolling: anonymous mailbox announce failed"
                    );
                }
            }
        }

        // UAK on the anonymous channel names the recipient account and not us.
        // No Basic-auth fallback: that would link our identity to the rotation.
        let uak = message_service
            .user_repo
            .get_friend_profile_key(&me, &recipient.user_id)
            .ok()
            .flatten()
            .map(|pk| crate::crypto::derive_uak(&pk));
        if let Some(uak) = uak {
            let sealed = SealedSenderMessage {
                sealed_body: ciphertext,
                server_key_id: signal_service.get_server_key_id(),
                timestamp: TimestampNanos::now_anonymized().as_u64(),
            };
            match anon_messages
                .send_sealed_messages_with_uak(
                    &recipient.user_id.to_string(),
                    vec![crate::proto::chat_v1::SealedDeviceMessage {
                        device_id: recipient.device_id,
                        registration_id: 0,
                        sealed_message: Some(sealed),
                    }],
                    crate::proto::chat_v1::SealedDelivery::NamedDevices,
                    &uuid::Uuid::new_v4().to_string(),
                    &uak,
                )
                .await
            {
                Ok(_) => reached = true,
                Err(e) => {
                    tracing::warn!(
                        group_id = %group_id,
                        member_id = %recipient.user_id,
                        device_id = recipient.device_id,
                        error = %e,
                        "KeyRolling: UAK announce failed; not falling back to identified delivery"
                    );
                }
            }
        } else if !reached {
            tracing::warn!(
                group_id = %group_id,
                member_id = %recipient.user_id,
                device_id = recipient.device_id,
                "KeyRolling: member device not told; no mailbox and no profile key, and an identified send is refused"
            );
        }

        if reached {
            told += 1;
        }
    }

    tracing::info!(group_id = %group_id, devices = told, "KeyRolling: group rotation announced");
}

/// Wait `BASE` plus up to `JITTER`, uniformly random.
pub(crate) async fn jittered_sleep() {
    use rand::Rng;
    const BASE_SECS: u64 = 5 * 60;
    const JITTER_SECS: u64 = 10 * 60;
    let wait = BASE_SECS + rand::rng().random_range(0..=JITTER_SECS);
    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
}

/// Startup is independent of Shield and sends. The shorter initial jitter
/// prepares normal sessions early; subsequent refills retain their own cadence.
async fn initial_token_refill_sleep() {
    use rand::Rng;
    let seconds = rand::rng().random_range(10..=30);
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
}

/// Top the mailbox token pool up from `have` to `target` in rounds (the
/// server grants at most 10 per request). Returns the claimable count reached.
async fn top_up(
    tm: &TokenManager,
    client: &mut crate::network::grpc::MailboxClient,
    kt: &mut crate::network::grpc::AnonymousKtClient,
    mut have: u32,
    target: u32,
) -> u32 {
    while have < target {
        match tm.request_tokens(client, kt, target - have).await {
            Ok(0) => break,
            Ok(n) => have += n,
            Err(e) => {
                trace_token_refill_failure(&e);
                tracing::warn!("Token refill failed: {}", e);
                break;
            }
        }
    }
    have
}

// Only constant targets cross the log redactor; never include error payloads.
fn trace_token_refill_failure(error: &crate::error::MailboxError) {
    use crate::error::MailboxError;
    match error {
        MailboxError::NoTokensAvailable => {
            tracing::warn!(target: "token_refill::no_tokens", "token refill failed")
        }
        MailboxError::QuotaExhausted => {
            tracing::warn!(target: "token_refill::quota", "token refill failed")
        }
        MailboxError::Network(_) => {
            tracing::warn!(target: "token_refill::network", "token refill failed")
        }
        MailboxError::Storage(_) => {
            tracing::warn!(target: "token_refill::storage", "token refill failed")
        }
        MailboxError::InvalidServerKey(_) => {
            tracing::warn!(target: "token_refill::invalid_manifest", "token refill failed")
        }
        MailboxError::BlindSignature(_) => {
            tracing::warn!(target: "token_refill::blind_signature", "token refill failed")
        }
        _ => tracing::warn!(target: "token_refill::other", "token refill failed"),
    }
}

#[cfg(test)]
mod initial_token_refill_tests {
    #[tokio::test(start_paused = true)]
    async fn initial_refill_is_early_but_jittered() {
        let mut wait = Box::pin(super::initial_token_refill_sleep());
        assert!(futures::poll!(&mut wait).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(9)).await;
        assert!(futures::poll!(&mut wait).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        wait.await;
    }
}
