//! Sync engine.
//!
//! Main synchronization orchestrator that coordinates channel, message,
//! and user sync operations. Runs as a background task with periodic
//! sync cycles and immediate sync on reconnect.
//!
//! A cycle that fails is not terminal. While the server looks reachable the
//! engine retries with a capped exponential backoff; while it is unreachable
//! the engine waits for the offline→online edge of [`subscribe_server_status`]
//! (the same signal the "No server" banner mirrors into `OfflineState`) and
//! runs again. One cycle is in flight at a time.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::domain::ids::ChannelId;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::connection::{ServerStatus, subscribe_server_status};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::message::channel_inbound::ChannelInbound;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;
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
/// run back to back at every start. A cycle that *failed* does not use this
/// skip: the retry has to run.
const TIMER_SKIP_IF_FRESH: Duration = Duration::from_secs(30);

/// First delay after a failed cycle while the server still looks reachable.
/// Doubles each consecutive failure, capped at [`RETRY_MAX`].
const RETRY_INITIAL: Duration = Duration::from_secs(1);

/// Cap on the failed-cycle backoff. Past this the engine keeps trying once
/// a minute while the server looks reachable, and waits for the online edge
/// when it does not.
const RETRY_MAX: Duration = Duration::from_secs(60);

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
        Self {
            started: AtomicU64::new(0),
            last_done: tokio::sync::Mutex::new(None),
        }
    }

    /// Run `cycle` unless a cycle that started after this call has finished by
    /// the time the lock is ours, or one finished within `skip_if_fresh`
    /// (`Duration::ZERO` = never skip on age). `None` when skipped.
    async fn run<F: Future>(
        &self,
        skip_if_fresh: Duration,
        cycle: impl FnOnce() -> F,
    ) -> Option<F::Output> {
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
    coalescer: Coalescer,
    /// Set at the end of every cycle, including one this engine did not start
    /// (the channel stream's catch-up shares the coalescer). The retry loop
    /// reads it when its own trigger was coalesced into that cycle.
    needs_retry: AtomicBool,
}

/// Result of a full sync cycle.
#[derive(Debug, Default)]
pub struct SyncCycleResult {
    pub channels: ChannelSyncResult,
    pub messages: FullMessageSyncResult,
    pub users: UserSyncResult,
    pub duration_ms: u64,
    /// Channel list RPC failed, or a per-channel step left the pass incomplete.
    pub channels_failed: bool,
    /// Message sync failed, or it was skipped because the channel list failed.
    pub messages_failed: bool,
    /// Block-list RPC failed. Blocks made on another device are not in force
    /// until a later cycle succeeds.
    pub blocks_failed: bool,
    /// Friends RPC failed, or a row left the pass incomplete.
    pub friends_failed: bool,
}

impl SyncCycleResult {
    /// A failed step is retried. The cycle itself is not a terminal state.
    pub fn needs_retry(&self) -> bool {
        self.channels_failed || self.messages_failed || self.blocks_failed || self.friends_failed
    }
}

