//! Service container.
//!
//! Pre-built services that encapsulate business logic. Services are created once
//! at startup and stored as `Arc<T>` for efficient cloning.

use std::sync::Arc;

use crate::services::ai::{LocalAIActorHandle, LocalAIService};
use crate::services::jarvis::JarvisService;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
use crate::services::channel::ChannelService;
use crate::services::channel_bootstrap_service::ChannelBootstrapService;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::draft_service::DraftService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::message_gateway::MessageGateway;
use crate::services::pairwise::PairwiseService;
use crate::services::pin_service::PinService;
use crate::services::reaction_service::ReactionService;
use crate::services::session_service::SessionService;
use crate::services::signal_service::SignalService;
use crate::services::typing_service::TypingService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;

use crate::state::OrgState;

use super::infrastructure::InfrastructureContainer;
use super::ContextError;

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
    /// Reaction service for message reactions.
    pub reaction_service: Arc<ReactionService>,
    /// Draft service for message drafts.
    pub draft_service: Arc<DraftService>,
    /// Pin service for pinned messages.
    pub pin_service: Arc<PinService>,
    /// Typing service for typing indicators.
    pub typing_service: Arc<TypingService>,
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
    pub jarvis_service: Arc<JarvisService>,
    /// Section service for custom sidebar sections.
    pub section_service: Arc<SectionService>,
    /// Organization state (shared with AuthService for org context).
    pub org_state: OrgState,
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
        let mailbox_manager = Arc::new(MailboxManager::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
        ));

        // 3. Message service with group mailbox support and Sesame
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
            infra.sesame_manager.clone(),
            infra.search_index.clone(),
            infra.token_manager.clone(),
        ));

        // 4. Message gateway with Sesame
        let message_gateway = Arc::new(MessageGateway::with_sesame(
            (*signal_service).clone(),
            infra.message_repo.clone(),
            infra.sesame_manager.clone(),
        ));

        // 5. DM bootstrap service
        let dm_bootstrap_service = Arc::new(DmBootstrapService::new(
            message_service.clone(),
            infra.channel_repo.clone(),
            infra.grpc_pool.clone(),
            mailbox_manager.clone(),
            infra.database.clone(),
        ));

        // 6. Auth service (with org state for post-login org context)
        let org_state = OrgState::new();
        let auth_service = Arc::new(AuthService::new(
            infra.grpc_pool.clone(),
            infra.identity_manager.clone(),
            infra.prekey_manager.clone(),
            infra.user_repo.clone(),
            infra.settings_repo.clone(),
            infra.key_store.clone(),
            infra.auth_interceptor.clone(),
            org_state.clone(),
        ));

        // 7. User service
        let user_service = Arc::new(UserService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
            infra.user_repo.clone(),
        ));

        // 8. Channel service
        let channel_service = Arc::new(ChannelService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
            infra.channel_repo.clone(),
            signal_service.clone(),
            infra.group_coordinator.clone(),
        ));

        // 9. Session service
        let session_service = Arc::new(SessionService::new(
            infra.grpc_pool.clone(),
            infra.settings_repo.clone(),
        ));

        // 10. Attachment service
        let attachment_service = Arc::new(AttachmentService::new(infra.attachment_repo.clone(), infra.grpc_pool.clone()));

        // 11. Channel event service (for member join/leave)
        let channel_event_service = Arc::new(ChannelEventService::new(
            infra.message_repo.clone(),
            infra.channel_repo.clone(),
            infra.user_repo.clone(),
            infra.grpc_pool.clone(),
            infra.event_bus.clone(),
        ));

        // 12. AI service for AI integrations
        let ai_service = Arc::new(AIService::new(
            infra.settings_repo.clone(),
            infra.grpc_pool.clone(),
        ));

        // 13. Reaction service for message reactions
        let reaction_service = Arc::new(ReactionService::new(
            infra.settings_repo.clone(),
            infra.reaction_repo.clone(),
            infra.reaction_aggregator.clone(),
        ));

        // 14. Draft service for message drafts
        let draft_service = Arc::new(DraftService::new(
            infra.settings_repo.clone(),
            infra.draft_repo.clone(),
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

        // 20. Jarvis AI assistant service
        let jarvis_service = Arc::new(JarvisService::new(
            infra.jarvis_memory_repo.clone(),
            infra.jarvis_graph_repo.clone(),
            local_ai.clone(),
        ));

        // 21. Typing service for typing indicators
        let typing_service = Arc::new(TypingService::new(
            infra.settings_repo.clone(),
            infra.typing_aggregator.clone(),
        ));

        // 22. Channel bootstrap service for pending message flush
        let channel_bootstrap_service = Arc::new(ChannelBootstrapService::new(
            infra.grpc_pool.clone(),
            signal_service.clone(),
            infra.group_fanout.clone(),
            infra.group_mailbox_manager.clone(),
        ));

        // 23. Section service for custom sidebar sections
        let section_service = Arc::new(SectionService::new(
            infra.section_repo.clone(),
            infra.channel_repo.clone(),
            infra.anon_dm_repo.clone(),
        ));

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
            reaction_service,
            draft_service,
            pin_service,
            pairwise_service,
            anon_group_service,
            anon_dm_service,
            local_ai,
            local_ai_service,
            jarvis_service,
            typing_service,
            channel_bootstrap_service,
            section_service,
            org_state,
        })
    }
}
