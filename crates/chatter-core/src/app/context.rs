//! Application context.
//!
//! Central dependency container that initializes all application subsystems
//! in the correct order and provides access to them.

use std::sync::Arc;

use crate::error::{NetworkError, StorageError, SystemError};
use crate::inbox::provider::DeviceInboxProvider;
use crate::mailbox::anon_dm::AnonDmPoller;
use crate::mailbox::group::GroupFanout;
#[cfg(feature = "media")]
use crate::services::CallWorkerHandle;
use crate::system::notification_sink::NotificationSink;

use super::core_services::CoreServices;
use super::infrastructure::InfrastructureContainer;
use super::services::ServiceContainer;

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

    /// The bitchat mesh failed to initialise.
    ///
    /// Its own error types live in the mesh crates and do not convert into the
    /// variants above — a sealer failure is `chatter_mesh_store::StoreError`, not
    /// chatter's `StorageError` — so folding it into `Storage` would report a mesh
    /// keyring problem as a database problem.
    #[cfg(feature = "mesh")]
    #[error("mesh error: {0}")]
    Mesh(String),
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
    #[cfg(feature = "media")]
    call_service: Arc<std::sync::OnceLock<CallWorkerHandle>>,
}

impl AppContext {
    /// Create a new AppContext, initializing all subsystems, against
    /// `CHATTER_SERVER_URL` or the built-in default server.
    pub fn new(data_dir: Option<String>) -> Result<Self, ContextError> {
        Self::with_config_and_sink(None, data_dir, None)
    }

    /// Create a new AppContext with optional overrides.
    ///
    /// If `server_url` is `Some(url)`, that URL is used instead of the
    /// `CHATTER_SERVER_URL` environment variable or the built-in default.
    /// If `data_dir` is `Some(path)`, that path is used as the base directory
    /// for config, data, and cache (required on Android).
    pub fn with_config(
        server_url: Option<String>,
        data_dir: Option<String>,
    ) -> Result<Self, ContextError> {
        Self::with_config_and_sink(server_url, data_dir, None)
    }