impl SyncEngine {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        event_bus: EventBus,
        inbound: Arc<ChannelInbound>,
        channel_events: Arc<crate::services::channel_event_service::ChannelEventService>,
    ) -> Self {
        Self {
            channel_sync: ChannelSync::new(
                grpc_pool.clone(),
                channel_repo.clone(),
                settings_repo.clone(),
                channel_events,
            ),
            message_sync: MessageSync::new(grpc_pool.clone(), settings_repo.clone(), inbound),
            user_sync: UserSync::new(grpc_pool, user_repo, settings_repo.clone()),
            event_bus,
            coalescer: Coalescer::new(),
            needs_retry: AtomicBool::new(false),
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
                result.channels_failed = channel_result.incomplete;
                result.channels = channel_result;

                // Step 2: Sync messages for all channels
                let channel_ids: Vec<ChannelId> = channels.iter().map(|c| c.id).collect();
                result.messages = self.message_sync.sync_all(&channel_ids).await;
                result.messages_failed = !result.messages.errors.is_empty();
            }
            Err(e) => {
                result.channels_failed = true;
                // No channel list, so message sync cannot run. It stays failed
                // until a later cycle gets the list.
                result.messages_failed = true;
                tracing::error!(error = %e, "Channel sync failed — skipping message sync");
            }
        }

        // Step 3: Sync the block list. Separate from friends because it is a
        // separate RPC, and run unconditionally because a failed friends pass
        // must not stop blocks from landing — the inbound block filter reads
        // the rows this writes, so skipping it means a blocked sender's
        // messages are delivered on this device. Before the friends pass: a
        // lifted block turns back into an accepted row, which that pass prunes
        // if the person was never a friend.
        let mut blocks_changed = false;
        let mut blocked_synced = 0;
        match self.user_sync.sync_blocked_users().await {
            Ok((count, changed)) => {
                blocked_synced = count;
                blocks_changed = changed;
            }
            Err(e) => {
                result.blocks_failed = true;
                tracing::warn!(
                    error = %e,
                    "Blocked-user sync failed — retrying so a block made on another device is enforced"
                );
            }
        }

        // Step 4: Sync users/friends
        match self.user_sync.sync_friends().await {
            Ok(user_result) => {
                result.friends_failed = user_result.incomplete;
                result.users = user_result;
            }
            Err(e) => {
                result.friends_failed = true;
                tracing::warn!(error = %e, "User sync failed — retrying");
            }
        }
        result.users.blocked_synced = blocked_synced;

        result.duration_ms = start.elapsed().as_millis() as u64;
        self.needs_retry
            .store(result.needs_retry(), Ordering::SeqCst);

        // Emit sync complete event. `blocks_changed` is what rebuilds the
        // blocked-people lists. A newly accepted friend is a different event:
        // the contacts tab reloads every friend from storage when it hears
        // one `FriendAccepted`, so a single id covers the whole pass.
        let _ = self
            .event_bus
            .domain_sender()
            .send(DomainEvent::SyncComplete {
                channels_synced: result.channels.synced,
                messages_synced: result.messages.total_inserted,
                duration_ms: result.duration_ms,
                blocks_changed,
            });
        if let Some(friend_id) = result.users.new_friend_ids.first().copied() {
            let _ = self
                .event_bus
                .domain_sender()
                .send(DomainEvent::FriendAccepted { friend_id });
        }

        tracing::info!(
            channels = result.channels.synced,
            messages = result.messages.total_inserted,
            friends = result.users.friends_synced,
            retry = result.needs_retry(),
            duration_ms = result.duration_ms,
            "Sync cycle complete"
        );

        result
    }

    /// Run the sync engine as a background task.
    ///
    /// One cycle after a short settle delay, then either the 5-minute
    /// periodic cadence or a retry of a failed cycle. Shuts down when
    /// `shutdown_rx` receives `true`.
    pub async fn run(self: Arc<Self>, shutdown_rx: watch::Receiver<bool>) {
        tracing::info!("Sync engine started");
        let engine = Arc::clone(&self);
        drive(
            RetryTiming::production(),
            &self.coalescer,
            &self.needs_retry,
            shutdown_rx,
            subscribe_server_status(),
            || {
                let engine = Arc::clone(&engine);
                async move { engine.sync_all().await.needs_retry() }
            },
        )
        .await;
        tracing::info!("Sync engine shutting down");
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

/// Delays for [`drive`]. Production uses the engine constants; tests pass
/// zeros so a failed step converges without sleeping.
struct RetryTiming {
    initial_delay: Duration,
    periodic: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    skip_if_fresh: Duration,
}

impl RetryTiming {
    fn production() -> Self {
        Self {
            initial_delay: RECONNECT_SYNC_DELAY,
            periodic: SYNC_INTERVAL,
            retry_initial: RETRY_INITIAL,
            retry_max: RETRY_MAX,
            skip_if_fresh: TIMER_SKIP_IF_FRESH,
        }
    }
}

/// What to do after a cycle. A healthy cycle waits out the periodic interval.
/// A failed cycle backs off while the server looks reachable (or has not
/// reported yet — a permission error does not flip the wire status) and,
/// once the wire says unreachable, waits for the online edge instead of
/// hammering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitPlan {
    Periodic(Duration),
    Backoff(Duration),
    UntilOnline,
}

fn became_reachable(before: ServerStatus, after: ServerStatus) -> bool {
    after == ServerStatus::Reachable && before != ServerStatus::Reachable
}

fn plan_after_cycle(
    failed: bool,
    status: ServerStatus,
    backoff: Duration,
    periodic: Duration,
) -> WaitPlan {
    if !failed {
        return WaitPlan::Periodic(periodic);
    }
    match status {
        ServerStatus::Unreachable => WaitPlan::UntilOnline,
        ServerStatus::Reachable | ServerStatus::Unknown => WaitPlan::Backoff(backoff),
    }
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    if current.is_zero() || max.is_zero() {
        return Duration::ZERO;
    }
    current.saturating_mul(2).min(max)
}

