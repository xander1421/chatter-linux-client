//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};

use crate::events::EventBus;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::mailbox::group::{GroupCoordinator, GroupFanout, GroupMailboxManager};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::{MailboxClient, MessageClient};
use crate::network::validation::MessageValidationPipeline;
use crate::services::message_gateway::MessageGateway;
use crate::storage::{ChannelRepository, Database, MessageRepository, SettingsRepository};

use super::config::InboxConfig;
use super::error::InboxError;
use super::events::InboxEvent;
use super::poller::InboxPoller;
use super::polling::{poll_dm_mailboxes, poll_group_mailboxes, process_device_message};
use super::state::{InboxState, InboxStats};
use super::streaming::spawn_user_events_stream;

/// Event channel capacity for inbox events.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Timeout for graceful shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Main coordinator for device inbox operations.
///
/// Orchestrates the polling loop, message validation, and event broadcasting.
/// Messages flow through:
/// 1. `InboxPoller` fetches from server
/// 2. `MessageValidationPipeline` validates and orders
/// 3. Events emitted for handlers to process
///
/// Also handles DM mailbox polling for two-way anonymous messaging.
///
/// # Example
///
/// ```ignore
/// let config = InboxConfig::builder()
///     .poll_interval(Duration::from_secs(5))
///     .build();
///
/// let provider = DeviceInboxProvider::new(
///     config,
///     message_client,
///     mailbox_client,
///     pipeline,
///     message_gateway,
///     database,
/// );
///
/// // Subscribe to events before starting
/// let mut rx = provider.subscribe();
/// provider.start().await?;
///
/// // Handle events
/// while let Ok(event) = rx.recv().await {
///     match event {
///         InboxEvent::MessageReceived { message_id, .. } => {
///             // Process message
///         }
///         InboxEvent::DmReceived { peer_id, text, .. } => {
///             // Handle DM message
///         }
///         _ => {}
///     }
/// }
/// ```
pub struct DeviceInboxProvider {
    config: InboxConfig,
    poller: Arc<InboxPoller>,
    pipeline: Arc<MessageValidationPipeline>,
    event_tx: broadcast::Sender<InboxEvent>,
    shutdown_tx: watch::Sender<bool>,
    poller_handle: RwLock<Option<std::thread::JoinHandle<()>>>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
    message_gateway: Arc<MessageGateway>,
    /// Mailbox client for DM polling.
    mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
    /// Database for DM conversation state.
    database: Arc<Database>,
    /// gRPC client pool for friend request polling.
    grpc_pool: Arc<GrpcClientPool>,
    /// Event bus for domain events.
    event_bus: Option<EventBus>,
}

impl DeviceInboxProvider {
    /// Create a new device inbox provider.
    ///
    /// # Arguments
    ///
    /// * `config` - Polling and processing configuration
    /// * `message_client` - Client for message RPCs
    /// * `mailbox_client` - Client for DM mailbox polling
    /// * `pipeline` - Message validation pipeline
    /// * `message_gateway` - Gateway for decrypting and routing messages
    /// * `database` - Database for DM conversation state
    /// * `grpc_pool` - gRPC client pool for friend request polling
    /// * `event_bus` - Optional event bus for domain events
    pub fn new(
        config: InboxConfig,
        message_client: Arc<tokio::sync::Mutex<MessageClient>>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        pipeline: Arc<MessageValidationPipeline>,
        message_gateway: Arc<MessageGateway>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
    ) -> Self {
        Self::with_event_bus(config, message_client, mailbox_client, pipeline, message_gateway, database, grpc_pool, None)
    }

    /// Create a new device inbox provider with an event bus for domain events.
    pub fn with_event_bus(
        config: InboxConfig,
        message_client: Arc<tokio::sync::Mutex<MessageClient>>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        pipeline: Arc<MessageValidationPipeline>,
        message_gateway: Arc<MessageGateway>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
        event_bus: Option<EventBus>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let state = Arc::new(RwLock::new(InboxState::Idle));

        let poller = Arc::new(InboxPoller::new(
            config.clone(),
            message_client,
            shutdown_rx,
            event_tx.clone(),
            stats.clone(),
            state.clone(),
        ));

        Self {
            config,
            poller,
            pipeline,
            event_tx,
            shutdown_tx,
            poller_handle: RwLock::new(None),
            stats,
            state,
            message_gateway,
            mailbox_client,
            database,
            grpc_pool,
            event_bus,
        }
    }

