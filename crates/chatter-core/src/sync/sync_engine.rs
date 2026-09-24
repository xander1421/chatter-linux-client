//! Sync engine.
//!
//! Main synchronization orchestrator that coordinates channel, message,
//! and user sync operations. Runs as a background task with periodic
//! sync cycles and immediate sync on reconnect.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::domain::ids::ChannelId;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;
use crate::services::message::channel_inbound::ChannelInbound;
use crate::storage::repositories::user_repo::UserRepository;

use super::channel_sync::{ChannelSync, ChannelSyncResult};
use super::message_sync::{FullMessageSyncResult, MessageSync};
use super::user_sync::{UserSync, UserSyncResult};

/// Interval between periodic sync cycles (5 minutes).
const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Short delay after reconnect before triggering sync.
const RECONNECT_SYNC_DELAY: Duration = Duration::from_secs(2);

/// The engine's own timer (initial and periodic) skips a cycle when some
/// other trigger — usually the channel stream's per-connect catch-up —
/// finished one this recently. That catch-up and the 2 s initial sync used to
/// run back to back at every start.
const TIMER_SKIP_IF_FRESH: Duration = Duration::from_secs(30);

/// One sync cycle in flight at a time, and a burst of triggers that arrive
/// while it runs costs at most ONE follow-up.
///
/// Every caller waits for a cycle that *started after it asked* — a trigger
/// that arrives mid-cycle cannot be satisfied by that cycle (the channel
/// stream's post-subscribe catch-up depends on this, OPEN-ITEMS §103), so the
/// first waiter runs a follow-up and every other waiter rides on it.
struct Coalescer {
    /// Cycles started so far. Bumped under `last_done`'s lock, before the work.
    started: AtomicU64,
    /// Held for the whole cycle; holds when the last cycle finished.
    last_done: tokio::sync::Mutex<Option<Instant>>,
}

impl Coalescer {
    fn new() -> Self {
        Self { started: AtomicU64::new(0), last_done: tokio::sync::Mutex::new(None) }
    }

    /// Run `cycle` unless a cycle that started after this call has finished by
    /// the time the lock is ours, or one finished within `skip_if_fresh`
    /// (`Duration::ZERO` = never skip on age). `None` when skipped.
    async fn run<F: Future>(&self, skip_if_fresh: Duration, cycle: impl FnOnce() -> F) -> Option<F::Output> {
        let seen = self.started.load(Ordering::SeqCst);
        let mut last_done = self.last_done.lock().await;
        if self.started.load(Ordering::SeqCst) != seen {
            return None;
        }
        if last_done.is_some_and(|t| t.elapsed() < skip_if_fresh) {
            return None;
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        let out = cycle().await;
        *last_done = Some(Instant::now());
        Some(out)
    }
}

/// Sync engine orchestrating all sync operations.
pub struct SyncEngine {
    channel_sync: ChannelSync,
    message_sync: MessageSync,
    user_sync: UserSync,
    event_bus: EventBus,
    #[allow(dead_code)]
    settings_repo: Arc<SettingsRepository>,
    coalescer: Coalescer,
}

/// Result of a full sync cycle.
#[derive(Debug, Default)]
pub struct SyncCycleResult {
    pub channels: ChannelSyncResult,
    pub messages: FullMessageSyncResult,
    pub users: UserSyncResult,
    pub duration_ms: u64,
}

impl SyncEngine {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        message_repo: Arc<UnifiedMessageRepository>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        event_bus: EventBus,
        inbound: Arc<ChannelInbound>,
    ) -> Self {
        Self {
            channel_sync: ChannelSync::new(
                grpc_pool.clone(),
                channel_repo.clone(),
                settings_repo.clone(),
            ),
            message_sync: MessageSync::new(
                grpc_pool.clone(),
                message_repo,
                settings_repo.clone(),
                channel_repo,
                inbound,
            ),
            user_sync: UserSync::new(
                grpc_pool,
                user_repo,
                settings_repo.clone(),
            ),
            event_bus,
            settings_repo,
            coalescer: Coalescer::new(),
        }
    }

