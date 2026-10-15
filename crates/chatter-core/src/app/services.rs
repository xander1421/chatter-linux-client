//! Service container.
//!
//! Pre-built services that encapsulate business logic. Services are created once
//! at startup and stored as `Arc<T>` for efficient cloning.

use std::sync::Arc;

use crate::services::ai::{LocalAIActorHandle, LocalAIService};
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
use crate::services::channel::ChannelService;
use crate::services::channel_bootstrap_service::ChannelBootstrapService;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::conversation_pins::ConversationPins;
use crate::services::device_service::DeviceService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::ephemeral_service::EphemeralService;
use crate::services::gateway::MessageGateway;
#[cfg(feature = "jarvis")]
use crate::services::jarvis::{JarvisEngine, JarvisService, SharedJarvisEngine};
use crate::services::local_ai_service::HttpLocalAIService;
use crate::services::mailbox_manager::MailboxManager;
#[cfg(feature = "mesh")]
use crate::services::mesh::{MeshConfig, MeshDeps, MeshService};
use crate::services::message::MessageService;
use crate::services::pairwise::PairwiseService;
use crate::services::pin_service::PinService;
use crate::services::section_service::SectionService;
use crate::services::semantic_search::SemanticSearchService;
use crate::services::session_service::SessionService;
use crate::services::signal::SignalService;
use crate::services::user::UserService;

use super::ContextError;
use super::infrastructure::InfrastructureContainer;

/// Service container holding all pre-built services.
///
/// Services are created once during initialization and stored as `Arc<T>`.
/// This avoids per-call allocation overhead and ensures consistent service instances.
///
/// # Design
///
/// - Services are built from InfrastructureContainer dependencies
/// - All services are `Arc<T>` for cheap cloning
/// - Services encapsulate all repository access
/// - UI should call service methods, not access repos directly
pub struct ServiceContainer {
    /// Authentication service.
    pub auth_service: Arc<AuthService>,
    /// Signal Protocol service.
    pub signal_service: Arc<SignalService>,
    /// Message service (send, receive, edit, delete).
    pub message_service: Arc<MessageService>,
    /// Message gateway for encryption/decryption routing.
    pub message_gateway: Arc<MessageGateway>,
    /// DM bootstrap service for session recovery and channel establishment.
    pub dm_bootstrap_service: Arc<DmBootstrapService>,
    /// Mailbox manager for privacy-preserving DM messaging.
    pub mailbox_manager: Arc<MailboxManager>,
    /// User service for profile and friend operations.
    pub user_service: Arc<UserService>,
    /// Channel service for channel operations.
    pub channel_service: Arc<ChannelService>,
    /// Session service for session management.
    pub session_service: Arc<SessionService>,
    /// Attachment service for file operations.
    pub attachment_service: Arc<AttachmentService>,
    /// Channel event service for member join/leave.
    pub channel_event_service: Arc<ChannelEventService>,
    /// AI service for AI integrations.
    pub ai_service: Arc<AIService>,
    /// Ephemeral (disappearing-message) local-expiry engine.
    pub ephemeral_service: Arc<EphemeralService>,
    /// Pin service for pinned messages.
    pub pin_service: Arc<PinService>,
    /// Channel bootstrap service for pending message flush.
    pub channel_bootstrap_service: Arc<ChannelBootstrapService>,
    /// Pairwise encryption service (reusable Signal Protocol).
    pub pairwise_service: Arc<PairwiseService>,
    /// Anonymous group service for server-oblivious groups.
    pub anon_group_service: Arc<AnonGroupService>,
    /// Anonymous DM service for server-oblivious direct messages.
    pub anon_dm_service: Arc<AnonDmService>,
    /// Local AI actor for GPU-accelerated inference (optional feature).
    pub local_ai: LocalAIActorHandle,
    /// Local AI service (high-level API for summarization, smart replies).
    pub local_ai_service: Arc<LocalAIService>,
    /// Jarvis AI assistant service.
    /// The bitchat mesh, when built in. Serverless and independent of every
    /// other service here: it makes no RPC and shares no identity with them.
    #[cfg(feature = "mesh")]
    pub mesh_service: Arc<MeshService>,
    #[cfg(feature = "jarvis")]
    pub jarvis_service: Arc<JarvisService>,
    /// Jarvis inference engine (hot-swappable on provider change).
    #[cfg(feature = "jarvis")]
    pub jarvis_engine: SharedJarvisEngine,
    /// Section service for custom sidebar sections.
    pub section_service: Arc<SectionService>,
    pub conversation_pins: Arc<ConversationPins>,
    /// HTTP-backed local AI service (Ollama/OpenAI-compat chat, models, semantic search).
    pub http_local_ai_service: Arc<HttpLocalAIService>,
    /// Semantic search service for E2E encrypted channel search.
    pub semantic_search_service: Arc<SemanticSearchService>,
    /// Device service for multi-device management.
    pub device_service: Arc<DeviceService>,
    /// Unified conversation repository (available to services).
    pub(crate) conversation_repo: Arc<crate::storage::repositories::UnifiedConversationRepository>,
}