    /// Start the polling loop.
    ///
    /// Spawns a background thread that continuously polls for messages.
    /// Does nothing if already running.
    pub async fn start(&self) -> Result<(), InboxError> {
        {
            let handle = self.poller_handle.read();
            if handle.is_some() {
                return Err(InboxError::AlreadyRunning);
            }
        }

        let poller = self.poller.clone();
        let pipeline = self.pipeline.clone();
        let event_tx = self.event_tx.clone();
        let stats = self.stats.clone();
        let message_gateway = self.message_gateway.clone();
        let mailbox_client = self.mailbox_client.clone();
        let database = self.database.clone();
        let grpc_pool = self.grpc_pool.clone();
        let shutdown_rx = self.shutdown_tx.subscribe();
        let event_bus = self.event_bus.clone();

        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime");

            rt.block_on(async move {
                Self::run_loop(
                    poller,
                    pipeline,
                    event_tx,
                    stats,
                    message_gateway,
                    mailbox_client,
                    database,
                    grpc_pool,
                    shutdown_rx,
                    event_bus,
                )
                .await;
            });
        });

        *self.poller_handle.write() = Some(handle);
        Ok(())
    }

    /// Stop the polling loop.
    ///
    /// Sends shutdown signal and waits for graceful termination.
    pub async fn stop(&self) -> Result<(), InboxError> {
        // Signal shutdown
        let _ = self.shutdown_tx.send(true);

        // Wait for handle with timeout
        let handle = self.poller_handle.write().take();
        if let Some(handle) = handle {
            let join_handle = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            });

            match tokio::time::timeout(SHUTDOWN_TIMEOUT, join_handle).await {
                Ok(_) => {
                    *self.state.write() = InboxState::Stopped;
                    Ok(())
                }
                Err(_) => Err(InboxError::ShutdownTimeout),
            }
        } else {
            Ok(())
        }
    }

    /// Main polling loop.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop(
        poller: Arc<InboxPoller>,
        pipeline: Arc<MessageValidationPipeline>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
        message_gateway: Arc<MessageGateway>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
        shutdown_rx: watch::Receiver<bool>,
        event_bus: Option<EventBus>,
    ) {
        // Create DM infrastructure
        let two_way = TwoWayAnonymity::new(database.clone());
        let conversation_manager = DmConversationManager::new(database.clone());

        // Create repositories
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));

        // Create group mailbox infrastructure
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator =
            Arc::new(GroupCoordinator::new(database.clone(), group_mailbox_manager.clone()));
        let group_fanout = Arc::new(GroupFanout::new(group_mailbox_manager, mailbox_client.clone()));

        // Start user events stream for real-time friend request notifications
        if let Ok(Some(user_id)) = settings_repo.get::<String>(crate::storage::settings_keys::CURRENT_USER_ID) {
            spawn_user_events_stream(
                user_id,
                grpc_pool.clone(),
                event_tx.clone(),
                shutdown_rx.clone(),
            )
            .await;
            tracing::debug!("User events stream started");
        } else {
            tracing::warn!("No user ID found, skipping user events stream");
        }

        // Get domain event sender if event_bus is available
        let domain_tx = event_bus.as_ref().map(|bus| bus.domain_sender());

        loop {
            // 1. Poll device inbox for regular messages
            match poller.run_once().await {
                Ok(Some(messages)) => {
                    for msg in messages {
                        process_device_message(
                            &msg,
                            &pipeline,
                            &event_tx,
                            &stats,
                            &message_gateway,
                            &message_repo,
                            &channel_repo,
                            &group_coordinator,
                            domain_tx.as_ref(),
                        )
                        .await;
                    }
                }
                Ok(None) => {
                    // Shutdown requested
                    break;
                }
                Err(_) => {
                    // Error already logged/emitted by poller
                }
            }

            // 2. Poll DM mailboxes
            poll_dm_mailboxes(
                &two_way,
                &conversation_manager,
                &mailbox_client,
                &message_gateway,
                &event_tx,
                &settings_repo,
                &message_repo,
                &channel_repo,
                domain_tx.as_ref(),
            )
            .await;

            // 3. Poll group mailboxes (ephemeral per-group mailboxes)
            poll_group_mailboxes(
                &group_fanout,
                &group_coordinator,
                &message_gateway,
                &event_tx,
                &settings_repo,
                &message_repo,
                &channel_repo,
            )
            .await;
        }
    }

    /// Subscribe to inbox events.
    ///
    /// Returns a receiver for all inbox events. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<InboxEvent> {
        self.event_tx.subscribe()
    }

    /// Get current inbox statistics.
    pub fn stats(&self) -> InboxStats {
        *self.stats.read()
    }

    /// Get current inbox state.
    pub fn state(&self) -> InboxState {
        *self.state.read()
    }

    /// Check if the provider is running.
    pub fn is_running(&self) -> bool {
        self.poller_handle.read().is_some()
    }

    /// Get a reference to the configuration.
    pub fn config(&self) -> &InboxConfig {
        &self.config
    }
}
