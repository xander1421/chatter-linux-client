//! Application context.
//!
//! Central dependency container that initializes all application subsystems
//! in the correct order and provides access to them.

use std::sync::Arc;

use crate::error::{NetworkError, StorageError, SystemError};
#[cfg(feature = "desktop-media")]
use crate::services::CallWorkerHandle;
use crate::mailbox::group::GroupFanout;
use crate::mailbox::anon_dm::AnonDmPoller;
use crate::inbox::provider::DeviceInboxProvider;

use super::infrastructure::InfrastructureContainer;
use super::services::ServiceContainer;
use super::core_services::CoreServices;

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
/// 3. **CoreServices**: Services + state stores (UI-facing API)
///
/// UI code accesses everything through `core_services()`.
pub struct AppContext {
    infrastructure: Arc<InfrastructureContainer>,
    services: Arc<ServiceContainer>,
    core_services: Arc<CoreServices>,

    // Post-boot initialization (OnceLock) - shared with CoreServices
    device_inbox_provider: Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>>,
    #[cfg(feature = "desktop-media")]
    call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
}

impl AppContext {
    /// Create a new AppContext, initializing all subsystems.
    pub fn new(data_dir: Option<String>) -> Result<Self, ContextError> {
        let infrastructure = Arc::new(InfrastructureContainer::new(data_dir)?);
        
        // Create shared OnceLocks for post-boot services (shared with CoreServices)
        let device_inbox_provider = Arc::new(std::sync::OnceLock::new());
        #[cfg(feature = "desktop-media")]
        let call_service = Arc::new(std::sync::OnceLock::new());

        let services = Arc::new(ServiceContainer::new(&infrastructure)?);

        let core_services = Arc::new(CoreServices::new(
            &infrastructure,
            &services,
            infrastructure.group_fanout.clone(),
            #[cfg(feature = "desktop-media")]
            call_service.clone(),
            device_inbox_provider.clone(),
        ));

        tracing::info!("GIF search enabled via server-side gRPC service");

        Ok(Self {
            infrastructure,
            services,
            core_services,
            device_inbox_provider,
            #[cfg(feature = "desktop-media")]
            call_service,
        })
    }

    /// Create a new AppContext with optional overrides.
    ///
    /// If `server_url` is `Some(url)`, that URL is used instead of the
    /// `CHATTER_SERVER_URL` environment variable or the built-in default.
    /// If `data_dir` is `Some(path)`, that path is used as the base directory
    /// for config, data, and cache (required on Android).
    pub fn with_config(server_url: Option<String>, data_dir: Option<String>) -> Result<Self, ContextError> {
        if let Some(url) = server_url {
            // SAFETY: Called before any threads are spawned (single-threaded init).
            unsafe { std::env::set_var(super::infrastructure::SERVER_URL_ENV, &url); }
        }
        Self::new(data_dir)
    }

    /// Get the UI context - the primary API for UI code.
    pub fn core_services(&self) -> Arc<CoreServices> {
        self.core_services.clone()
    }

    /// Initialize the device inbox provider (call after grpc connect).
    pub fn init_device_inbox_provider(&self) {
        self.device_inbox_provider.get_or_init(|| {
            let message_client = self.infrastructure.grpc_pool.message_client();
            let mailbox_client = self.infrastructure.grpc_pool.anonymous_mailbox_client();
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
                    Some(self.core_services.event_bus.clone()),
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
        self.infrastructure.group_fanout.get_or_init(|| {
            let mailbox_client = self.infrastructure.grpc_pool.anonymous_mailbox_client();
            Arc::new(GroupFanout::new(
                self.infrastructure.group_mailbox_manager.clone(),
                Arc::new(tokio::sync::Mutex::new(mailbox_client)),
            ))
        });
    }

    /// Get the group fanout.
    pub fn get_group_fanout(&self) -> Option<Arc<GroupFanout>> {
        self.infrastructure.group_fanout.get().cloned()
    }

    /// Initialize the call worker (call after grpc connect).
    #[cfg(feature = "desktop-media")]
    pub fn init_call_service(&self) {
        self.call_service.get_or_init(|| {
            CallWorkerHandle::spawn(self.infrastructure.grpc_pool.call_client())
        });
    }

    /// Get the call worker handle.
    #[cfg(feature = "desktop-media")]
    pub fn get_call_service(&self) -> Option<CallWorkerHandle> {
        self.call_service.get().cloned()
    }

    /// Spawn the DM bootstrap handler.
    pub fn spawn_dm_bootstrap_handler(&self) -> tokio::task::JoinHandle<()> {
        crate::inbox::streaming::spawn_dm_bootstrap_handler_from_bus(
            self.infrastructure.database.clone(),
            self.infrastructure.grpc_pool.clone(),
            &self.core_services.event_bus,
        )
    }

    /// Spawn the domain event handlers.
    pub fn spawn_event_handlers(&self) -> tokio::task::JoinHandle<()> {
        use crate::events::EventHandlers;

        let handlers = EventHandlers::new(
            self.core_services.message_state.clone(),
            self.core_services.chat_state.clone(),
            self.core_services.pinned_state.clone(),
            self.core_services.user_state.clone(),
            self.core_services.presence_state.clone(),
            self.core_services.streaming_state.clone(),
            self.core_services.offline_state.clone(),
        )
        .with_group_coordinator(self.infrastructure.group_coordinator.clone())
        .with_message_service(self.core_services.message_service.clone())
        .with_anon_group_service(self.services.anon_group_service.clone())
        .with_anon_dm_service(self.services.anon_dm_service.clone());

        handlers.spawn(self.core_services.event_bus.clone())
    }
    
    /// Start the channel event stream service.
    ///
    /// This starts the background service that owns the gRPC stream,
    /// handles reconnection, and emits domain events.
    pub fn start_channel_event_stream(&self) -> tokio::task::JoinHandle<()> {
        self.services.channel_event_service.clone().start_event_stream()
    }
    
    /// Restart the channel event stream service.
    ///
    /// This is needed after re-login to ensure the stream uses the new
    /// authentication credentials. Returns a new task handle.
    pub fn restart_channel_event_stream(&self) -> tokio::task::JoinHandle<()> {
        // The service handles stream lifecycle internally with retry logic
        // Just start a new instance - the old one will eventually stop
        // when it fails to authenticate with old credentials
        self.services.channel_event_service.clone().start_event_stream()
    }
    
    /// Start the channel bootstrap service.
    ///
    /// This starts the background service that listens for GroupMailboxReceived
    /// events and flushes pending messages to new members.
    pub fn start_channel_bootstrap_service(&self) -> tokio::task::JoinHandle<()> {
        self.services.channel_bootstrap_service.start(
            self.core_services.event_bus.subscribe_domain()
        )
    }
}
