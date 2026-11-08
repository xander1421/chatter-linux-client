//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use futures::{stream, StreamExt};
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
use crate::storage::{
    AttachmentRepository, ChannelRepository, Database, MessageRepository, SettingsRepository,
    UserRepository,
};

use super::config::InboxConfig;
use super::error::InboxError;
use super::events::InboxEvent;
use super::poller::InboxPoller;
use super::polling::{
    poll_dm_mailboxes, poll_group_mailboxes, process_anon_dm_results, process_device_message,
};
use super::state::{InboxState, InboxStats};
use super::streaming::spawn_user_events_stream;

/// Event channel capacity for inbox events.
/// Increased from 64 to 256 to handle message bursts without lagging.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Coalesced local hints affect pacing only on direct transport. Every cycle
/// still polls and validates through the normal privacy-preserving pipeline.
struct PollHints {
    wake: tokio::sync::Notify,
    anon_wake: std::sync::atomic::AtomicBool,
    foreground_wake: tokio::sync::Notify,
    visible: std::sync::atomic::AtomicBool,
    leases: parking_lot::Mutex<Vec<(u64, std::time::Instant)>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for PollHints {
    fn default() -> Self {
        Self {
            wake: tokio::sync::Notify::new(),
            anon_wake: std::sync::atomic::AtomicBool::new(false),
            foreground_wake: tokio::sync::Notify::new(),
            visible: std::sync::atomic::AtomicBool::new(true),
            leases: parking_lot::Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

pub(crate) struct ResponsivePollGuard {
    hints: Arc<PollHints>,
    id: u64,
}

impl Drop for ResponsivePollGuard {
    fn drop(&mut self) {
        self.hints.leases.lock().retain(|(id, _)| *id != self.id);
    }
}

impl PollHints {
    fn set_visible(&self, visible: bool) {
        let was_visible = self
            .visible
            .swap(visible, std::sync::atomic::Ordering::AcqRel);
        if visible && !was_visible {
            self.anon_wake
                .store(true, std::sync::atomic::Ordering::Release);
            self.foreground_wake.notify_one();
        }
    }

    fn cycle_interval(&self, foreground: std::time::Duration) -> std::time::Duration {
        if self.visible.load(std::sync::atomic::Ordering::Acquire) {
            foreground
        } else if crate::network::connection::push_is_connected() {
            // Signal-mobile model: backgrounded with a live ntfy push SSE, a real
            // wake arrives in ~250ms via provider.wake(), so delivery is
            // push-driven. This timer is only a rare safety net for a silently
            // dropped wake — 30 minutes, so the radio sleeps instead of polling.
            std::time::Duration::from_secs(1800)
        } else {
            // Push SSE is down (or never connected): it is no longer waking us,
            // so fall back to the tight 30s poll until the stream reconnects.
            std::time::Duration::from_secs(30)
        }
    }

    fn begin(self: &Arc<Self>) -> ResponsivePollGuard {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.leases.lock().push((
            id,
            std::time::Instant::now() + std::time::Duration::from_secs(65),
        ));
        self.wake.notify_one();
        ResponsivePollGuard {
            hints: self.clone(),
            id,
        }
    }

    fn interval(&self, base: std::time::Duration, direct: bool) -> std::time::Duration {
        let now = std::time::Instant::now();
        let mut leases = self.leases.lock();
        leases.retain(|(_, deadline)| *deadline > now);
        if direct && !leases.is_empty() {
            base.min(std::time::Duration::from_millis(250))
        } else {
            base
        }
    }

    /// Return false on shutdown. Restore the original deadline if transport
    /// changes to Shield, including after a wake shortened the sleep.
    async fn wait(
        &self,
        base: std::time::Duration,
        shutdown: &mut watch::Receiver<bool>,
        direct: impl Fn() -> bool,
    ) -> bool {
        let original = tokio::time::Instant::now() + base;
        let mut target = tokio::time::Instant::now() + self.interval(base, direct());
        let mut foreground_requested = false;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return false; }
                }
                _ = self.wake.notified(), if direct() => {
                    // Burst hints never postpone or repeatedly shorten an
                    // already scheduled responsive cycle.
                    target = target.min(tokio::time::Instant::now() + std::time::Duration::from_millis(250));
                }
                _ = self.foreground_wake.notified() => {
                    foreground_requested = true;
                    target = tokio::time::Instant::now();
                }
                _ = tokio::time::sleep_until(target) => {
                    if !direct() && !foreground_requested && tokio::time::Instant::now() < original {
                        target = original;
                    } else {
                        return true;
                    }
                }
            }
        }
    }
}