impl ServiceContainer {
    /// Create all services from infrastructure dependencies.
    ///
    /// Services are built in dependency order to ensure proper initialization.
    pub(crate) fn new(infra: &InfrastructureContainer) -> Result<Self, ContextError> {
        // 1. Signal service (foundational crypto)
        let signal_service = Arc::new(SignalService::new(
            infra.key_store.clone(),
            infra.settings_repo.clone(),
        ));

        // 2. Mailbox manager for DM privacy
        let mailbox_manager = Arc::new(MailboxManager::new(infra.grpc_pool.clone()));

        // 2b. Ephemeral (disappearing-message) local-expiry engine. Shared by
        // the send path (MessageService — arms the sender's copy), the receive
        // path (MessageGateway — arms the recipient's copy), and the periodic
        // reaper (ChatterClient::start — calls disappearing::reap_expired).
        let ephemeral_service = Arc::new(EphemeralService::new(
            infra.settings_repo.clone(),
            infra.ephemeral_repo.clone(),
        ));

        // 3. Message service with group mailbox support
        let message_service = Arc::new(MessageService::new(
            infra.grpc_pool.clone(),
            infra.database.clone(),
            infra.settings_repo.clone(),
            infra.message_repo.clone(),
            infra.channel_repo.clone(),
            infra.attachment_repo.clone(),
            infra.user_repo.clone(),
            signal_service.clone(),
            infra.group_mailbox_manager.clone(),
            infra.pending_dm_repo.clone(),
            infra.search_index.clone(),
            infra.token_manager.clone(),
            infra.event_bus.clone(),
            ephemeral_service.clone(),
            mailbox_manager.clone(),
        ));

        // 4. Message gateway
        let message_gateway = Arc::new(MessageGateway::with_ephemeral(
            (*signal_service).clone(),
            infra.message_repo.clone(),
            infra.channel_repo.clone(),
            infra.user_repo.clone(),
            infra.settings_repo.clone(),
            infra.ephemeral_repo.clone(),
            message_service.solicited_resends.clone(),
        ));

        // 5. DM bootstrap service
        let dm_bootstrap_service = Arc::new(DmBootstrapService::new(
            message_service.clone(),
            infra.grpc_pool.clone(),
            mailbox_manager.clone(),
        ));

        // 6. Auth service
        let auth_service = Arc::new(AuthService::new(
            infra.grpc_pool.clone(),
            infra.identity_manager.clone(),
            infra.prekey_manager.clone(),
            infra.user_repo.clone(),
            infra.settings_repo.clone(),
            infra.key_store.clone(),
            infra.auth_interceptor.clone(),
        ));

        // 7. User service
        let user_service = Arc::new(UserService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
            infra.user_repo.clone(),
            infra.anon_dm_repo.clone(),
            infra.anon_group_repo.clone(),
        ));

