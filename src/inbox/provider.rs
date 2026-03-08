//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};

use crate::events::EventBus;
use crate::mailbox::anon_dm::AnonDmPoller;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::mailbox::group::{GroupCoordinator, GroupFanout, GroupMailboxManager};
use crate::mailbox::timing::{TimingConfig, TimingObfuscation};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::{AnonymousMailboxClient, MailboxClient, MessageClient};
use crate::network::validation::MessageValidationPipeline;
use crate::services::message_gateway::MessageGateway;
use crate::services::pairwise::PairwiseService;
use crate::storage::{AttachmentRepository, ChannelRepository, Database, MessageRepository, SettingsRepository, UserRepository};

use super::config::InboxConfig;
use super::error::InboxError;
use super::events::InboxEvent;
use super::poller::InboxPoller;
use super::polling::{poll_dm_mailboxes, poll_group_mailboxes, process_anon_dm_results, process_device_message};
use super::state::{InboxState, InboxStats};
use super::streaming::spawn_user_events_stream;

/// Event channel capacity for inbox events.
/// Increased from 64 to 256 to handle message bursts without lagging.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Timeout for graceful shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of decoy mailboxes to maintain.
const MAX_DECOY_MAILBOXES: usize = 10;

/// Perform a dummy poll to a random decoy mailbox (NDSS '21 access pattern obfuscation).
///
/// The server will return "mailbox not found" but cannot distinguish
/// this from a legitimate poll to an empty mailbox.
async fn perform_dummy_poll(
    mailbox_client: &Arc<tokio::sync::Mutex<MailboxClient>>,
    decoys: &tokio::sync::RwLock<Vec<[u8; 32]>>,
) {
    // Ensure we have decoy addresses
    {
        let mut decoys_guard = decoys.write().await;
        while decoys_guard.len() < MAX_DECOY_MAILBOXES {
            let mut key = [0u8; 32];
            rand::fill(&mut key[..]);
            decoys_guard.push(key);
        }
    }

    // Select random decoy
    let decoy_key = {
        use rand::prelude::IndexedRandom;
        let decoys_guard = decoys.read().await;
        decoys_guard.choose(&mut rand::rng()).copied()
    };

    if let Some(key) = decoy_key {
        // Poll the decoy mailbox (will fail, but that's expected)
        let mut client = mailbox_client.lock().await;
        let _ = client.get_mailbox_challenge(key.to_vec()).await;
        tracing::trace!("Performed dummy poll to decoy mailbox");
    }
}

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
    poller_handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
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
    /// Deduplicator for DM messages.
    /// Prevents "old counter" errors when acknowledgment fails and server re-delivers.
    dm_deduplicator: Arc<crate::network::validation::MessageDeduplicator>,
    /// Attachment repository for creating records from incoming messages.
    attachment_repo: Arc<AttachmentRepository>,
    /// Timing obfuscation for privacy (NDSS '21).
    timing: TimingObfuscation,
    /// Probability of dummy polling (0.0 - 1.0).
    dummy_poll_probability: f64,
    /// Decoy mailbox addresses for dummy polling.
    decoys: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    /// Anonymous DM poller (optional).
    anon_dm_poller: Option<Arc<AnonDmPoller>>,
    /// Pairwise service for anon DM decryption (optional).
    pairwise_service: Option<Arc<PairwiseService>>,
    /// Anonymous mailbox client for anon DM polling (no JWT auth, challenge-response only).
    anon_mailbox_client: Option<Arc<tokio::sync::Mutex<AnonymousMailboxClient>>>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: InboxConfig,
        message_client: Arc<tokio::sync::Mutex<MessageClient>>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        pipeline: Arc<MessageValidationPipeline>,
        message_gateway: Arc<MessageGateway>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
        attachment_repo: Arc<AttachmentRepository>,
    ) -> Self {
        Self::with_event_bus(config, message_client, mailbox_client, pipeline, message_gateway, database, grpc_pool, attachment_repo, None)
    }

    /// Create a new device inbox provider with an event bus for domain events.
    #[allow(clippy::too_many_arguments)]
    pub fn with_event_bus(
        config: InboxConfig,
        message_client: Arc<tokio::sync::Mutex<MessageClient>>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        pipeline: Arc<MessageValidationPipeline>,
        message_gateway: Arc<MessageGateway>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
        attachment_repo: Arc<AttachmentRepository>,
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

        // Create timing obfuscation for privacy (NDSS '21)
        let timing = TimingObfuscation::new(TimingConfig::default());

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
            dm_deduplicator: Arc::new(crate::network::validation::MessageDeduplicator::with_default_capacity()),
            attachment_repo,
            timing,
            dummy_poll_probability: 0.2, // 20% dummy polls per NDSS '21
            decoys: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            anon_dm_poller: None,
            pairwise_service: None,
            anon_mailbox_client: None,
        }
    }

    /// Configure anonymous DM polling.
    ///
    /// Enables polling of anonymous DM mailboxes for stranger messaging.
    /// Requires both an `AnonDmPoller` for fetching messages and a
    /// `PairwiseService` for sealed sender decryption.
    pub fn with_anon_dm_polling(
        mut self,
        poller: Arc<AnonDmPoller>,
        pairwise_service: Arc<PairwiseService>,
    ) -> Self {
        // Create anonymous mailbox client (no JWT auth, challenge-response only)
        let anon_client = AnonymousMailboxClient::new(self.grpc_pool.channel());
        self.anon_dm_poller = Some(poller);
        self.pairwise_service = Some(pairwise_service);
        self.anon_mailbox_client = Some(Arc::new(tokio::sync::Mutex::new(anon_client)));
        self
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
        let dm_deduplicator = self.dm_deduplicator.clone();
        let attachment_repo = self.attachment_repo.clone();
        let timing = self.timing.clone();
        let dummy_poll_probability = self.dummy_poll_probability;
        let decoys = self.decoys.clone();
        let anon_dm_poller = self.anon_dm_poller.clone();
        let pairwise_service = self.pairwise_service.clone();
        let anon_mailbox_client = self.anon_mailbox_client.clone();

        tracing::info!("Spawning inbox provider task...");
        let handle = tokio::spawn(async move {
            tracing::info!("Inbox provider task started...");
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
                dm_deduplicator,
                attachment_repo,
                timing,
                dummy_poll_probability,
                decoys,
                anon_dm_poller,
                pairwise_service,
                anon_mailbox_client,
            )
            .await;
            tracing::info!("Inbox provider run_loop exited");
        });

        *self.poller_handle.write() = Some(handle);
        tracing::info!("Inbox provider started successfully");
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
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, handle).await {
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
        dm_deduplicator: Arc<crate::network::validation::MessageDeduplicator>,
        attachment_repo: Arc<AttachmentRepository>,
        timing: TimingObfuscation,
        dummy_poll_probability: f64,
        decoys: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
        anon_dm_poller: Option<Arc<AnonDmPoller>>,
        pairwise_service: Option<Arc<PairwiseService>>,
        anon_mailbox_client: Option<Arc<tokio::sync::Mutex<AnonymousMailboxClient>>>,
    ) {
        tracing::info!("Inbox provider run_loop started");

        // Create DM infrastructure
        let two_way = TwoWayAnonymity::new(database.clone());
        let conversation_manager = DmConversationManager::new(database.clone());

        // Create repositories
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
        let user_repo = Arc::new(UserRepository::new(database.clone()));

        // Create group mailbox infrastructure
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator =
            Arc::new(GroupCoordinator::new(database.clone(), group_mailbox_manager.clone()));
        let group_fanout = Arc::new(GroupFanout::new(group_mailbox_manager, mailbox_client.clone()));

        // Start user events stream for real-time friend request notifications
        tracing::info!("Checking for user ID to start user events stream...");
        match settings_repo.get::<String>(crate::storage::settings_keys::CURRENT_USER_ID) {
            Ok(Some(user_id)) => {
                tracing::info!(user_id = %user_id, "Starting user events stream");
                spawn_user_events_stream(
                    user_id,
                    grpc_pool.clone(),
                    event_tx.clone(),
                    shutdown_rx.clone(),
                )
                .await;
                tracing::info!("User events stream spawned");
            }
            Ok(None) => {
                tracing::warn!("No user ID found in settings, skipping user events stream");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to get user ID from settings");
            }
        }

        // Get domain event sender if event_bus is available
        let domain_tx = event_bus.as_ref().map(|bus| bus.domain_sender());

        // Helper to check if we should do a dummy poll
        let should_dummy_poll = || {
            use rand::Rng;
            rand::rng().random::<f64>() < dummy_poll_probability
        };

        tracing::info!(
            dummy_poll_probability = dummy_poll_probability,
            "Starting polling loop with timing obfuscation (NDSS '21)"
        );

        loop {
            // Apply timing delay before poll cycle (NDSS '21 timing obfuscation)
            timing.delay_before_poll().await;

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

            // Random delay between device and DM polling
            timing.delay_between_batch_items().await;

            // Maybe do a dummy poll (NDSS '21 access pattern obfuscation)
            if should_dummy_poll() {
                perform_dummy_poll(&mailbox_client, &decoys).await;
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
                &attachment_repo,
                &user_repo,
                domain_tx.as_ref(),
                &dm_deduplicator,
            )
            .await;

            // Random delay between DM and group polling
            timing.delay_between_batch_items().await;

            // Maybe do a dummy poll before group polling
            if should_dummy_poll() {
                perform_dummy_poll(&mailbox_client, &decoys).await;
            }

            // 3. Poll group mailboxes (ephemeral per-group mailboxes)
            poll_group_mailboxes(
                &group_fanout,
                &group_coordinator,
                &message_gateway,
                &event_tx,
                &channel_repo,
                &attachment_repo,
            )
            .await;

            // 4. Poll anonymous DM mailboxes (if configured)
            if let (Some(anon_poller), Some(pairwise), Some(anon_client)) = (&anon_dm_poller, &pairwise_service, &anon_mailbox_client) {
                // Random delay before anon DM polling
                timing.delay_between_batch_items().await;

                // Maybe do a dummy poll
                if should_dummy_poll() {
                    perform_dummy_poll(&mailbox_client, &decoys).await;
                }

                // Check how many mailboxes we have to poll
                let mailbox_count = anon_poller.get_active_mailbox_keys()
                    .map(|keys| keys.len())
                    .unwrap_or(0);

                tracing::info!(
                    mailbox_count = mailbox_count,
                    "[ANON_DM] Polling anonymous DM mailboxes"
                );

                // Poll anon DM mailboxes using anonymous client (no JWT, challenge-response only)
                let mut client = anon_client.lock().await;
                match anon_poller.poll_all(&mut client).await {
                    Ok(results) => {
                        if !results.is_empty() {
                            // Create mpsc channel for events (process_anon_dm_results expects mpsc)
                            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                            // Use deduplicator to prevent re-decryption when ack fails
                            let processed = process_anon_dm_results(results, pairwise, &tx, &dm_deduplicator).await;
                            drop(tx); // Close sender so receiver knows when done

                            // Forward events to broadcast channel
                            while let Some(event) = rx.recv().await {
                                let _ = event_tx.send(event);
                            }

                            // Acknowledge processed messages to prevent re-delivery
                            for (conversation_id, message_ids) in processed {
                                if !message_ids.is_empty() {
                                    if let Err(e) = anon_poller
                                        .acknowledge_messages(&conversation_id, message_ids, &mut client)
                                        .await
                                    {
                                        tracing::warn!(
                                            conversation_id = %conversation_id,
                                            error = %e,
                                            "Failed to acknowledge anonymous DM messages"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to poll anonymous DM mailboxes");
                    }
                }
            } else {
                // Log why anon DM polling is skipped (only once per session would be better, but for debugging)
                tracing::debug!(
                    has_poller = anon_dm_poller.is_some(),
                    has_pairwise = pairwise_service.is_some(),
                    has_anon_client = anon_mailbox_client.is_some(),
                    "[ANON_DM] Skipping anonymous DM polling (not configured)"
                );
            }
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