/// `true` when another cycle should run. `false` when shutting down.
async fn sleep_or_shutdown(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    if delay.is_zero() {
        return true;
    }
    tokio::select! {
        biased;
        res = shutdown.changed() => match res {
            Ok(()) => !*shutdown.borrow(),
            Err(_) => false,
        },
        _ = tokio::time::sleep(delay) => true,
    }
}

/// Wait out `plan`. An offline→online transition ends the wait early, including
/// one that landed after the cycle was planned. Going unreachable during a
/// backoff stops the timer and waits for the server instead.
///
/// `true` = run another cycle. `false` = shut down.
async fn wait_plan(
    mut plan: WaitPlan,
    planned_against: ServerStatus,
    shutdown: &mut watch::Receiver<bool>,
    status: &mut watch::Receiver<ServerStatus>,
) -> bool {
    let mut saw = planned_against;
    loop {
        if *shutdown.borrow() {
            return false;
        }
        // Mark this value seen so `changed` below only wakes on a later write.
        let current = *status.borrow_and_update();
        if became_reachable(saw, current) {
            return true;
        }
        if current == ServerStatus::Unreachable {
            if let WaitPlan::Backoff(_) = plan {
                plan = WaitPlan::UntilOnline;
            }
        }
        if matches!(plan, WaitPlan::UntilOnline) && current == ServerStatus::Reachable {
            return true;
        }
        saw = current;

        let sleep_for = match plan {
            WaitPlan::Periodic(d) | WaitPlan::Backoff(d) => Some(d),
            WaitPlan::UntilOnline => None,
        };
        tokio::select! {
            biased;
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    return false;
                }
            }
            res = status.changed() => {
                if res.is_err() {
                    return false;
                }
            }
            _ = async {
                match sleep_for {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => return true,
        }
    }
}

