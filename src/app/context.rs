//! Application context.
//!
//! Central dependency container that initializes all application subsystems
//! in the correct order and provides access to them.

use std::sync::Arc;

use crate::error::{NetworkError, StorageError, SystemError};
use crate::services::CallWorkerHandle;
use crate::mailbox::group::GroupFanout;
use crate::mailbox::anon_dm::AnonDmPoller;
use crate::inbox::provider::DeviceInboxProvider;

use super::infrastructure::InfrastructureContainer;
use super::services::ServiceContainer;
use super::ui_context::UIContext;

/// Application context error.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// System initialization failed.
    #[error("system error: {0}")]
    System(#[from] SystemError),

    /// Storage initialization failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Network initialization failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),
}

/// Application context holding all initialized dependencies.
///
/// This is the central dependency container for the application. It composes
/// three layers:
///
/// 1. **InfrastructureContainer**: Storage, crypto, network (low-level)
/// 2. **ServiceContainer**: Pre-built services (business logic)
/// 3. **UIContext**: Services + state stores (UI-facing API)
///
/// UI code accesses everything through `ui_context()`.
pub struct AppContext {
    infrastructure: Arc<InfrastructureContainer>,
    services: Arc<ServiceContainer>,
    ui_context: Arc<UIContext>,

    // Post-boot initialization (OnceLock) - shared with UIContext
    device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
    group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
    call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
}

impl AppContext {
    /// Create a new AppContext, initializing all subsystems.
    pub fn new() -> Result<Self, ContextError> {
        let infrastructure = Arc::new(InfrastructureContainer::new()?);
        let services = Arc::new(ServiceContainer::new(&infrastructure)?);

        // Create shared OnceLocks for post-boot services (shared with UIContext)
        let device_inbox_provider = Arc::new(std::sync::OnceLock::new());
        let group_fanout = Arc::new(std::sync::OnceLock::new());
        let call_service = Arc::new(std::sync::OnceLock::new());

        let ui_context = Arc::new(UIContext::new(
            &infrastructure,
            &services,
            group_fanout.clone(),
            call_service.clone(),
            device_inbox_provider.clone(),
        ));

        tracing::info!("GIF search enabled via server-side gRPC service");

        Ok(Self {
            infrastructure,
            services,
            ui_context,
            device_inbox_provider,
            group_fanout,
            call_service,
        })
    }

    /// Get the UI context - the primary API for UI code.
    pub fn ui_context(&self) -> Arc<UIContext> {
        self.ui_context.clone()
    }

    /// Initialize the device inbox provider (call after grpc connect).
    pub fn init_device_inbox_provider(&self) {
        self.device_inbox_provider.get_or_init(|| {
            let message_client = crate::network::grpc::message_client::MessageClient::new(
                self.infrastructure.grpc_pool.channel(),
                self.infrastructure.grpc_pool.auth_clone(),
            );
            let mailbox_client = crate::network::grpc::mailbox_client::MailboxClient::new(
                self.infrastructure.grpc_pool.channel(),
                self.infrastructure.grpc_pool.auth_clone(),
            );
            let pipeline = Arc::new(crate::network::validation::MessageValidationPipeline::new());

            // Create anonymous DM poller for stranger messaging
            let anon_dm_poller = Arc::new(AnonDmPoller::new(
                self.infrastructure.anon_dm_repo.clone(),
            ));

            Arc::new(
                DeviceInboxProvider::with_event_bus(
                    crate::inbox::InboxConfig::default(),
                    Arc::new(tokio::sync::Mutex::new(message_client)),
                    Arc::new(tokio::sync::Mutex::new(mailbox_client)),
                    pipeline,
                    self.services.message_gateway.clone(),
                    self.infrastructure.database.clone(),
                    self.infrastructure.grpc_pool.clone(),
                    self.infrastructure.attachment_repo.clone(),
                    Some(self.ui_context.event_bus.clone()),
                )
                .with_anon_dm_polling(anon_dm_poller, self.services.pairwise_service.clone()),
            )
        });
    }

    /// Get the device inbox provider.
    pub fn get_device_inbox_provider(&self) -> Option<Arc<DeviceInboxProvider>> {
        self.device_inbox_provider.get().cloned()
    }

    /// Initialize the group fanout (call after grpc connect).
    pub fn init_group_fanout(&self) {
        self.group_fanout.get_or_init(|| {
            let mailbox_client = crate::network::grpc::mailbox_client::MailboxClient::new(
                self.infrastructure.grpc_pool.channel(),
                self.infrastructure.grpc_pool.auth_clone(),
            );
            Arc::new(GroupFanout::new(
                self.infrastructure.group_mailbox_manager.clone(),
                Arc::new(tokio::sync::Mutex::new(mailbox_client)),
            ))
        });
    }

    /// Get the group fanout.
    pub fn get_group_fanout(&self) -> Option<Arc<GroupFanout>> {
        self.group_fanout.get().cloned()
    }

    /// Initialize the call worker (call after grpc connect).
    pub fn init_call_service(&self) {
        self.call_service.get_or_init(|| {
            let call_client = crate::network::grpc::CallClient::new(
                self.infrastructure.grpc_pool.channel(),
                self.infrastructure.grpc_pool.auth_clone(),
            );
            CallWorkerHandle::spawn(call_client)
        });
    }

    /// Get the call worker handle.
    pub fn get_call_service(&self) -> Option<CallWorkerHandle> {
        self.call_service.get().cloned()
    }

    /// Spawn the DM bootstrap handler.
    pub fn spawn_dm_bootstrap_handler(&self) -> tokio::task::JoinHandle<()> {
        crate::inbox::streaming::spawn_dm_bootstrap_handler_from_bus(
            self.infrastructure.database.clone(),
            self.infrastructure.grpc_pool.clone(),
            &self.ui_context.event_bus,
        )
    }

    /// Spawn the domain event handlers.
    pub fn spawn_event_handlers(&self) -> tokio::task::JoinHandle<()> {
        use crate::events::EventHandlers;

        let handlers = EventHandlers::new(
            self.ui_context.message_state.clone(),
            self.ui_context.chat_state.clone(),
            self.ui_context.pinned_state.clone(),
            self.ui_context.user_state.clone(),
            self.ui_context.presence_state.clone(),
            self.ui_context.streaming_state.clone(),
            self.ui_context.offline_state.clone(),
        )
        .with_group_coordinator(self.infrastructure.group_coordinator.clone());

        handlers.spawn(self.ui_context.event_bus.clone())
    }
}