    /// Like `with_config` but allows injecting a platform notification sink.
    ///
    /// The URL is handed to the network pool explicitly: it used to travel
    /// through a process-wide `set_var`, which raced between clients built
    /// on different threads (parallel tests) and is unsound once threads run.
    pub fn with_config_and_sink(
        server_url: Option<String>,
        data_dir: Option<String>,
        notification_sink: Option<Arc<dyn NotificationSink>>,
    ) -> Result<Self, ContextError> {
        let server_url = super::infrastructure::resolve_server_url(server_url);
        let infrastructure = Arc::new(InfrastructureContainer::new(data_dir, &server_url)?);

        // Create shared OnceLocks for post-boot services (shared with CoreServices)
        let device_inbox_provider = Arc::new(std::sync::OnceLock::new());
        #[cfg(feature = "media")]
        let call_service = Arc::new(std::sync::OnceLock::new());

        let services = Arc::new(ServiceContainer::new(&infrastructure)?);

        let core_services = Arc::new(CoreServices::new(
            &infrastructure,
            &services,
            infrastructure.group_fanout.clone(),
            #[cfg(feature = "media")]
            call_service.clone(),
            device_inbox_provider.clone(),
            notification_sink,
        ));

        tracing::info!("GIF search enabled via server-side gRPC service");

        Ok(Self {
            infrastructure,
            services,
            core_services,
            device_inbox_provider,
            #[cfg(feature = "media")]
            call_service,
        })
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
            let anon_dm_poller =
                Arc::new(AnonDmPoller::new(self.infrastructure.anon_dm_repo.clone()));

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
                .with_anon_dm_polling(anon_dm_poller, self.services.pairwise_service.clone())
                .with_content_handoff(
                    self.services.anon_dm_service.clone(),
                    self.services.anon_group_service.clone(),
                    self.core_services.event_bus.clone(),
                    self.infrastructure.database.clone(),
                ),
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
                self.infrastructure.grpc_pool.clone(),
            ))
        });
    }

    /// Get the group fanout.
    pub fn get_group_fanout(&self) -> Option<Arc<GroupFanout>> {
        self.infrastructure.group_fanout.get().cloned()
    }

    /// Initialize the call worker (call after grpc connect).
    ///
    /// Also spawns the EventBus → orchestrator bridge so incoming-call offers
    /// reach the worker without UI-layer dispatch, and the orchestrator →
    /// `call_history` recorder that feeds the Calls screen.
    #[cfg(feature = "media")]
    pub fn init_call_service(&self) {
        let mut newly_created = false;
        self.call_service.get_or_init(|| {
            newly_created = true;
            CallWorkerHandle::spawn(self.infrastructure.grpc_pool.call_client())
        });
        if newly_created {
            if let Some(handle) = self.call_service.get().cloned() {
                let signaling = crate::services::call_signaling::CallSignalingService::new(
                    self.core_services.message_service.clone(),
                    self.core_services.channel_service.clone(),
                );
                crate::services::call_history_recorder::spawn_call_history_recorder(
                    &handle,
                    self.infrastructure.database.clone(),
                    self.core_services.message_service.clone(),
                );
                let _ = crate::services::call_worker::spawn_call_orchestrator_bridge(
                    &self.core_services.event_bus,
                    handle,
                    signaling,
                );
            }
        }
    }

    /// Get the call worker handle.
    #[cfg(feature = "media")]
    pub fn get_call_service(&self) -> Option<CallWorkerHandle> {
        self.call_service.get().cloned()
    }

    /// Spawn the DM bootstrap handler.
    pub fn spawn_dm_bootstrap_handler(&self) -> tokio::task::JoinHandle<()> {
        crate::inbox::streaming::spawn_dm_bootstrap_handler_from_bus(
            self.infrastructure.database.clone(),
            self.infrastructure.grpc_pool.clone(),
            self.core_services.message_service.clone(),
            &self.core_services.event_bus,
        )
    }

    /// Spawn the domain event handlers.
    pub fn spawn_event_handlers(&self) -> tokio::task::JoinHandle<()> {
        use crate::events::EventHandlers;

        let handlers = EventHandlers::new(
            self.core_services.message_state.clone(),
            self.core_services.chat_state.clone(),
        )
        .with_group_coordinator(self.infrastructure.group_coordinator.clone())
        .with_message_service(self.core_services.message_service.clone())
        .with_anon_group_service(self.services.anon_group_service.clone())
        .with_anon_dm_service(self.services.anon_dm_service.clone())
        .with_signal_service(self.core_services.signal_service.clone());

        handlers.spawn(self.core_services.event_bus.clone())
    }

    /// Start the channel event stream service.
    ///
    /// This starts the background service that owns the gRPC stream,
    /// handles reconnection, and emits domain events.
    pub fn start_channel_event_stream(&self) -> tokio::task::JoinHandle<()> {
        self.services
            .channel_event_service
            .clone()
            .start_event_stream()
    }

    /// Restart the channel event stream service.
    ///
    /// This is needed after re-login to ensure the stream uses the new
    /// authentication credentials. Returns a new task handle.
    pub fn restart_channel_event_stream(&self) -> tokio::task::JoinHandle<()> {
        // The service handles stream lifecycle internally with retry logic
        // Just start a new instance - the old one will eventually stop
        // when it fails to authenticate with old credentials
        self.services
            .channel_event_service
            .clone()
            .start_event_stream()
    }

    /// Start the channel bootstrap service.
    ///
    /// This starts the background service that listens for GroupMailboxReceived
    /// events and flushes pending messages to new members.
    pub fn start_channel_bootstrap_service(&self) -> tokio::task::JoinHandle<()> {
        self.services
            .channel_bootstrap_service
            .start(self.core_services.event_bus.subscribe_domain())
    }
}

#[cfg(test)]
mod server_url_tests {
    use super::*;

    /// Building against an explicit URL leaves the process environment
    /// alone: the URL reaches the pool as an argument (it used to be
    /// `set_var`'d, so the next client built anywhere in the process
    /// silently inherited it).
    #[tokio::test]
    async fn an_explicit_server_url_is_not_written_into_the_environment() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let url = "https://explicit.invalid:1";
        AppContext::with_config(Some(url.into()), Some(dir.path().to_str().unwrap().into()))
            .expect("context");
        assert_ne!(
            std::env::var("CHATTER_SERVER_URL").ok().as_deref(),
            Some(url)
        );
        assert_eq!(
            super::super::infrastructure::resolve_server_url(Some(url.into())),
            url
        );
    }
}