    /// Run a full sync cycle: channels → messages → users.
    ///
    /// Channels are synced first so we know which channels to fetch messages for.
    /// Only ever called through `coalescer` — never run two of these at once.
    async fn sync_all(&self) -> SyncCycleResult {
        let start = std::time::Instant::now();
        let mut result = SyncCycleResult::default();

        // Step 1: Sync channels
        match self.channel_sync.sync().await {
            Ok((channels, channel_result)) => {
                result.channels = channel_result;

                // Step 2: Sync messages for all channels
                let channel_ids: Vec<ChannelId> = channels.iter().map(|c| c.id).collect();
                result.messages = self.message_sync.sync_all(&channel_ids).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Channel sync failed — skipping message sync");
            }
        }

        // Step 3: Sync users/friends
        match self.user_sync.sync_friends().await {
            Ok(user_result) => {
                result.users = user_result;
            }
            Err(e) => {
                tracing::warn!(error = %e, "User sync failed");
            }
        }

        // Step 4: Sync the block list. Separate from friends because it is a
        // separate RPC, and run unconditionally because a failed friends pass
        // must not stop blocks from landing — the inbound block filter reads
        // the rows this writes, so skipping it means a blocked sender's
        // messages are delivered on this device.
        match self.user_sync.sync_blocked_users().await {
            Ok(count) => result.users.blocked_synced = count,
            Err(e) => {
                tracing::warn!(error = %e, "Blocked-user sync failed — blocks made on \
                                           another device are not enforced here yet");
            }
        }

        result.duration_ms = start.elapsed().as_millis() as u64;

        // Emit sync complete event
        let _ = self.event_bus.domain_sender().send(DomainEvent::SyncComplete {
            channels_synced: result.channels.synced,
            messages_synced: result.messages.total_inserted,
            duration_ms: result.duration_ms,
        });

        tracing::info!(
            channels = result.channels.synced,
            messages = result.messages.total_inserted,
            friends = result.users.friends_synced,
            duration_ms = result.duration_ms,
            "Sync cycle complete"
        );

        result
    }

    /// Run the sync engine as a background task.
    ///
    /// Performs an initial sync immediately, then periodic syncs at `SYNC_INTERVAL`.
    /// Shuts down when `shutdown_rx` receives `true`.
    pub async fn run(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        tracing::info!("Sync engine started");

        // Initial sync after a short delay (let connections stabilize)
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_SYNC_DELAY) => {
                self.sync_if_stale(TIMER_SKIP_IF_FRESH).await;
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("Sync engine shutting down before initial sync");
                return;
            }
        }

        // Periodic sync loop
        let mut interval = tokio::time::interval(SYNC_INTERVAL);
        interval.tick().await; // Skip the first immediate tick (already synced above)

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.sync_if_stale(TIMER_SKIP_IF_FRESH).await;
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("Sync engine shutting down");
                    return;
                }
            }
        }
    }

    /// Trigger an immediate sync (e.g., after reconnect). Waits for a cycle
    /// that started after this call; `None` when another trigger's cycle was
    /// that one.
    pub async fn trigger_sync(&self) -> Option<SyncCycleResult> {
        tracing::info!("Triggered immediate sync");
        self.sync_if_stale(Duration::ZERO).await
    }

    /// Like [`Self::trigger_sync`], but also skipped when a cycle finished
    /// within `max_age`.
    pub async fn sync_if_stale(&self, max_age: Duration) -> Option<SyncCycleResult> {
        let result = self.coalescer.run(max_age, || self.sync_all()).await;
        if result.is_none() {
            tracing::debug!("Sync trigger coalesced into another cycle");
        }
        result
    }

    /// Check if this is the first sync (no channels synced yet).
    pub fn is_initial_sync(&self) -> bool {
        self.channel_sync.last_synced_at().is_none()
    }
}

#[cfg(test)]
mod coalescer_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A cycle that counts itself and blocks until `release` fires.
    async fn gated_cycle(runs: &AtomicUsize, release: &tokio::sync::Notify) {
        runs.fetch_add(1, Ordering::SeqCst);
        release.notified().await;
    }

    #[tokio::test]
    async fn a_burst_during_a_cycle_costs_exactly_one_follow_up() {
        let c = Arc::new(Coalescer::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());

        let spawn = |c: Arc<Coalescer>, runs: Arc<AtomicUsize>, release: Arc<tokio::sync::Notify>| {
            tokio::spawn(async move {
                c.run(Duration::ZERO, || gated_cycle(&runs, &release)).await.is_some()
            })
        };

        let first = spawn(c.clone(), runs.clone(), release.clone());
        while runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Five triggers land while the first cycle is in flight.
        let burst: Vec<_> = (0..5).map(|_| spawn(c.clone(), runs.clone(), release.clone())).collect();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no second cycle while one is in flight");

        release.notify_one(); // finish the first cycle
        while runs.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        release.notify_one(); // finish the follow-up

        assert!(first.await.unwrap());
        let mut ran = 0;
        for h in burst {
            ran += h.await.unwrap() as usize;
        }
        assert_eq!(ran, 1, "exactly one burst trigger runs the follow-up");
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the burst cost one follow-up, not five");
    }

    #[tokio::test]
    async fn sequential_triggers_each_run() {
        // A trigger after the previous cycle finished is not coalesced: that
        // cycle started before it asked.
        let c = Coalescer::new();
        assert_eq!(c.run(Duration::ZERO, || async { 1 }).await, Some(1));
        assert_eq!(c.run(Duration::ZERO, || async { 2 }).await, Some(2));
    }

    #[tokio::test]
    async fn a_fresh_cycle_satisfies_a_timer_trigger_only() {
        let c = Coalescer::new();
        assert!(c.run(Duration::ZERO, || async {}).await.is_some());
        assert!(
            c.run(Duration::from_secs(30), || async {}).await.is_none(),
            "the engine timer skips when a cycle just finished"
        );
        assert!(
            c.run(Duration::ZERO, || async {}).await.is_some(),
            "a forced trigger still runs"
        );
    }
}