        // 8. Channel service, on the signed membership log
        let membership_log = Arc::new(crate::services::channel::membership_log::MembershipLog::new(
            Arc::new(crate::services::channel::membership_log::SignalLogBackend::new(
                infra.grpc_pool.clone(),
                signal_service.clone(),
                infra.settings_repo.clone(),
            )),
            Arc::new(crate::services::channel::membership_log::SignalKtVerifier::cached(
                infra.grpc_pool.clone(),
                signal_service.clone(),
                infra.settings_repo.clone(),
            )),
            crate::storage::repositories::verified_membership_repo::VerifiedMembershipRepository::new(
                infra.database.clone(),
            ),
        ));
        message_service.set_channel_state_sync(membership_log.clone());
        let channel_service = Arc::new(ChannelService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
            infra.channel_repo.clone(),
            infra.section_repo.clone(),
            signal_service.clone(),
            infra.group_coordinator.clone(),
            message_service.clone(),
            membership_log.clone(),
        ));

        // 9. Session service
        let session_service = Arc::new(SessionService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
            infra.key_store.clone(),
        ));

        // 10. Attachment service
        let attachment_service = Arc::new(AttachmentService::new(
            infra.attachment_repo.clone(),
            infra.grpc_pool.clone(),
        ));

        // 11. Channel event service (for member join/leave)
        let channel_event_service = Arc::new(ChannelEventService::new(
            infra.message_repo.clone(),
            infra.channel_repo.clone(),
            infra.user_repo.clone(),
            infra.grpc_pool.clone(),
            infra.event_bus.clone(),
            infra.settings_repo.clone(),
            membership_log.clone(),
        ));
        membership_log.set_observer(Arc::downgrade(&channel_event_service)
            as std::sync::Weak<dyn crate::services::channel::membership_log::LogObserver>);

        // 12. AI service for AI integrations
        let ai_service = Arc::new(AIService::new(
            infra.settings_repo.clone(),
            infra.grpc_pool.clone(),
        ));

        // 15. Pin service for pinned messages
        let pin_service = Arc::new(PinService::new(
            infra.database.clone(),
            message_service.clone(),
            attachment_service.clone(),
        ));

        // 16. Pairwise encryption service (reusable Signal Protocol for DM/groups)
        let pairwise_service = Arc::new(PairwiseService::new(
            signal_service.clone(),
            infra.grpc_pool.clone(),
        ));

        // 16. Anonymous group service for NDSS'21 style server-oblivious groups
        let anon_group_service = Arc::new(AnonGroupService::new(
            infra.settings_repo.clone(),
            infra.anon_group_repo.clone(),
            infra.anon_dm_repo.clone(),
            infra.attachment_repo.clone(),
            message_service.clone(),
            pairwise_service.clone(),
            mailbox_manager.clone(),
            infra.grpc_pool.clone(),
            infra.token_manager.clone(),
        ));

        // 17. Anonymous DM service for NDSS'21 style server-oblivious direct messages
        let anon_dm_service = Arc::new(AnonDmService::new(
            infra.settings_repo.clone(),
            infra.anon_dm_repo.clone(),
            infra.message_repo.clone(),
            infra.attachment_repo.clone(),
            pairwise_service.clone(),
            mailbox_manager.clone(),
            infra.grpc_pool.clone(),
            infra.token_manager.clone(),
        ));

        // 18. Local AI actor for GPU-accelerated inference
        let local_ai = LocalAIActorHandle::spawn();

        // 19. Local AI service (high-level API wrapping the actor)
        let local_ai_service = Arc::new(LocalAIService::new(
            local_ai.clone(),
            infra.settings_repo.clone(),
        ));

        // 19c. HTTP local AI service (Ollama backend + semantic search)
        let http_local_ai_service = Arc::new(HttpLocalAIService::new(
            infra.settings_repo.clone(),
            infra.vector_repo.clone(),
        ));
        let semantic_search_service = Arc::new(
            SemanticSearchService::new(
                infra.vector_repo.clone(),
                Arc::new(crate::local_ai::embeddings::EmbeddingGenerator::new(
                    crate::local_ai::ollama::OllamaBackend::default_local(),
                    &crate::local_ai::settings::LocalAISettings::default(),
                )),
            )
            // Hybrid retrieval: BM25 over messages_fts fused with vectors.
            .with_keyword_index(infra.search_index.clone(), infra.message_repo.clone()),
        );

        // The service owns both indexer tasks, so opt-out cancels queued work.
        http_local_ai_service
            .configure_indexing(semantic_search_service.clone(), infra.event_bus.clone());
        local_ai_service.attach_http_service(http_local_ai_service.clone());

        // 20. Jarvis AI assistant service, driven by the active local AI provider
        // The mesh. Constructed but not started — `start()` is the caller's,
        // so the radio does not come up merely because the app booted.
        #[cfg(feature = "mesh")]
        let mesh_service = {
            let db_path = infra.directories.database_path();
            let (mesh_storage, mesh_sealer) =
                crate::storage::mesh_store::sealed_store(infra.database.clone(), &db_path);
            let deps = MeshDeps {
                key_store: infra.key_store.clone(),
                binding_store: Arc::new(crate::storage::mesh_binding_store::MeshBindingStore::new(
                    infra.database.clone(),
                )),
                private_channels: Arc::new(
                    crate::services::mesh::private_channel::PrivateChannelStore::new(
                        infra.database.clone(),
                    ),
                ),
                identity_store: Arc::new(crate::storage::MeshIdentityStore::new(
                    infra.database.clone(),
                )),
                storage: Arc::new(mesh_storage),
                // Mesh messages are rows in the same unified message store as
                // every other conversation type. See `MeshTimeline::record` for
                // why they are stored at all — bitchat's stack is ephemeral by
                // design and this one deliberately is not.
                messages: infra.message_repo.clone(),
                deliveries: Arc::new(crate::storage::repositories::MeshDeliveryRepository::new(
                    infra.database.clone(),
                )),
                conversations: Arc::new(
                    crate::storage::repositories::MeshConversationRepository::new(
                        infra.database.clone(),
                    ),
                ),
                // Files carried by mesh messages are rows in the same attachment
                // table as every other conversation type's, for the same reason
                // the messages are: a mesh file is not a second kind of
                // attachment, it just never had a CDN behind it.
                attachments: infra.attachment_repo.clone(),
                // Never fatal. `from_keyring` hands back a sealer that reports
                // why it has no key rather than an error, and `MeshService::start`
                // declines to start on that basis. A locked keyring used to abort
                // initialization for the entire client here, so a Secret Service
                // that did not answer meant nobody could log in — over one
                // optional feature.
                sealer: Arc::new(mesh_sealer),
                // Relay traffic rides the Tor client the gRPC pool already owns,
                // on its own isolation group. Without the `onion` feature there is
                // no connector at all and the mesh runs radio-only.
                #[cfg(feature = "onion")]
                relay_connector: Some(Arc::new(
                    crate::services::mesh::tor_relay::TorRelayConnector::new(
                        infra.grpc_pool.onion_tor(),
                    ),
                )),
                #[cfg(not(feature = "onion"))]
                relay_connector: None,
                radio_factory: None,
            };
            let mut config = MeshConfig::default();
            if let Ok(Some(nickname)) =
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    infra.database.clone(),
                )
                .get_string(crate::storage::repositories::settings_repo::keys::MESH_NICKNAME)
            {
                config.nickname = nickname;
            }
            let service =
                MeshService::new(deps, config).map_err(|e| ContextError::Mesh(e.to_string()))?;
            let settings = crate::storage::repositories::settings_repo::SettingsRepository::new(
                infra.database.clone(),
            );
            use crate::storage::repositories::settings_repo::keys;
            if let Ok(Some(names)) = settings.get::<Vec<String>>(keys::MESH_MUTED_CHANNELS) {
                for name in names {
                    let _ = service.set_channel_muted(&name, true);
                }
            }
            if let Ok(Some(peers)) = settings.get::<Vec<String>>(keys::MESH_BLOCKED_PEERS) {
                for peer in peers {
                    service.set_peer_blocked(&chatter_mesh::ids::PeerId::from_str(&peer), true);
                }
            }
            if let Ok(Some(peers)) = settings.get::<Vec<String>>(keys::MESH_MUTED_PEERS) {
                for peer in peers {
                    service.set_peer_muted(&chatter_mesh::ids::PeerId::from_str(&peer), true);
                }
            }
            Arc::new(service)
        };

        #[cfg(feature = "jarvis")]
        let jarvis_engine = SharedJarvisEngine::new(JarvisEngine::from_settings(
            http_local_ai_service.settings(),
            local_ai.clone(),
        ));
        #[cfg(feature = "jarvis")]
        let jarvis_service = {
            // App-context bridge: live channel + friend names ground Jarvis
            // answers ("what did alex say", "summarize general").
            let ctx_channels = channel_service.clone();
            let ctx_users = user_service.clone();
            let app_context: crate::services::jarvis::service::AppContextFn = Arc::new(move || {
                use crate::domain::channel::ChannelType;
                let mut s = String::new();
                if let Ok(channels) = ctx_channels.get_channels_mru() {
                    let names: Vec<_> = channels
                        .iter()
                        .filter(|c| {
                            matches!(c.channel_type, ChannelType::Public | ChannelType::Private)
                                && !c.id.0.is_nil()
                        })
                        .take(15)
                        .map(|c| c.name.clone())
                        .collect();
                    if !names.is_empty() {
                        s.push_str(&format!("User's channels: {}.\n", names.join(", ")));
                    }
                }
                let friends = ctx_users.friend_usernames_local();
                if !friends.is_empty() {
                    s.push_str(&format!("User's friends: {}.\n", friends.join(", ")));
                }
                s
            });
            Arc::new(
                JarvisService::new(
                    infra.jarvis_memory_repo.clone(),
                    infra.jarvis_graph_repo.clone(),
                    jarvis_engine.clone(),
                    Some(semantic_search_service.clone()),
                )
                .with_app_context(app_context),
            )
        };

        // 22. Channel bootstrap service for pending message flush
        let channel_bootstrap_service = Arc::new(ChannelBootstrapService::new(
            infra.grpc_pool.clone(),
            signal_service.clone(),
            infra.group_fanout.clone(),
            infra.group_mailbox_manager.clone(),
        ));

        // 23. Local-only conversation pinning (settings-backed, never synced)
        let conversation_pins = Arc::new(ConversationPins::new(infra.settings_repo.clone()));

        // 23b. Section service for custom sidebar sections
        let section_service = Arc::new(SectionService::new(
            infra.section_repo.clone(),
            infra.unified_conversation_repo.clone(),
            conversation_pins.clone(),
        ));

        // 24. Device service for multi-device management
        let device_service = Arc::new(DeviceService::new(
            infra.settings_repo.clone(),
            channel_service.clone(),
            signal_service.clone(),
        ));

        // 25. Unified conversation repository
        let conversation_repo = infra.unified_conversation_repo.clone();

        tracing::info!("Services initialized");

        Ok(Self {
            auth_service,
            signal_service,
            message_service,
            message_gateway,
            dm_bootstrap_service,
            mailbox_manager,
            user_service,
            channel_service,
            session_service,
            attachment_service,
            channel_event_service,
            ai_service,
            pin_service,
            ephemeral_service,
            pairwise_service,
            anon_group_service,
            anon_dm_service,
            local_ai,
            local_ai_service,
            #[cfg(feature = "mesh")]
            mesh_service,
            #[cfg(feature = "jarvis")]
            jarvis_service,
            #[cfg(feature = "jarvis")]
            jarvis_engine,
            channel_bootstrap_service,
            section_service,
            conversation_pins,
            http_local_ai_service,
            semantic_search_service,
            device_service,
            conversation_repo,
        })
    }
}