/// Space empty secret-mailbox fetches while direct-transport wake subscriptions
/// can request an immediate cycle. The bounded fallback repairs lost SSE wakes.
struct AnonPollBackoff {
    empty_polls: u32,
    next_due: tokio::time::Instant,
}

impl AnonPollBackoff {
    fn new() -> Self {
        Self {
            empty_polls: 0,
            next_due: tokio::time::Instant::now(),
        }
    }

    fn interval(empty_polls: u32) -> std::time::Duration {
        std::time::Duration::from_secs(5u64.saturating_mul(1u64 << empty_polls.min(5)))
            .min(std::time::Duration::from_secs(120))
    }

    fn reset(&mut self) {
        self.empty_polls = 0;
        self.next_due = tokio::time::Instant::now();
    }

    fn finished_empty(&mut self) {
        self.next_due = tokio::time::Instant::now() + Self::interval(self.empty_polls);
        self.empty_polls = self.empty_polls.saturating_add(1);
    }
}

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
            {
                use rand::TryRngCore;
                rand::rngs::OsRng
                    .try_fill_bytes(&mut seed[..])
                    .expect("OS CSPRNG must not fail");
            }
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
    poll_hints: Arc<PollHints>,
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
    content_handoff: Option<Arc<crate::inbox::content_handoff::ContentHandoff>>,
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
        Self::with_event_bus(
            config,
            message_client,
            mailbox_client,
            pipeline,
            message_gateway,
            database,
            grpc_pool,
            attachment_repo,
            None,
        )
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
            poll_hints: Arc::new(PollHints::default()),
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
            dm_deduplicator: Arc::new(
                crate::network::validation::MessageDeduplicator::with_default_capacity(),
            ),
            attachment_repo,
            timing,
            dummy_poll_probability: 0.2, // 20% dummy polls per NDSS '21
            decoys: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            anon_dm_poller: None,
            pairwise_service: None,
            content_handoff: None,
        }
    }

    /// Configure anonymous DM polling.
    ///
    /// Enables polling of anonymous DM mailboxes for stranger messaging.
    /// Requires both an `AnonDmPoller` for fetching messages and a
    /// `PairwiseService` for sealed sender decryption. Anonymous DMs are
    /// secret chats: each cycle polls them over the pool's secret (Tor)
    /// channel, and skips them when there is no Tor route.
    pub fn with_anon_dm_polling(
        mut self,
        poller: Arc<AnonDmPoller>,
        pairwise_service: Arc<PairwiseService>,
    ) -> Self {
        self.anon_dm_poller = Some(poller);
        self.pairwise_service = Some(pairwise_service);
        self
    }

    pub(crate) fn with_content_handoff(
        mut self,
        dm: Arc<crate::services::anon_dm::AnonDmService>,
        group: Arc<crate::services::anon_group::AnonGroupService>,
        events: EventBus,
        database: Arc<Database>,
    ) -> Self {
        self.content_handoff = Some(Arc::new(crate::inbox::content_handoff::ContentHandoff {
            dm,
            group,
            events,
            database,
        }));
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

        // This provider survives logout. Pagination must not carry the old
        // device's high-water mark into the new device's independent queue.
        let settings = SettingsRepository::new(self.database.clone());
        let user_id = settings
            .get::<String>(crate::storage::settings_keys::CURRENT_USER_ID)
            .map_err(|e| InboxError::AuthFailed(e.to_string()))?
            .ok_or_else(|| InboxError::AuthFailed("No current user id".into()))?;
        let device_id = settings
            .get::<u32>(crate::storage::settings_keys::CURRENT_DEVICE_ID)
            .map_err(|e| InboxError::AuthFailed(e.to_string()))?
            .ok_or_else(|| InboxError::AuthFailed("No current device id".into()))?;
        self.poller
            .bind_device(user_id, device_id, self.grpc_pool.auth().generation());

        let poll_hints = self.poll_hints.clone();
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
        let content_handoff = self.content_handoff.clone();
        let user_events_handle = self.user_events_handle.clone();

        tracing::info!("Spawning inbox provider task...");
        let handle = tokio::spawn(async move {
            tracing::info!("Inbox provider task started...");
            Self::run_loop(
                poll_hints,
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
                content_handoff,
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
        poll_hints: Arc<PollHints>,
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
        content_handoff: Option<Arc<crate::inbox::content_handoff::ContentHandoff>>,
        user_events_handle: Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    ) {
        tracing::info!("Inbox provider run_loop started");

        // Create DM infrastructure
        let two_way = Arc::new(TwoWayAnonymity::new(database.clone()));
        let conversation_manager = Arc::new(DmConversationManager::new(database.clone()));

        // Create repositories
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        if let Some(handoff) = &content_handoff {
            handoff.replay(&message_repo).await;
        }
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
        let user_repo = Arc::new(UserRepository::new(database.clone()));
        let ephemeral_repo = Arc::new(
            crate::storage::repositories::ephemeral_repo::EphemeralRepository::new(
                database.clone(),
            ),
        );

        // Create group mailbox infrastructure
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator = Arc::new(GroupCoordinator::new(group_mailbox_manager.clone()));
        let group_fanout = Arc::new(GroupFanout::new(
            group_mailbox_manager,
            mailbox_client.clone(),
            grpc_pool.clone(),
        ));

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
                    settings_repo.clone(),
                    event_bus.as_ref().map(|bus| bus.domain_sender()),
                    shutdown_rx.clone(),
                    crate::network::connection::subscribe_app_visible(),
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
        let retry_budget = Arc::new(crate::inbox::ack_policy::RetryBudget::new());

        let mut anon_polls = tokio::task::JoinSet::new();
        let mut anon_backoff = AnonPollBackoff::new();
        let mut group_polls = tokio::task::JoinSet::new();
        let mut dm_polls = tokio::task::JoinSet::new();
        let mut poll_cycle: u64 = 0;
        let mut pending_device_acks: Vec<String> = Vec::new();
        let mut catch_up_pages = 0u8;
        loop {
            poll_cycle += 1;
            let cycle_started = tokio::time::Instant::now();

            // Apply timing delay before poll cycle (NDSS '21 timing obfuscation)
            timing.delay_before_poll().await;

            // 1. Poll device inbox for regular messages
            tracing::info!(cycle = poll_cycle, "Polling device inbox...");
            if !pending_device_acks.is_empty() {
                if let Err(error) = poller.acknowledge(pending_device_acks.clone()).await {
                    tracing::warn!(cycle = poll_cycle, %error, pending = pending_device_acks.len(),
                        "Device inbox: pending acknowledgements still await retry");
                } else {
                    pending_device_acks.clear();
                }
            }
            // Stop paging while a failed ACK batch is outstanding. This bounds
            // the retry journal and avoids re-decrypting already handled mail.
            let poll_result = if pending_device_acks.is_empty() {
                poller.run_once().await
            } else {
                Ok(Some((Vec::new(), false)))
            };
            match poll_result {
                Ok(Some((messages, has_more))) => {
                    let count = messages.len();
                    if count > 0 {
                        tracing::info!(
                            cycle = poll_cycle,
                            count,
                            "Device inbox: received messages"
                        );
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
                        if !retry_budget.should_attempt(&guid, crate::inbox::ack_policy::now_ns()) {
                            lowest_retained = Some(lowest_retained.map_or(queue_id, |lo: i64| lo.min(queue_id)));
                            continue;
                        }
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
                            &crate::storage::repositories::AnonGroupRepository::new(
                                database.clone(),
                            ),
                            &ephemeral_repo,
                            &group_coordinator,
                            domain_tx.as_ref(),
                            &retry_budget,
                        )
                        .await;
                        if decision.should_ack() {
                            retry_budget.forget(&guid);
                            handled.push(guid);
                        } else {
                            lowest_retained =
                                Some(lowest_retained.map_or(queue_id, |lo: i64| lo.min(queue_id)));
                            tracing::info!(
                                cycle = poll_cycle,
                                message_guid = %guid,
                                queue_id,
                                "Device inbox: retaining envelope for redelivery (transient \
                                 failure, or its failure marker could not be written)"
                            );
                        }
                    }

                    if !handled.is_empty() {
                        retry_budget.retry_after_progress();
                    }
                    // Even an empty terminal page closes the lookahead traversal
                    // and reoffers retained mail. ACK failures still block fetching.
                    poller.advance_after_id(batch_max, lowest_retained, has_more);
                    catch_up_pages = if has_more && catch_up_pages < 20
                    {
                        catch_up_pages + 1
                    } else {
                        0
                    };

                    // Drain the server queue. `AcknowledgeMessages` is documented in
                    // the proto as "Acknowledge receipt of messages (deletes from
                    // queue)", and nothing was calling it for the device inbox — only
                    // the group/DM/anon mailbox paths, which use a different client.
                    // So the queue only ever grew, and since `after_id` starts at 0
                    // every launch, each start re-fetched and re-decrypted the whole
                    // history: 100 messages a page with `has_more=true`.
                    //
                    // An undecryptable message cannot be recovered by handing it to us again — the
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
                        && let Err(error) = poller.acknowledge(handled.clone()).await
                    {
                        // Keep the GUIDs and retry before another fetch. The
                        // cursor can stay advanced because no envelope is
                        // discarded from this retry journal on RPC failure.
                        tracing::warn!(
                            cycle = poll_cycle,
                            %error,
                        "Device inbox: acknowledge failed; retrying this batch next cycle"
                        );
                        pending_device_acks = handled;
                        catch_up_pages = 0;
                    }
                }
                Ok(None) => {
                    tracing::info!(cycle = poll_cycle, "Device inbox: shutdown requested");
                    break;
                }
                Err(e) => {
                    tracing::warn!(cycle = poll_cycle, error = %e, "Device inbox: poll failed");
                    catch_up_pages = 0;
                }
            }

            // Mailbox RPCs can stall indefinitely after a background network
            // change. Bound the auxiliary portion so the authenticated device
            // inbox retains its fallback cadence even if ntfy never wakes us.
            // Shield permits a cold onion connect; its privacy transport has
            // its own longer budget and does not use direct push wakes.
            let auxiliary_budget = if grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Direct
            {
                std::time::Duration::from_secs(5)
            } else {
                std::time::Duration::from_secs(95)
            };
            let auxiliary = async {
                // Random delay between device and DM polling
                timing.delay_between_batch_items().await;

                // Maybe do a dummy poll (NDSS '21 access pattern obfuscation)
                if should_dummy_poll() {
                    let dummy_client = Arc::new(tokio::sync::Mutex::new(
                        grpc_pool.anonymous_mailbox_client(),
                    ));
                    perform_dummy_poll(&dummy_client, &decoys).await;
                }
            };
            let resumed = tokio::select! {
                result = tokio::time::timeout(auxiliary_budget, auxiliary) => {
                    if result.is_err() {
                        tracing::warn!(cycle = poll_cycle, "Inbox auxiliary polling exceeded cycle budget");
                    }
                    false
                }
                _ = poll_hints.foreground_wake.notified() => true,
            };

            // Preserve fetched DM results across device-cycle deadlines and wakes.
            while let Some(result) = dm_polls.try_join_next() {
                if let Err(error) = result {
                    tracing::warn!(%error, "DM mailbox polling task failed");
                }
            }
            if dm_polls.is_empty() {
                let two_way = two_way.clone();
                let conversation_manager = conversation_manager.clone();
                let mailbox_client = mailbox_client.clone();
                let message_gateway = message_gateway.clone();
                let event_tx = event_tx.clone();
                let settings_repo = settings_repo.clone();
                let message_repo = message_repo.clone();
                let attachment_repo = attachment_repo.clone();
                let user_repo = user_repo.clone();
                let domain_tx = domain_tx.clone();
                let dm_deduplicator = dm_deduplicator.clone();
                let retry_budget = retry_budget.clone();
                dm_polls.spawn(async move {
                    poll_dm_mailboxes(
                        &two_way,
                        &conversation_manager,
                        &mailbox_client,
                        &message_gateway,
                        &event_tx,
                        &settings_repo,
                        &message_repo,
                        &attachment_repo,
                        &user_repo,
                        domain_tx.as_ref(),
                        &dm_deduplicator,
                        &retry_budget,
                        auxiliary_budget,
                    )
                    .await;
                });
            }

            // Group mailboxes can take longer than the auxiliary budget over
            // Tor. Keep one poll in flight across device-inbox cycles; a cycle
            // timeout or foreground wake must not discard fetched messages.
            while let Some(result) = group_polls.try_join_next() {
                if let Err(error) = result {
                    tracing::warn!(%error, "Group mailbox polling task failed");
                }
            }
            if group_polls.is_empty() {
                let group_fanout = group_fanout.clone();
                let group_coordinator = group_coordinator.clone();
                let message_gateway = message_gateway.clone();
                let event_tx = event_tx.clone();
                let channel_repo = channel_repo.clone();
                let attachment_repo = attachment_repo.clone();
                let message_repo = message_repo.clone();
                let domain_tx = domain_tx.clone();
                let retry_budget = retry_budget.clone();
                let content_handoff = content_handoff.clone();
                let timing = timing.clone();
                let retry_database = database.clone();
                let retry_pool = grpc_pool.clone();
                let retry_owner = current_user_id;
                group_polls.spawn(async move {
                    timing.delay_between_batch_items().await;
                    let healthy = poll_group_mailboxes(
                        &group_fanout,
                        &group_coordinator,
                        &message_gateway,
                        &event_tx,
                        &channel_repo,
                        &attachment_repo,
                        &message_repo,
                        domain_tx.as_ref(),
                        &retry_budget,
                        content_handoff.as_ref(),
                    )
                    .await;
                    if healthy {
                        if let Some(owner) = retry_owner {
                            let _ = crate::services::gateway::submission::retry_pending(
                                &retry_database,
                                &retry_pool,
                                &owner.to_string(),
                                None,
                                domain_tx.as_ref(),
                            )
                            .await;
                        }
                    }
                });
            }

            // Reap completed polls before starting another. JoinSet aborts its tasks
            // when the inbox loop stops, including on a transport switch.
            while let Some(result) = anon_polls.try_join_next() {
                match result {
                    Ok(true) => anon_backoff.reset(),
                    Ok(false) => anon_backoff.finished_empty(),
                    Err(error) => {
                        tracing::warn!(%error, "Anonymous DM polling task failed");
                        anon_backoff.finished_empty();
                    }
                }
            }
            let direct = grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Direct;
            if !direct
                || (anon_polls.is_empty()
                    && poll_hints
                        .anon_wake
                        .swap(false, std::sync::atomic::Ordering::AcqRel))
            {
                anon_backoff.reset();
            }
            if anon_polls.is_empty()
                && (!direct || tokio::time::Instant::now() >= anon_backoff.next_due)
            {
                if let (Some(anon_poller), Some(pairwise)) = (&anon_dm_poller, &pairwise_service) {
                    let anon_poller = anon_poller.clone();
                    let pairwise = pairwise.clone();
                    let timing = timing.clone();
                    let decoys = decoys.clone();
                    let grpc_pool = grpc_pool.clone();
                    let database = database.clone();
                    let message_repo = message_repo.clone();
                    let dm_deduplicator = dm_deduplicator.clone();
                    let event_tx = event_tx.clone();
                    let current_user_id = current_user_id.clone();
                    let domain_tx = domain_tx.clone();
                    let content_handoff = content_handoff.clone();
                    anon_polls.spawn(async move {
                        let mut found_messages = false;
                        if let Some(handoff) = &content_handoff {
                            handoff.replay(&message_repo).await;
                        }
                        let should_dummy_poll = || {
                            use rand::Rng;
                            rand::rng().random::<f64>() < dummy_poll_probability
                        };
                        // Bound preparation and Tor fetch separately from the
                        // direct mailbox cycle. Processing and ACK run outside
                        // this timeout, so the budget cannot cancel an ACK.
                        let poll_deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_secs(95);
                        // Random delay before anon DM polling
                        timing.delay_between_batch_items().await;

                        // Report each secret cycle before cover traffic or Tor
                        // calls, which can use the whole deadline.
                        let mailbox_keys = match anon_poller.get_active_mailbox_keys() {
                            Ok(keys) => keys,
                            Err(error) => {
                                tracing::warn!(%error, "Secret chat mailbox lookup failed");
                                return false;
                            }
                        };
                        let mailbox_count = mailbox_keys.len();
                        tracing::debug!(mailbox_count, "Secret chat polling cycle");

                        // Keep secret-chat cover traffic off the direct mailbox
                        // client's mutex so it cannot stall the next inbox cycle.
                        if should_dummy_poll() {
                            let dummy_client = Arc::new(tokio::sync::Mutex::new(
                                grpc_pool.anonymous_mailbox_client(),
                            ));
                            if tokio::time::timeout_at(
                                poll_deadline,
                                perform_dummy_poll(&dummy_client, &decoys),
                            )
                            .await
                            .is_err()
                            {
                                tracing::warn!("Anonymous DM cover poll exceeded 95-second budget");
                                return false;
                            }
                        }

                        // Anonymous DMs are secret chats: poll them over Tor only
                        // (no auth headers, challenge-response). No Tor route → skip.
                        if mailbox_count > 0 {
                            let polls = stream::iter(mailbox_keys).map(|(conversation_id, _, _)| {
                                let grpc_pool = grpc_pool.clone();
                                let anon_poller = anon_poller.clone();
                                async move {
                                    let mut client = grpc_pool.secret_mailbox_client()
                                        .map_err(|error| error.to_string())?;
                                    let result = tokio::time::timeout(
                                        std::time::Duration::from_secs(95),
                                        anon_poller.poll_mailbox(&conversation_id, &mut client, None),
                                    )
                                    .await
                                    .map_err(|_| "Secret mailbox poll timed out".to_string())?
                                    .map_err(|error| error.to_string())?;
                                    Ok::<_, String>((client, result))
                                }
                            }).buffer_unordered(4);
                            tokio::pin!(polls);
                            let mut secret_healthy = false;
                            let mut failed = false;
                            while let Some(outcome) = polls.next().await {
                                match outcome {
                                Ok((mut client, result)) => {
                                    secret_healthy = true;
                                    let results = vec![result];
                                    if results[0].has_messages() {
                                        found_messages = true;
                                        // Create mpsc channel for events (process_anon_dm_results expects mpsc)
                                        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                                        // Blocking check closure — uses the DB directly to
                                        // avoid threading AnonDmService through the polling layer.
                                        let anon_repo = crate::storage::repositories::AnonDmRepository::new(
                                            database.clone(),
                                        );
                                        let is_blocked = |sender: &crate::domain::UserId,
                                                          conv: &crate::domain::anon_dm::AnonDmId|
                                         -> bool {
                                            if anon_repo.is_user_blocked(sender).unwrap_or(false) {
                                                return true;
                                            }
                                            anon_repo
                                                .get_conversation(conv)
                                                .ok()
                                                .flatten()
                                                .map(|dm| {
                                                    dm.status
                                                        == crate::domain::anon_dm::AnonDmStatus::Blocked
                                                })
                                                .unwrap_or(false)
                                        };
                                        let bound_device = |conv: &crate::domain::anon_dm::AnonDmId| {
                                            anon_repo
                                                .get_conversation(conv)
                                                .ok()
                                                .flatten()
                                                .and_then(|dm| dm.peer_device_id)
                                        };
                                        // Capture exact current-envelope binding before processing and
                                        // retain it until the actual upstream ACK result is known.
                                        let ack_bindings:std::collections::HashMap<_,_>=results.iter().flat_map(|result|result.messages.iter().filter_map(|message|{
                                        message_repo.reaction_receive_key("anon_dm",result.conversation_id.into(),&message.message_id,message.server_timestamp).ok().map(|key|((result.conversation_id,message.message_id.clone()),(key,crate::storage::receive_artifact::envelope_digest(&message.encrypted_envelope))))
                                    })).collect();
                                        // Use deduplicator to prevent re-decryption when ack fails
                                        let processed = process_anon_dm_results(
                                            results,
                                            &pairwise,
                                            &tx,
                                            &dm_deduplicator,
                                            current_user_id.as_ref(),
                                            &is_blocked,
                                            &bound_device,
                                            &message_repo,
                                            &crate::storage::repositories::AnonGroupRepository::new(database.clone()),
                                            domain_tx.as_ref(),
                                            content_handoff.as_ref(),
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
                                                let ack_ids = message_ids.clone();
                                                let ack_result = anon_poller
                                                    .acknowledge_messages(
                                                        &conversation_id,
                                                        message_ids,
                                                        &mut client,
                                                    )
                                                    .await;
                                                if let Err(e) = &ack_result {
                                                    tracing::warn!(
                                                        conversation_id = %conversation_id,
                                                        error = %e,
                                                        "Failed to acknowledge anonymous DM messages"
                                                    );
                                                }
                                                if ack_result.is_ok() {
                                                    for id in ack_ids {
                                                        if let Some((key, digest)) =
                                                            ack_bindings.get(&(conversation_id, id.clone()))
                                                        {
                                                            if let Ok(Some(kind)) = message_repo.receive_artifact_kind(key, digest) {
                                                                tracing::info!(envelope_id = %id, kind = kind.as_str(), "secret mailbox ACK succeeded");
                                                            } else {
                                                                tracing::info!(envelope_id = %id, "secret mailbox ACK succeeded");
                                                            }
                                                            if crate::inbox::reaction_handoff::acknowledge(
                                                                &message_repo,
                                                                key,
                                                                digest,
                                                                true,
                                                            )
                                                            .is_err()
                                                            {
                                                                tracing::warn!(
                                                                    "Anonymous ACK receipt awaits storage recovery"
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    failed = true;
                                    tracing::warn!(%error, "Failed to poll anonymous DM mailbox");
                                }
                                }
                            }
                            if failed {
                                grpc_pool.disconnect_secret();
                            }
                            if secret_healthy {
                                if let Some(owner) = current_user_id.as_ref() {
                                    let _ = crate::services::gateway::submission::retry_pending(
                                        &database, &grpc_pool, &owner.to_string(), None,
                                        domain_tx.as_ref(),
                                    ).await;
                                }
                            }
                        }
                        found_messages
                    });
                }
            }

            // Drain up to twenty more pages at a cooperative cadence when the
            // server reports backlog; then give other work a normal interval.
            let base = if catch_up_pages > 0 {
                std::time::Duration::from_millis(250)
            } else {
                let interval = if grpc_pool.transport_mode()
                    == crate::network::grpc::client_pool::TransportMode::Direct
                {
                    poller.direct_idle_interval()
                } else {
                    poller.current_interval()
                };
                poll_hints.cycle_interval(interval)
            };
            let wait = timing
                .jitter_interval(base)
                .min(base)
                .saturating_sub(cycle_started.elapsed());
            if resumed {
                continue;
            }
            if !poll_hints
                .wait(wait, &mut shutdown_rx, || {
                    grpc_pool.transport_mode()
                        == crate::network::grpc::client_pool::TransportMode::Direct
                })
                .await
            {
                break;
            }
        }
    }

    /// Wake the normal cycle; ignored for pacing while on Shield transport.
    pub fn wake(&self) {
        self.poller.reset_idle_backoff();
        self.poll_hints
            .anon_wake
            .store(true, std::sync::atomic::Ordering::Release);
        self.poll_hints.wake.notify_one();
    }

    /// Update the Activity's foreground state without restarting the task.
    pub fn set_app_visible(&self, visible: bool) {
        if visible { self.poller.reset_idle_backoff(); }
        self.poll_hints.set_visible(visible);
    }

    /// Bound responsive polling to a key wait and a hard 65-second deadline.
    pub(crate) fn responsive_polling(&self) -> ResponsivePollGuard {
        self.poll_hints.begin()
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

#[cfg(test)]
mod responsive_poll_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn empty_secret_polls_back_off_with_a_two_minute_fallback() {
        let expected = [5, 10, 20, 40, 80, 120, 120];
        for (empty_polls, seconds) in expected.into_iter().enumerate() {
            assert_eq!(
                AnonPollBackoff::interval(empty_polls as u32),
                Duration::from_secs(seconds)
            );
        }
        assert_eq!(AnonPollBackoff::interval(u32::MAX), Duration::from_secs(120));
    }

    #[test]
    fn secret_wake_or_received_mail_resets_backoff() {
        let mut backoff = AnonPollBackoff::new();
        backoff.finished_empty();
        backoff.finished_empty();
        assert_eq!(backoff.empty_polls, 2);
        backoff.reset();
        assert_eq!(backoff.empty_polls, 0);
        assert!(backoff.next_due <= tokio::time::Instant::now());
    }

    #[tokio::test]
    async fn key_wait_wakes_and_restores_normal_cadence_on_drop() {
        let hints = Arc::new(PollHints::default());
        let base = Duration::from_secs(5);
        assert_eq!(hints.interval(base, true), base);
        let guard = hints.begin();
        tokio::time::timeout(Duration::from_millis(50), hints.wake.notified())
            .await
            .unwrap();
        assert_eq!(hints.interval(base, true), Duration::from_millis(250));
        assert_eq!(
            hints.interval(base, false),
            base,
            "Shield cadence stays unchanged"
        );
        let other = hints.begin();
        drop(guard);
        assert_eq!(hints.interval(base, true), Duration::from_millis(250));
        drop(other);
        assert_eq!(hints.interval(base, true), base);
    }

    #[test]
    fn abandoned_lease_expires_and_never_slows_a_shorter_base() {
        let hints = Arc::new(PollHints::default());
        let _guard = hints.begin();
        assert_eq!(
            hints.interval(Duration::from_millis(100), true),
            Duration::from_millis(100)
        );
        hints.leases.lock()[0].1 = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            hints.interval(Duration::from_secs(5), true),
            Duration::from_secs(5)
        );
    }

    #[tokio::test]
    async fn wake_interrupts_existing_sleep_and_shield_transition_restores_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (shield, responsive) in [(false, false), (true, false), (false, true), (true, true)] {
            let hints = Arc::new(PollHints::default());
            let _guard = responsive.then(|| hints.begin());
            let direct = Arc::new(AtomicBool::new(true));
            let (_tx, mut rx) = watch::channel(false);
            let task_hints = hints.clone();
            let task_direct = direct.clone();
            let start = tokio::time::Instant::now();
            let task = tokio::spawn(async move {
                task_hints
                    .wait(Duration::from_millis(600), &mut rx, || {
                        task_direct.load(Ordering::Relaxed)
                    })
                    .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            hints.wake.notify_one();
            if shield {
                tokio::time::sleep(Duration::from_millis(20)).await;
                direct.store(false, Ordering::Relaxed);
            }
            assert!(task.await.unwrap());
            if shield {
                assert!(start.elapsed() >= Duration::from_millis(600));
            } else {
                assert!(start.elapsed() < Duration::from_millis(550));
                assert!(start.elapsed() >= Duration::from_millis(250));
            }
        }
    }

    #[tokio::test]
    async fn shutdown_interrupts_responsive_sleep() {
        let hints = Arc::new(PollHints::default());
        let _guard = hints.begin();
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();
        assert!(!hints.wait(Duration::from_secs(5), &mut rx, || true).await);
    }

    #[tokio::test]
    async fn push_bursts_coalesce_to_one_pending_wake() {
        let hints = PollHints::default();
        for _ in 0..100 {
            hints.wake.notify_one();
        }
        tokio::time::timeout(Duration::from_millis(50), hints.wake.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), hints.wake.notified())
                .await
                .is_err()
        );
    }
}