/// Initial settle, then cycles until shutdown.
///
/// `cycle` returns whether that cycle needs a retry. `needs_retry` is also
/// updated by whatever ran the cycle when this call was coalesced into one
/// that started earlier — the channel stream's catch-up does that — so a
/// skip still observes the failure.
async fn drive<F, Fut>(
    timing: RetryTiming,
    coalescer: &Coalescer,
    needs_retry: &AtomicBool,
    mut shutdown: watch::Receiver<bool>,
    mut status: watch::Receiver<ServerStatus>,
    mut cycle: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    if !sleep_or_shutdown(timing.initial_delay, &mut shutdown).await {
        return;
    }
    let mut backoff = timing.retry_initial;
    // Consecutive failed cycles. Reset only by a cycle that succeeds, never by
    // a connectivity edge.
    let mut failures: u32 = 0;
    loop {
        if *shutdown.borrow() {
            return;
        }
        // A failed cycle must not be swallowed by the freshness skip. A
        // healthy one still yields to a catch-up that just finished.
        let skip = if needs_retry.load(Ordering::SeqCst) {
            Duration::ZERO
        } else {
            timing.skip_if_fresh
        };
        let status_before = *status.borrow();
        let outcome = coalescer.run(skip, || cycle()).await;
        let failed = match outcome {
            Some(failed) => failed,
            None => needs_retry.load(Ordering::SeqCst),
        };
        let failed_at = Instant::now();
        let status_after = *status.borrow();
        if failed {
            failures = failures.saturating_add(1);
        } else {
            failures = 0;
            backoff = timing.retry_initial;
        }
        // The server came back during the cycle. Retry now, but only for a
        // first failure — the outage that just ended explains it.
        if failures == 1 && became_reachable(status_before, status_after) {
            continue;
        }
        let plan = plan_after_cycle(failed, status_after, backoff, timing.periodic);
        if failed {
            match plan {
                WaitPlan::UntilOnline => {
                    tracing::info!("Sync incomplete — waiting for the server");
                }
                WaitPlan::Backoff(delay) => {
                    tracing::info!(
                        retry_in_ms = delay.as_millis() as u64,
                        "Sync incomplete — retrying"
                    );
                }
                WaitPlan::Periodic(_) => {}
            }
        }
        if !wait_plan(plan, status_after, &mut shutdown, &mut status).await {
            return;
        }
        if failed {
            // An online edge ends the wait early. For a step that keeps
            // failing it does not end the backoff: otherwise anyone able to
            // reset the connection turns every flap into a full cycle. An
            // outage longer than the backoff has already used it up.
            if failures > 1 {
                let remaining = backoff.saturating_sub(failed_at.elapsed());
                if !sleep_or_shutdown(remaining, &mut shutdown).await {
                    return;
                }
            }
            backoff = next_backoff(backoff, timing.retry_max);
        }
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

        let spawn =
            |c: Arc<Coalescer>, runs: Arc<AtomicUsize>, release: Arc<tokio::sync::Notify>| {
                tokio::spawn(async move {
                    c.run(Duration::ZERO, || gated_cycle(&runs, &release))
                        .await
                        .is_some()
                })
            };

        let first = spawn(c.clone(), runs.clone(), release.clone());
        while runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Five triggers land while the first cycle is in flight.
        let burst: Vec<_> = (0..5)
            .map(|_| spawn(c.clone(), runs.clone(), release.clone()))
            .collect();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "no second cycle while one is in flight"
        );

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
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the burst cost one follow-up, not five"
        );
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

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fast_timing() -> RetryTiming {
        RetryTiming {
            initial_delay: Duration::ZERO,
            // Park after success so the test can stop the loop itself.
            periodic: Duration::from_secs(3600),
            retry_initial: Duration::ZERO,
            retry_max: Duration::from_secs(60),
            skip_if_fresh: Duration::ZERO,
        }
    }

    #[test]
    fn a_failed_cycle_backs_off_while_reachable_and_waits_while_not() {
        let periodic = Duration::from_secs(300);
        let backoff = Duration::from_secs(2);
        assert_eq!(
            plan_after_cycle(false, ServerStatus::Reachable, backoff, periodic),
            WaitPlan::Periodic(periodic)
        );
        assert_eq!(
            plan_after_cycle(true, ServerStatus::Reachable, backoff, periodic),
            WaitPlan::Backoff(backoff),
            "a reachable server (or a permission error that left it reachable) retries on a timer"
        );
        assert_eq!(
            plan_after_cycle(true, ServerStatus::Unknown, backoff, periodic),
            WaitPlan::Backoff(backoff),
            "no wire report yet is not a reason to wait forever"
        );
        assert_eq!(
            plan_after_cycle(true, ServerStatus::Unreachable, backoff, periodic),
            WaitPlan::UntilOnline
        );
        assert!(became_reachable(
            ServerStatus::Unreachable,
            ServerStatus::Reachable
        ));
        assert!(became_reachable(
            ServerStatus::Unknown,
            ServerStatus::Reachable
        ));
        assert!(!became_reachable(
            ServerStatus::Reachable,
            ServerStatus::Reachable
        ));
        assert_eq!(
            next_backoff(Duration::from_secs(8), RETRY_MAX),
            Duration::from_secs(16)
        );
        assert_eq!(next_backoff(Duration::from_secs(32), RETRY_MAX), RETRY_MAX);
    }

    #[tokio::test]
    async fn a_failed_step_retries_until_it_succeeds() {
        let runs = Arc::new(AtomicUsize::new(0));
        let needs = Arc::new(AtomicBool::new(false));
        let coalescer = Coalescer::new();
        let (shut_tx, shut_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(ServerStatus::Reachable);
        let runs_c = Arc::clone(&runs);
        let needs_c = Arc::clone(&needs);
        let shut_c = shut_tx.clone();

        let driver = tokio::spawn(async move {
            drive(
                fast_timing(),
                &coalescer,
                needs_c.as_ref(),
                shut_rx,
                status_rx,
                || {
                    let runs = Arc::clone(&runs_c);
                    let needs = Arc::clone(&needs_c);
                    let shut = shut_c.clone();
                    async move {
                        let n = runs.fetch_add(1, Ordering::SeqCst);
                        let failed = n == 0;
                        needs.store(failed, Ordering::SeqCst);
                        if !failed {
                            let _ = shut.send(true);
                        }
                        failed
                    }
                },
            )
            .await;
        });

        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("retry converged without a restart")
            .unwrap();
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "fail once, then the retry succeeds"
        );
    }

    /// Finding #7: a step that keeps failing must not be rerun on every
    /// offline→online flap. The first failure still retries on the edge.
    #[tokio::test]
    async fn connectivity_flaps_do_not_reset_a_persistent_failure() {
        let runs = Arc::new(AtomicUsize::new(0));
        let needs = Arc::new(AtomicBool::new(false));
        let coalescer = Coalescer::new();
        let (shut_tx, shut_rx) = watch::channel(false);
        let (status_tx, status_rx) = watch::channel(ServerStatus::Reachable);
        let runs_c = Arc::clone(&runs);
        let needs_c = Arc::clone(&needs);

        let mut timing = fast_timing();
        timing.retry_initial = Duration::from_secs(3600);
        timing.retry_max = Duration::from_secs(3600);

        let driver = tokio::spawn(async move {
            drive(
                timing,
                &coalescer,
                needs_c.as_ref(),
                shut_rx,
                status_rx,
                || {
                    let runs = Arc::clone(&runs_c);
                    let needs = Arc::clone(&needs_c);
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        needs.store(true, Ordering::SeqCst);
                        true
                    }
                },
            )
            .await;
        });

        let flap = || async {
            status_tx.send(ServerStatus::Unreachable).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            status_tx.send(ServerStatus::Reachable).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        flap().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "a first failure retries on the online edge"
        );
        flap().await;
        flap().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "a repeat failure keeps its backoff across flaps"
        );

        shut_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_failure_retries_when_the_server_comes_back() {
        let runs = Arc::new(AtomicUsize::new(0));
        let needs = Arc::new(AtomicBool::new(false));
        let first_done = Arc::new(tokio::sync::Notify::new());
        let coalescer = Coalescer::new();
        let (shut_tx, shut_rx) = watch::channel(false);
        let (status_tx, status_rx) = watch::channel(ServerStatus::Unreachable);
        let runs_c = Arc::clone(&runs);
        let needs_c = Arc::clone(&needs);
        let first_c = Arc::clone(&first_done);
        let shut_c = shut_tx.clone();

        // The backoff is an hour. Only the online edge may start the second cycle.
        let mut timing = fast_timing();
        timing.retry_initial = Duration::from_secs(3600);
        timing.retry_max = Duration::from_secs(3600);

        let driver = tokio::spawn(async move {
            drive(
                timing,
                &coalescer,
                needs_c.as_ref(),
                shut_rx,
                status_rx,
                || {
                    let runs = Arc::clone(&runs_c);
                    let needs = Arc::clone(&needs_c);
                    let first = Arc::clone(&first_c);
                    let shut = shut_c.clone();
                    async move {
                        let n = runs.fetch_add(1, Ordering::SeqCst);
                        let failed = n == 0;
                        needs.store(failed, Ordering::SeqCst);
                        if n == 0 {
                            first.notify_one();
                        } else {
                            let _ = shut.send(true);
                        }
                        failed
                    }
                },
            )
            .await;
        });

        tokio::time::timeout(Duration::from_secs(5), first_done.notified())
            .await
            .expect("first cycle");
        // Still inside the failing cycle, so this edge is the one the driver
        // must notice — either mid-cycle or once it has parked on UntilOnline.
        status_tx.send(ServerStatus::Reachable).unwrap();

        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("online edge retried the failed cycle")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_triggers_do_not_run_the_step_in_parallel() {
        let coalescer = Arc::new(Coalescer::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let needs = Arc::new(AtomicBool::new(false));
        let (shut_tx, shut_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(ServerStatus::Reachable);

        let c_drive = Arc::clone(&coalescer);
        let runs_d = Arc::clone(&runs);
        let inflight_d = Arc::clone(&inflight);
        let release_d = Arc::clone(&release);
        let needs_d = Arc::clone(&needs);

        let driver = tokio::spawn(async move {
            drive(
                fast_timing(),
                &c_drive,
                needs_d.as_ref(),
                shut_rx,
                status_rx,
                || {
                    let runs = Arc::clone(&runs_d);
                    let inflight = Arc::clone(&inflight_d);
                    let release = Arc::clone(&release_d);
                    async move {
                        let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        assert_eq!(now, 1, "a second cycle entered while one was in flight");
                        let n = runs.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            release.notified().await;
                        }
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        false
                    }
                },
            )
            .await;
        });

        while runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let mut burst = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&coalescer);
            let runs = Arc::clone(&runs);
            let inflight = Arc::clone(&inflight);
            let release = Arc::clone(&release);
            burst.push(tokio::spawn(async move {
                c.run(Duration::ZERO, || {
                    let runs = Arc::clone(&runs);
                    let inflight = Arc::clone(&inflight);
                    let release = Arc::clone(&release);
                    async move {
                        let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        assert_eq!(now, 1, "a trigger overlapped the in-flight cycle");
                        runs.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        inflight.fetch_sub(1, Ordering::SeqCst);
                    }
                })
                .await
                .is_some()
            }));
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "no second cycle while one is in flight"
        );
        assert_eq!(inflight.load(Ordering::SeqCst), 1);

        release.notify_one();
        while runs.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        release.notify_one();

        let mut ran = 0;
        for handle in burst {
            ran += handle.await.unwrap() as usize;
        }
        assert_eq!(ran, 1, "exactly one of the burst runs the follow-up");
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        let _ = shut_tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("driver stopped")
            .unwrap();
    }
}
