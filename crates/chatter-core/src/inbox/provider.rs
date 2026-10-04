//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};

use crate::events::EventBus;
use crate::mailbox::anon_dm::AnonDmPoller;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::mailbox::group::{GroupCoordinator, GroupFanout, GroupMailboxManager};
use crate::mailbox::timing::{TimingConfig, TimingObfuscation};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::{AnonymousMailboxClient, MessageClient};
use crate::network::validation::MessageValidationPipeline;
use crate::services::gateway::MessageGateway;
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

/// Maximum number of decoy mailboxes to maintain.
const MAX_DECOY_MAILBOXES: usize = 10;

/// Perform a dummy poll to a random decoy mailbox (NDSS '21 access pattern obfuscation).
///
/// The server will return "mailbox not found" but cannot distinguish
/// this from a legitimate poll to an empty mailbox.
async fn perform_dummy_poll(
    mailbox_client: &Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
    decoys: &tokio::sync::RwLock<Vec<SigningKey>>,
) {
    // Ensure we have decoy signing keys. These must be real keypairs (not bare
    // random pubkeys) so a dummy poll can complete the SAME two-leg
    // challenge->sign->poll flow as a real poll. A bare pubkey could only do the
    // challenge leg, so decoys were trivially distinguishable from real polls by
    // request count — defeating the access-pattern obfuscation.
    {
        let mut decoys_guard = decoys.write().await;
        while decoys_guard.len() < MAX_DECOY_MAILBOXES {
            let mut seed = [0u8; 32];
            {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut seed[..]).expect("OS CSPRNG must not fail");}
            decoys_guard.push(SigningKey::from_bytes(&seed));
        }
    }

    // Select a random decoy keypair.
    let decoy = {
        use rand::prelude::IndexedRandom;
        let decoys_guard = decoys.read().await;
        decoys_guard.choose(&mut rand::rng()).cloned()
    };

    if let Some(key) = decoy {
        let pubkey = key.verifying_key().to_bytes().to_vec();
        let mut client = mailbox_client.lock().await;
        // Leg 1: challenge. Leg 2: signed poll — byte-for-byte the same request
        // shape as a real poll (server hides mailbox existence, so an unregistered
        // decoy is indistinguishable from an empty real mailbox).
        if let Ok(challenge_response) = client.get_mailbox_challenge(pubkey.clone()).await {
            let sig = key.sign(&challenge_response.challenge);
            let _ = client
                .poll_ephemeral_mailbox(
                    pubkey,
                    sig.to_bytes().to_vec(),
                    challenge_response.challenge.clone(),
                    None,
                    10,
                )
                .await;
        }
        tracing::trace!("Performed 2-leg dummy poll to decoy mailbox");
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
/// let config = InboxConfig {
///     poll_interval: Duration::from_secs(5),
///     ..InboxConfig::default()
/// };
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
    /// Handle to the spawned user-events stream, so it can be ABORTED on stop /
    /// transport switch. Without this the old stream survives on its pre-switch
    /// (direct) channel, keeping an IP-revealing socket open in shield mode.
    user_events_handle: Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
    message_gateway: Arc<MessageGateway>,
    /// Mailbox client for DM polling (anonymous - no auth headers).
    mailbox_client: Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
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
    decoys: Arc<tokio::sync::RwLock<Vec<SigningKey>>>,
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
        mailbox_client: Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
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
        mailbox_client: Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
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
            grpc_pool.clone(),
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
            user_events_handle: Arc::new(parking_lot::Mutex::new(None)),
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
        let anon_client = self.grpc_pool.anonymous_mailbox_client();
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
        // Reset the shutdown latch before subscribing. `watch` retains its last
        // value, so after a prior `stop()` (which sent `true`) a fresh subscriber
        // would read `true` and the new loop would exit on cycle 1 — exactly what
        // stops device-inbox polling from ever resuming after a transport switch
        // (shield toggle → stop() → refresh_clients() → start()). Sealed-sender
        // anon-DM requests land in the device queue, so a dead poll loop means the
        // recipient never sees them.
        let _ = self.shutdown_tx.send(false);
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
        let user_events_handle = self.user_events_handle.clone();

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
                user_events_handle,
            )
            .await;
            tracing::info!("Inbox provider run_loop exited");
        });

        *self.poller_handle.write() = Some(handle);
        tracing::info!("Inbox provider started successfully");
        Ok(())
    }

    /// Rebuild every transport-bound gRPC client from the pool so the next
    /// `start()` polls over the pool's CURRENT transport.
    ///
    /// The provider captures its mailbox/message clients at construction, and a
    /// tonic `Channel` is pinned to the transport (direct TCP vs. the Tor/Arti
    /// connector) it was built with — a direct channel can never route over
    /// onion. When anon mode toggles the pool's transport we must therefore
    /// swap the captured clients, not just restart the loop. Call this while the
    /// loop is STOPPED (between `stop()` and `start()`); the mutexes make it
    /// safe regardless, but rebuilding a live client mid-poll would be pointless.
    pub async fn refresh_clients(&self, pool: &GrpcClientPool) {
        *self.mailbox_client.lock().await = pool.anonymous_mailbox_client();
        if let Some(anon) = &self.anon_mailbox_client {
            *anon.lock().await = pool.anonymous_mailbox_client();
        }
        self.poller.refresh_message_client(pool).await;
        tracing::info!("inbox provider: rebuilt gRPC clients for new transport");
    }

    /// Stop the polling loop.
    ///
    /// Sends shutdown signal and waits for graceful termination.
    pub async fn stop(&self) -> Result<(), InboxError> {
        // Signal shutdown
        let _ = self.shutdown_tx.send(true);

        // Abort the user-events stream outright: it is a separate spawned task we
        // never join here, and relying on the shared watch races with start()'s
        // reset — an un-aborted old stream keeps its pre-switch (direct) channel
        // alive, leaking an IP-revealing socket in shield mode.
        if let Some(h) = self.user_events_handle.lock().take() {
            h.abort();
        }

        // Kill the run_loop. The old code awaited the handle with a timeout and,
        // ON TIMEOUT, DROPPED it — which DETACHES the tokio task, leaving it
        // running forever. Every transport toggle / reconnect that didn't drain
        // in time orphaned a poll loop: duplicate GetDeviceMessages spam (many
        // loops polling in parallel) AND a survivor still bound to the pre-switch
        // (direct) channel. Abort unconditionally so the task cannot detach; the
        // cooperative shutdown signal above lets an idle loop exit cleanly first,
        // and a mid-cycle abort is safe (unacked messages are re-delivered and
        // de-duplicated).
        let handle = self.poller_handle.write().take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await; // reap; JoinError(cancelled) is expected
        }
        *self.state.write() = InboxState::Stopped;
        Ok(())
    }

    /// Main polling loop.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop(
        poller: Arc<InboxPoller>,
        pipeline: Arc<MessageValidationPipeline>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
        message_gateway: Arc<MessageGateway>,
        mailbox_client: Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
        database: Arc<Database>,
        grpc_pool: Arc<GrpcClientPool>,
        mut shutdown_rx: watch::Receiver<bool>,
        event_bus: Option<EventBus>,
        dm_deduplicator: Arc<crate::network::validation::MessageDeduplicator>,
        attachment_repo: Arc<AttachmentRepository>,
        timing: TimingObfuscation,
        dummy_poll_probability: f64,
        decoys: Arc<tokio::sync::RwLock<Vec<SigningKey>>>,
        anon_dm_poller: Option<Arc<AnonDmPoller>>,
        pairwise_service: Option<Arc<PairwiseService>>,
        anon_mailbox_client: Option<Arc<tokio::sync::Mutex<AnonymousMailboxClient>>>,
        user_events_handle: Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
        let ephemeral_repo = Arc::new(
            crate::storage::repositories::ephemeral_repo::EphemeralRepository::new(database.clone()),
        );

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
                let ue_handle = spawn_user_events_stream(
                    user_id,
                    grpc_pool.clone(),
                    event_tx.clone(),
                    user_repo.clone(),
                    event_bus.as_ref().map(|bus| bus.domain_sender()),
                    shutdown_rx.clone(),
                )
                .await;
                // Track the handle and abort any prior-generation stream so its
                // pre-switch (direct) channel is dropped immediately.
                if let Some(old) = user_events_handle.lock().replace(ue_handle) {
                    old.abort();
                }
                tracing::info!("User events stream spawned");
            }
            Ok(None) => {
                tracing::warn!("No user ID found in settings, skipping user events stream");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to get user ID from settings");
            }
        }

        // Resolve current user ID for self-message filtering in anon DM polling.
        let current_user_id: Option<crate::domain::UserId> = settings_repo
            .get::<String>(crate::storage::settings_keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());

        // DMs are channel-free (Signal-style): there is no server DM channel
        // to open a StreamMessages listener on. Incoming DMs arrive as
        // per-device sealed-sender envelopes through the device-inbox poll
        // (see services/gateway/mod.rs store_data_message), so no per-channel
        // stream spawn is needed here.

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

        // One retain budget for the whole polling loop. Retaining is otherwise
        // unbounded: an envelope that keeps failing transiently is re-fetched
        // and re-decrypted every cycle for as long as the client runs, and
        // `classify_crypto` defaults to transient, so that is the common case
        // rather than the rare one. Shared across the loops because each of
        // them keys on the same server-side message id.
        let retry_budget = crate::inbox::ack_policy::RetryBudget::new();

        let mut poll_cycle: u64 = 0;
        loop {
            poll_cycle += 1;

            // Apply timing delay before poll cycle (NDSS '21 timing obfuscation)
            timing.delay_before_poll().await;

            // 1. Poll device inbox for regular messages
            tracing::info!(cycle = poll_cycle, "Polling device inbox...");
            match poller.run_once().await {
                Ok(Some(messages)) => {
                    let count = messages.len();
                    if count > 0 {
                        tracing::info!(cycle = poll_cycle, count, "Device inbox: received messages");
                    } else {
                        tracing::info!(cycle = poll_cycle, "Device inbox: empty");
                    }
                    let mut handled: Vec<String> = Vec::with_capacity(count);
                    // The pagination cursor moves with these decisions, not
                    // ahead of them: `after_id` is exclusive, so leaving a
                    // retained id below it means the server keeps the envelope
                    // (we never acked) and we never ask for it again (the
                    // cursor passed it). That made `Retain` a no-op.
                    let batch_max = messages.iter().map(|m| m.queue_id).max().unwrap_or(0);
                    let mut lowest_retained: Option<i64> = None;
                    for msg in messages {
                        let guid = msg.message_guid.clone();
                        let queue_id = msg.queue_id;
                        // Acking DELETES it server-side, so the guid only goes
                        // into `handled` when `process_device_message` says it
                        // is safe to destroy — either it was processed, or it
                        // failed permanently AND a marker is already durably
                        // written. This used to push unconditionally.
                        let decision = process_device_message(
                            &msg,
                            &pipeline,
                            &event_tx,
                            &stats,
                            &message_gateway,
                            &message_repo,
                            &channel_repo,
                            &ephemeral_repo,
                            &group_coordinator,
                            domain_tx.as_ref(),
                            &retry_budget,
                        )
                        .await;
                        if decision.should_ack() {
                            handled.push(guid);
                        } else {
                            lowest_retained = Some(
                                lowest_retained.map_or(queue_id, |lo: i64| lo.min(queue_id)),
                            );
                            tracing::info!(
                                cycle = poll_cycle,
                                message_guid = %guid,
                                queue_id,
                                "Device inbox: retaining envelope for redelivery (transient \
                                 failure, or its failure marker could not be written)"
                            );
                        }
                    }

                    if count > 0 {
                        poller.advance_after_id(batch_max, lowest_retained);
                    }

                    // Drain the server queue. `AcknowledgeMessages` is documented in
                    // the proto as "Acknowledge receipt of messages (deletes from
                    // queue)", and nothing was calling it for the device inbox — only
                    // the group/DM/anon mailbox paths, which use a different client.
                    // So the queue only ever grew, and since `after_id` starts at 0
                    // every launch, each start re-fetched and re-decrypted the whole
                    // history: 100 messages a page with `has_more=true`.
                    //
                    // Acked whatever the outcome, not only on success. An undecryptable
                    // message cannot be recovered by handing it to us again — the
                    // ciphertext is dead. Keeping it queued instead means replaying it
                    // at every start and reporting it again each time, which is
                    // precisely the storm this produced: 201 inbound reports and 12
                    // outbound in four minutes between two idle devices.
                    //
                    // Note what that costs: the message itself is lost. The
                    // `DecryptionErrorMessage` we send back only heals the *session*
                    // — the sender archives it (`services::gateway::decryption`,
                    // `archive_session_for_resend`) so the next message uses a fresh
                    // one. Nothing re-sends the message that failed.
                    //
                    // So the loss is made VISIBLE instead: the failure branch in
                    // `polling::device` writes a `SystemMessage::DeliveryIssue` entry
                    // into that peer's conversation (Signal-Android's "Delivery
                    // issue" update), keyed to the envelope timestamp so a replay
                    // does not stack duplicates. The user sees where the gap is and
                    // that the fix is to ask the peer to send it again.
                    if !handled.is_empty()
                        && let Err(error) = poller.acknowledge(handled).await
                    {
                        // Not fatal. The cursor has advanced past these (they
                        // were decided ackable, so nothing is retaining them),
                        // which means the next poll skips them; they come back
                        // on the next start, when the cursor begins at 0 again,
                        // and processing is idempotent. Loud because a
                        // persistent failure here is invisible except as a
                        // queue that never shrinks.
                        tracing::warn!(
                            cycle = poll_cycle,
                            %error,
                            "Device inbox: acknowledge failed; these stay queued on \
                             the server and are reprocessed at the next start"
                        );
                    }
                }
                Ok(None) => {
                    tracing::info!(cycle = poll_cycle, "Device inbox: shutdown requested");
                    break;
                }
                Err(e) => {
                    tracing::warn!(cycle = poll_cycle, error = %e, "Device inbox: poll failed");
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
                &retry_budget,
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
                &message_repo,
                domain_tx.as_ref(),
                &retry_budget,
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

                tracing::trace!(
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
                            // Blocking check closure — uses the DB directly to
                            // avoid threading AnonDmService through the polling layer.
                            let anon_repo = crate::storage::repositories::AnonDmRepository::new(database.clone());
                            let is_blocked = |sender: &crate::domain::UserId, conv: &crate::domain::anon_dm::AnonDmId| -> bool {
                                if anon_repo.is_user_blocked(sender).unwrap_or(false) {
                                    return true;
                                }
                                anon_repo.get_conversation(conv)
                                    .ok()
                                    .flatten()
                                    .map(|dm| dm.status == crate::domain::anon_dm::AnonDmStatus::Blocked)
                                    .unwrap_or(false)
                            };
                            // Use deduplicator to prevent re-decryption when ack fails
                            let processed = process_anon_dm_results(
                                results,
                                pairwise,
                                &tx,
                                &dm_deduplicator,
                                current_user_id.as_ref(),
                                &is_blocked,
                                &message_repo,
                                domain_tx.as_ref(),
                            )
                            .await;
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

            // Base poll interval between full cycles (5s max — the config's
            // active/idle interval). Jittered DOWNWARD only (clamp to base) so the
            // gap never exceeds 5s while still adding timing noise. The per-batch
            // delays above are only sub-second access-pattern noise; this is what
            // actually paces the loop. Honour shutdown during the wait.
            let base = poller.current_interval();
            let wait = timing.jitter_interval(base).min(base);
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::info!(cycle = poll_cycle, "Inbox run_loop: shutdown during inter-poll wait");
                        break;
                    }
                }
                _ = tokio::time::sleep(wait) => {}
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
