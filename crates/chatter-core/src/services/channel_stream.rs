//! Live channel-message stream (server-managed channels).
//!
//! # Why this exists (INFRA-1)
//!
//! Server-managed channel messages reach a client two ways:
//!
//! 1. **Durably**, from Postgres via `GetMessages` — that is the system of
//!    record, and `sync::MessageSync` reads it.
//! 2. **Live**, over `StreamGlobalEvents` — low latency, best effort.
//!
//! Before this service the live half was per-channel `StreamMessageEvents`
//! subscriptions started from exactly two places (`create_channel` and
//! `join_channel`), with no re-subscription at login and no reconnect loop.
//! After any restart or stream error a user received zero live messages in
//! every channel joined in a previous session, and opening the channel only
//! read local SQLite — so nothing backfilled either.
//!
//! This service replaces all of that with a single task:
//!
//! - one `StreamGlobalEvents` stream for every channel the user is in (the
//!   server auto-subscribes from its own membership list when `channel_ids`
//!   is empty, which is what makes a restart recover),
//! - a reconnect loop with capped backoff,
//! - a durable message catch-up on every connect, so
//!   correctness never depends on the live stream having been up, and
//! - a resume cursor (`event_id`) persisted across runs so a short drop
//!   replays from the server's event log instead of relying on the catch-up.
//!
//! The catch-up is the correctness path; the cursor is a latency
//! optimisation. The server's event log is a bounded replay buffer, and when
//! a cursor has fallen out of it (or replaying would exceed the server's
//! budget) the server says so with a `ResyncRequired` event instead of
//! replaying a partial history. That event means "what you missed is not
//! coming on this stream": we hold our old cursor until the durable
//! mutation snapshots have been fully applied, while continuing live delivery (OPEN-ITEMS §40/§56).

use std::sync::Arc;
use std::time::Duration;

use crate::domain::ids::{ChannelId, MessageId};
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::streaming::heartbeat::{self, IsHeartbeat, Next};
use crate::proto::chat_v1::{self, message_event::Event as MsgEventType};
use crate::services::message::MessageService;
use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
use crate::services::sync_service::SyncService;
use crate::storage::repositories::settings_repo::SettingsRepository;

// Process-wide diagnostic counter; also emitted as a structured metric in logs.
static RESYNC_REJECTED_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn resync_row_complete(
    result: Result<(), crate::services::message::MessageError>,
    channel: &str,
    message: &str,
) -> Result<bool, crate::error::NetworkError> {
    use crate::services::message::MessageError;
    match result {
        Ok(()) => Ok(true),
        Err(MessageError::ResyncPending | MessageError::Storage(_)) => Ok(false),
        Err(MessageError::InvalidContent(error)) => {
            let total = RESYNC_REJECTED_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::warn!(
                channel_id = channel,
                message_id = message,
                error,
                resync_rejected_rows_total = total,
                "Skipping rejected resync row; retained local history wins identity conflicts"
            );
            Ok(true)
        }
        Err(error) => Err(crate::error::NetworkError::Unknown(error.to_string())),
    }
}

/// Settings key holding the last successfully applied global checkpoint.
const CURSOR_KEY: &str = "stream:global_events:cursor:v1";

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Minimum gap between durable catch-up syncs, so a flapping connection
/// cannot turn the reconnect loop into a sync storm.
const MIN_CATCHUP_INTERVAL: Duration = Duration::from_secs(15);

/// True when the server refused the stream because this user is in no
/// channels — the one "error" that is really just an empty account.
/// How long to wait after the server answers "you are in no channels".
///
/// Only sleep out the full backoff when nobody asked us to be here. A nudge means
/// someone just joined or accepted a channel, and the server's answer can still be
/// "no channels" for a moment — the membership write that prompted the nudge is
/// not necessarily visible yet to the read this stream does. Jumping to
/// MAX_BACKOFF on that answer cost the accept path up to 30 seconds of silence,
/// because the nudge is single-shot: nothing arrives to cut the wait short a
/// second time. That is what `v55` caught whenever the machine was loaded enough
/// to lose the race (OPEN-ITEMS §103).
fn backoff_after_no_channels(current: Duration, nudged: bool) -> Duration {
    if nudged { current } else { MAX_BACKOFF }
}

fn is_no_channels(e: &crate::error::NetworkError) -> bool {
    matches!(e, crate::error::NetworkError::InvalidRequest(msg)
        if msg.contains("no channels") || msg.contains("channel_ids must be provided"))
}

/// Parse strictly before applying or acknowledging a global checkpoint.
#[cfg(test)]
thread_local! { static CURSOR_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn validate_global_cursor(raw: &str) -> Result<(), crate::error::NetworkError> {
    #[cfg(test)]
    CURSOR_VALIDATIONS.with(|count| count.set(count.get() + 1));
    use serde::de::{Error, MapAccess, Visitor};
    struct Positions;
    impl<'de> Visitor<'de> for Positions {
        type Value = std::collections::BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded channel positions")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut positions = std::collections::BTreeMap::new();
            while let Some((channel, id)) = map.next_entry::<String, String>()? {
                let canonical = channel
                    .parse::<uuid::Uuid>()
                    .map_err(M::Error::custom)?
                    .to_string();
                let parts: Vec<_> = id.split('-').collect();
                if canonical != channel
                    || parts.len() != 2
                    || parts.iter().any(|p| {
                        p.parse::<u64>()
                            .map(|n| n.to_string() != *p)
                            .unwrap_or(true)
                    })
                    || positions.contains_key(&channel)
                    || positions.len() >= 500
                {
                    return Err(M::Error::custom("invalid channel position"));
                }
                positions.insert(channel, id);
            }
            Ok(positions)
        }
    }
    if raw.len() > 45003 || !raw.starts_with("v1:") {
        return Err(crate::error::NetworkError::DataLoss(
            "invalid global cursor version or size".into(),
        ));
    }
    let mut decoder = serde_json::Deserializer::from_str(&raw[3..]);
    serde::Deserializer::deserialize_map(&mut decoder, Positions)
        .and_then(|_| decoder.end())
        .map_err(|e| crate::error::NetworkError::DataLoss(e.to_string()))
}

/// Proof that a wire checkpoint passed strict admission. Only this constructor
/// can create candidates; coalesced flushes never parse the same cursor again.
#[derive(Debug, Clone)]
struct ValidatedGlobalCursor(String);
impl ValidatedGlobalCursor {
    fn parse(raw: String) -> Result<Self, crate::error::NetworkError> {
        validate_global_cursor(&raw)?;
        Ok(Self(raw))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Applied {
    Complete,
    Pending,
}

fn require_ingested(outcome: Ingested) -> Applied {
    match outcome {
        Ingested::Stored | Ingested::OwnMessage | Ingested::Dropped | Ingested::DeliveryIssue => {
            Applied::Complete
        }
        Ingested::RetryableFailure | Ingested::DeferredPendingSenderKey => Applied::Pending,
    }
}

const RECOVERY_BUDGET: Duration = Duration::from_secs(2);
const RECOVERY_INTERVAL: Duration = Duration::from_secs(5);

/// Session-local gaps hold the entire checkpoint, including unrelated positions.
/// Reconnecting loses only this retry plan; the durable old checkpoint replays it.
#[derive(Default)]
struct RecoveryState {
    session: Option<crate::state::receive_session::ReceiveSession>,
    checkpoint_events: usize,
    checkpoint_written_at: Option<std::time::Instant>,
    channels: std::collections::VecDeque<String>,
    candidate: Option<ValidatedGlobalCursor>,
    progress: std::collections::BTreeMap<String, RecoveryProgress>,
}

#[derive(Default)]
struct RecoveryProgress {
    page_token: Option<String>,
    seen_tokens: std::collections::BTreeSet<String>,
    incomplete: bool,
}

impl RecoveryProgress {
    fn finish_page(
        &mut self,
        next_page_token: String,
    ) -> Result<Applied, crate::error::NetworkError> {
        if next_page_token.is_empty() {
            let applied = if self.incomplete {
                Applied::Pending
            } else {
                Applied::Complete
            };
            // Restart incomplete truth on the next traversal. Never remember a
            // failed row as applied, even after later pages succeeded.
            *self = Self::default();
            return Ok(applied);
        }
        if !self.seen_tokens.insert(next_page_token.clone()) {
            return Err(crate::error::NetworkError::DataLoss(
                "repeated resync page token".into(),
            ));
        }
        self.page_token = Some(next_page_token);
        Ok(Applied::Pending)
    }
}

impl RecoveryState {
    fn defer(&mut self, channel: &str) -> Result<(), crate::error::NetworkError> {
        if !self.channels.iter().any(|existing| existing == channel) {
            if self.channels.len() >= 500 {
                return Err(crate::error::NetworkError::DataLoss(
                    "too many recovery channels".into(),
                ));
            }
            self.channels.push_back(channel.to_owned());
        }
        self.progress
            .insert(channel.to_owned(), RecoveryProgress::default());
        Ok(())
    }

    fn acknowledgeable(&self) -> Option<&str> {
        if self.channels.is_empty() {
            self.candidate.as_ref().map(|cursor| cursor.0.as_str())
        } else {
            None
        }
    }
}

/// Live message-event stream for every server-managed channel the user is in.
pub struct ChannelStreamService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    inbound: Arc<ChannelInbound>,
    sync_service: Arc<SyncService>,
    messages: Arc<MessageService>,
    event_bus: EventBus,
    /// Nudged when the set of channels this user is in changed locally
    /// (create/join), so the stream reconnects and the server re-runs its
    /// membership-based auto-subscribe. The server only publishes membership
    /// "left" events (kick/ban), never "joined", so a client-side nudge is the
    /// only thing that makes a freshly joined channel live in this session.
    resubscribe: tokio::sync::Notify,
}

impl ChannelStreamService {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        inbound: Arc<ChannelInbound>,
        sync_service: Arc<SyncService>,
        messages: Arc<MessageService>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            inbound,
            sync_service,
            messages,
            event_bus,
            resubscribe: tokio::sync::Notify::new(),
        }
    }

    /// Ask the stream to reconnect, picking up channels joined since it
    /// started. Cheap and idempotent: a reconnect in flight absorbs it.
    pub fn request_resubscribe(&self) {
        self.resubscribe.notify_one();
    }

    /// Start the stream task. Runs until the process ends or the task is
    /// aborted; every error path reconnects.
    ///
    /// Called from `ChatterClient::start`, which every authenticated entry
    /// path funnels through. If this loses its caller, channel messages stop
    /// arriving live again — `tests/production_wiring.rs` pins it.
    pub fn start_channel_message_stream(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut backoff = INITIAL_BACKOFF;
            // Set when a resubscribe nudge woke us, cleared once a stream has
            // actually run. See the `is_no_channels` arm for why it matters.
            let mut nudged = false;
            loop {
                let mut connected_at = None;
                let outcome = self.run_once(&mut connected_at).await;
                // A stream that had been up for a while and then dropped (the
                // server closed it, a network change, a missed heartbeat) is
                // the ordinary case, not a failure streak: reconnect on the
                // shortest backoff. Without this the ladder kept climbing across
                // healthy streams, and every ~5 min server cut cost 30 s of
                // silence (2026-10-03, "backoff_secs=30").
                if connected_at.is_some_and(|t: std::time::Instant| {
                    t.elapsed() >= heartbeat::HEALTHY_STREAM_AFTER
                }) {
                    backoff = INITIAL_BACKOFF;
                }
                match outcome {
                    Ok(()) => {
                        tracing::info!("Channel message stream ended; reconnecting");
                        backoff = INITIAL_BACKOFF;
                        nudged = false;
                    }
                    Err(e) if is_no_channels(&e) => {
                        // The server refuses a global stream for a user who is
                        // in no channels. That is a normal state for a fresh
                        // account, not an error to shout about every second —
                        // wait out the full backoff quietly and try again
                        // (joining a channel nudges us awake immediately).
                        // Only sleep out the full backoff when nobody asked us
                        // to be here. A nudge means someone just joined or
                        // accepted a channel, and the server's answer can still
                        // be "no channels" for a moment — the membership write
                        // that prompted the nudge is not necessarily visible to
                        // the read this stream does. Jumping to MAX_BACKOFF on
                        // that answer made the accept path cost up to 30 seconds
                        // of silence, which is what `v55` was catching whenever
                        // the machine was loaded enough to lose that race
                        // (OPEN-ITEMS §103). The nudge is single-shot: nothing
                        // arrives to cut the wait short a second time.
                        backoff = backoff_after_no_channels(backoff, nudged);
                        tracing::debug!(
                            nudged,
                            backoff_secs = backoff.as_secs(),
                            "Server reports no channels; will retry"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            backoff_secs = backoff.as_secs(),
                            "Channel message stream error; reconnecting"
                        );
                    }
                }

                // A resubscribe nudge cuts the wait short: joining a channel
                // should start delivering now, not after the backoff.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = self.resubscribe.notified() => {
                        backoff = INITIAL_BACKOFF;
                        nudged = true;
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        })
    }

    /// One connect → consume → disconnect cycle. `connected_at` is set once
    /// the subscription is established.
    #[tracing::instrument(skip_all)]
    async fn run_once(
        &self,
        connected_at: &mut Option<std::time::Instant>,
    ) -> Result<(), crate::error::NetworkError> {
        let session = self.messages.receive_session().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        let connect_started = std::time::Instant::now();
        let cursor = self.cursor().await?;
        let mut client = self.grpc_pool.message_client();
        // Empty channel_ids => the server subscribes us to every non-archived
        // channel we are a member of, including ones joined in a previous
        // session. That is the property the old per-channel subscriptions
        // lacked.
        let stream = match client
            .stream_global_events(Vec::new(), cursor.as_deref())
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                // Opportunistic durable sync while we cannot connect: if the
                // stream is broken for a while, this is what keeps messages
                // arriving at all. Only on failure — a successful connect runs
                // the forced catch-up below, and running this first as well
                // made every start sync twice back to back.
                self.catch_up().await;
                return Err(e);
            }
        };

        *connected_at = Some(std::time::Instant::now());
        tracing::info!(
            resume_from = cursor.as_deref().unwrap_or("<none>"),
            subscribe_ms = connect_started.elapsed().as_millis() as u64,
            "Channel message stream connected"
        );

        // SUBSCRIBE FIRST, THEN BACKFILL. This ordering is the whole fix for
        // OPEN-ITEMS §103, and the reverse of what this function used to do.
        //
        // A cursorless global stream is positioned by the server at each
        // channel's CURRENT TAIL (`newEventLogReader` -> `eventLogTail`), so it
        // delivers only what is appended after this call returns. Backfilling
        // before subscribing therefore leaves a window — everything published
        // between the sync and the subscribe is delivered by neither half:
        // too late for the backfill, too early for the stream. Nothing retries,
        // because a healthy stream never reconnects.
        //
        // That window is exactly what `v55` was catching. A brand-new invitee
        // has no stream at all (the server refuses a global stream for a user
        // in no channels), so accepting an invitation means bootstrapping one;
        // under load the sender's first message lands inside the window and is
        // never delivered. It looked load-sensitive because the window is
        // normally sub-millisecond.
        //
        // Forced, not rate-limited: MIN_CATCHUP_INTERVAL is 15s and a
        // reconnecting client has usually synced within that, so the throttled
        // call above would skip precisely the message pass that closes the window.
        // Duplicates are free — the message store inserts with
        // INSERT OR IGNORE on the message id.
        let catch_up_started = std::time::Instant::now();
        self.catch_up_now().await;
        tracing::info!(
            catch_up_ms = catch_up_started.elapsed().as_millis() as u64,
            "Channel message stream catch-up finished; consuming subscription"
        );

        tokio::pin!(stream);
        session.check().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        let mut recovery = RecoveryState { session: Some(session), ..Default::default() };
        let mut retry = tokio::time::interval(RECOVERY_INTERVAL);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_stream_item = tokio::time::Instant::now();
        loop {
            tokio::select! {
                item = heartbeat::next_or_silent(&mut stream, heartbeat::STREAM_SILENCE_LIMIT.saturating_sub(last_stream_item.elapsed())) => match item {
                    Next::Item(result) => {
                        last_stream_item = tokio::time::Instant::now();
                        tokio::select! {
                            result = self.handle_event(result?, &mut recovery) => result?,
                            _ = self.resubscribe.notified() => return Ok(()),
                        }
                    },
                    Next::Ended => return Ok(()),
                    Next::Silent => {
                        tracing::warn!("Channel message stream missed its heartbeats; reconnecting");
                        return Err(crate::error::NetworkError::Timeout);
                    }
                },
                _ = retry.tick() => {
                    if recovery.channels.is_empty() {
                        self.flush_checkpoint(&mut recovery, true).await?;
                    } else {
                        tokio::select! {
                            result = self.retry_recovery(&mut recovery) => result?,
                            _ = self.resubscribe.notified() => return Ok(()),
                        }
                    }
                }
                _ = self.resubscribe.notified() => {
                    tracing::info!("Channel membership changed; resubscribing global stream");
                    return Ok(());
                }
            }
        }
    }

    /// A failed subscribe leaves the normal full sync available as an
    /// opportunistic fallback, subject to its rate limiter.
    async fn catch_up(&self) {
        self.sync_service.sync_if_stale(MIN_CATCHUP_INTERVAL).await;
    }

    /// A durable message pass that always runs after subscribe.
    ///
    /// Used once per successful subscribe (§103). It waits for a full sync
    /// already in flight, then reads from each local channel's message cursor,
    /// so even a sync begun before subscribe cannot leave a stream gap.
    async fn catch_up_now(&self) {
        match self.sync_service.catch_up_messages().await {
            Ok(result) => tracing::info!(
                channels = result.channels_synced,
                messages = result.total_inserted,
                errors = result.errors.len(),
                "Channel stream catch-up complete"
            ),
            Err(error) => tracing::warn!(%error, "Channel stream catch-up could not list local memberships"),
        }
    }

    async fn cursor(&self) -> Result<Option<String>, crate::error::NetworkError> {
        let settings = self.settings_repo.clone();
        let cursor = self.messages.database.run_blocking(move || settings.get::<String>(CURSOR_KEY)).await
            .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        if let Some(raw) = &cursor {
            validate_global_cursor(raw)?;
        }
        Ok(cursor)
    }

    /// The server could not replay everything after our cursor: it was older
    /// than the event log retains, a channel's buffer was trimmed past it, or
    /// the stream's replay budget ran out. What it skipped is not coming on
    /// this stream, so fetch it from the durable store — the sync cycle covers
    /// the channel list, members and messages (OPEN-ITEMS §40/§56).
    ///
    /// Forced, not rate-limited, for the same reason as the connect-time
    /// catch-up: a throttle that skips this one leaves the gap open.
    async fn handle_resync(
        &self,
        new_cursor: ValidatedGlobalCursor,
        resync: &chat_v1::ResyncRequired,
        recovery: &mut RecoveryState,
    ) -> Result<(), crate::error::NetworkError> {
        if resync.channel_ids.len() > 500 {
            return Err(crate::error::NetworkError::DataLoss(
                "too many resync channels".into(),
            ));
        }
        tracing::info!(
            channels = resync.channel_ids.len(),
            new_cursor = %new_cursor.0,
            "Server could not replay from our cursor; resyncing from the durable store"
        );
        // Validate the whole plan before applying anything. A recoverable gap
        // holds the old checkpoint, but must not end an otherwise healthy stream.
        for channel in &resync.channel_ids {
            let canonical = channel
                .parse::<uuid::Uuid>()
                .map_err(|_| crate::error::NetworkError::DataLoss("invalid resync channel".into()))?
                .to_string();
            if canonical != *channel {
                return Err(crate::error::NetworkError::DataLoss(
                    "noncanonical resync channel".into(),
                ));
            }
        }
        for channel in &resync.channel_ids {
            recovery.defer(channel)?;
        }
        recovery.candidate = Some(new_cursor);
        recovery.checkpoint_events += 1;
        // Ancillary channel/member sync is useful, but mutation snapshots below
        // are the authority for message application and checkpoint completion.
        let _ = tokio::time::timeout(RECOVERY_BUDGET, self.catch_up_now()).await;
        self.acknowledge_recovery(recovery).await
    }

    async fn acknowledge_recovery(
        &self,
        recovery: &mut RecoveryState,
    ) -> Result<(), crate::error::NetworkError> {
        self.flush_checkpoint(recovery, false).await
    }

    async fn flush_checkpoint(
        &self,
        recovery: &mut RecoveryState,
        force: bool,
    ) -> Result<(), crate::error::NetworkError> {
        let Some(cursor) = recovery.acknowledgeable() else { return Ok(()) };
        if recovery.checkpoint_events == 0 { return Ok(()) }
        if !force && recovery.checkpoint_events < 32
            && recovery.checkpoint_written_at.is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return Ok(());
        }
        let cursor = cursor.to_owned();
        let session = match &recovery.session {
            Some(session) => session.clone(),
            None => self.messages.receive_session().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?,
        };
        let completion_session = session.clone();
        let settings = self.settings_repo.clone();
        self.messages.database.run_blocking(move || settings.set_received(CURSOR_KEY, &cursor, &session)).await
            .map_err(|error| crate::error::NetworkError::Unknown(error.to_string()))?;
        completion_session.check().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        recovery.checkpoint_events = 0;
        recovery.checkpoint_written_at = Some(std::time::Instant::now());
        Ok(())
    }

    async fn retry_recovery(
        &self,
        recovery: &mut RecoveryState,
    ) -> Result<(), crate::error::NetworkError> {
        // One channel per tick, rotating even when an old sender key never arrives.
        let channel = recovery
            .channels
            .front()
            .cloned()
            .expect("pending recovery");
        let session = recovery.session.clone().ok_or_else(|| crate::error::NetworkError::DataLoss("Missing recovery session".into()))?;
        session.check().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        let outcome = tokio::time::timeout(
            RECOVERY_BUDGET,
            self.recover_channel_changes(
                &session,
                &channel,
                recovery
                    .progress
                    .get_mut(&channel)
                    .expect("recovery progress"),
            ),
        )
        .await;
        match outcome {
            Ok(Ok(Applied::Complete)) => {
                recovery.channels.pop_front();
                recovery.progress.remove(&channel);
            }
            Ok(Ok(Applied::Pending)) | Err(_) => {
                recovery.channels.rotate_left(1);
            }
            Ok(Err(
                crate::error::NetworkError::ConnectionFailed(_)
                | crate::error::NetworkError::Timeout
                | crate::error::NetworkError::Unavailable
                | crate::error::NetworkError::RateLimited,
            )) => {
                recovery.channels.rotate_left(1);
            }
            Ok(Err(error)) => return Err(error),
        }
        self.acknowledge_recovery(recovery).await
    }

    /// Reconcile one channel from an authoritative retained mutation snapshot.
    /// Used for explicit resync and a reaction whose old target is not cached;
    /// new-message catch-up alone cannot recover an old edited/reaction target.
    async fn recover_channel_changes(
        &self,
        session: &crate::state::receive_session::ReceiveSession,
        channel: &str,
        progress: &mut RecoveryProgress,
    ) -> Result<Applied, crate::error::NetworkError> {
        let channel_id = channel.parse::<ChannelId>().map_err(|_| {
            crate::error::NetworkError::InvalidRequest("invalid resync channel".into())
        })?;
        let page = self
            .grpc_pool
            .message_client()
            .get_message_changes(channel, progress.page_token.as_deref())
            .await?;
        for message in &page.messages {
            if message.channel_id != channel {
                return Err(crate::error::NetworkError::DataLoss(
                    "resync channel mismatch".into(),
                ));
            }
            let result = if message.deleted_at != 0 {
                self.apply_delete(&message.id, channel_id, session.clone()).await
            } else {
                self.inbound.ingest_resynced_with_session(message, channel_id, session.clone()).await
            };
            // Rejected content is terminal for this row, not for the stream.
            // Storage and missing-key failures still hold the checkpoint.
            progress.incomplete |= !resync_row_complete(result, channel, &message.id)?;
        }
        // Advance pagination only after every row in this page was considered.
        // Cancellation retries this page; complete pages survive bounded ticks.
        progress.finish_page(page.next_page_token)
    }

    /// Dispatch one `MessageEvent`.
    ///
    /// INFRA-4: the oneof has five arms and the server constructs all five.
    /// The old consumer matched `MessageSent` only, with no `else` and no log,
    /// so edits, deletions and reactions from other members were received and
    /// dropped on the floor. This match is exhaustive.
    async fn handle_event(
        &self,
        event: chat_v1::MessageEvent,
        recovery: &mut RecoveryState,
    ) -> Result<(), crate::error::NetworkError> {
        // The server's keepalive (also its first item, once subscribed). Not
        // data and never a cursor, so it goes before anything else.
        if event.is_heartbeat() {
            tracing::trace!("Channel message stream heartbeat");
            return Ok(());
        }
        let cursor = ValidatedGlobalCursor::parse(event.event_id.clone())?;
        let session = match &recovery.session {
            Some(session) => session.clone(),
            None => {
                let session = self.messages.receive_session().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
                recovery.session = Some(session.clone());
                session
            }
        };
        session.check().map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        // Before anything that reads channel_id: a resync covers several
        // channels and carries none of its own.
        if let Some(MsgEventType::ResyncRequired(resync)) = &event.event {
            self.handle_resync(cursor, resync, recovery)
                .await?;
            return Ok(());
        }

        let channel_id = match event.channel_id.parse::<uuid::Uuid>() {
            Ok(id) => ChannelId::from(id),
            Err(_) => {
                // message_deleted events and reaction events still carry a
                // channel_id; without one we cannot route anything.
                tracing::warn!(
                    channel_id = %event.channel_id,
                    "Message event with unparseable channel_id; dropping"
                );
                return Err(crate::error::NetworkError::DataLoss(
                    "invalid event channel".into(),
                ));
            }
        };

        let mut applied = Applied::Complete;
        match event.event {
            Some(MsgEventType::MessageSent(msg)) => {
                if msg.channel_id != event.channel_id {
                    return Err(crate::error::NetworkError::DataLoss(
                        "message channel mismatch".into(),
                    ));
                }
                applied = require_ingested(self.inbound.ingest_with_session(&msg, channel_id, true, crate::services::message::channel_inbound::PresenceHint::KnownPresent, session.clone()).await);
            }
            Some(MsgEventType::MessageUpdated(msg)) => {
                if msg.channel_id != event.channel_id {
                    return Err(crate::error::NetworkError::DataLoss(
                        "edit channel mismatch".into(),
                    ));
                }
                applied = require_ingested(self.inbound.ingest_edit_with_session(&msg, channel_id, session.clone()).await);
            }
            Some(MsgEventType::MessageDeletedId(id)) => match self.apply_delete(&id, channel_id, session.clone()).await {
                Ok(()) => {}
                Err(crate::services::message::MessageError::Storage(_)) => {
                    applied = Applied::Pending
                }
                Err(crate::services::message::MessageError::InvalidContent(error)) => {
                    return Err(crate::error::NetworkError::DataLoss(error));
                }
                Err(error) => return Err(crate::error::NetworkError::Unknown(error.to_string())),
            },
            Some(MsgEventType::ReactionAdded(update)) => {
                applied = self
                    .handle_reaction(update, channel_id, false, event.timestamp, session.clone())
                    .await?;
            }
            Some(MsgEventType::ReactionRemoved(update)) => {
                applied = self
                    .handle_reaction(update, channel_id, true, event.timestamp, session.clone())
                    .await?;
            }
            // Handled above, before the channel_id parse.
            Some(MsgEventType::ResyncRequired(_)) | Some(MsgEventType::Heartbeat(_)) => {}
            None => {
                tracing::warn!(
                    %channel_id,
                    event_id = %event.event_id,
                    "Message event with no payload arm"
                );
                return Err(crate::error::NetworkError::DataLoss(
                    "missing event payload".into(),
                ));
            }
        }
        if applied == Applied::Pending {
            recovery.defer(&event.channel_id)?;
        }
        recovery.candidate = Some(cursor);
        recovery.checkpoint_events += 1;
        self.acknowledge_recovery(recovery).await
    }

    async fn handle_reaction(
        &self,
        update: chat_v1::ReactionUpdate,
        channel_id: ChannelId,
        remove: bool,
        timestamp: u64,
        session: crate::state::receive_session::ReceiveSession,
    ) -> Result<Applied, crate::error::NetworkError> {
        let messages = self.messages.clone();
        let event_bus = self.event_bus.clone();
        self.messages.database.run_blocking(move || {
            session.check()?;
            Ok(Self::apply_reaction_unit(&messages, &event_bus, &session, update, channel_id, remove, timestamp))
        }).await.unwrap_or(Ok(Applied::Pending))
    }

    fn apply_reaction_unit(
        messages: &MessageService,
        event_bus: &EventBus,
        session: &crate::state::receive_session::ReceiveSession,
        update: chat_v1::ReactionUpdate,
        channel_id: ChannelId,
        remove: bool,
        timestamp: u64,
    ) -> Result<Applied, crate::error::NetworkError> {
        let (Ok(sender_id), Ok(reactor_id)) =
            (update.target_author_aci.parse(), update.reactor_id.parse())
        else {
            tracing::warn!(%channel_id, "Reaction event has invalid author or reactor identity");
            return Err(crate::error::NetworkError::DataLoss(
                "invalid reaction identity".into(),
            ));
        };
        // A stream must never mutate a target in another conversation.
        let scope = match messages.channel_reaction_scope(channel_id, &sender_id, update.target_sent_timestamp) {
            Ok(scope) => scope,
            Err(crate::services::message::MessageError::Storage(_)) => return Ok(Applied::Pending),
            Err(crate::services::message::MessageError::InvalidContent(error)) => return Err(crate::error::NetworkError::DataLoss(error)),
            Err(error) => return Err(crate::error::NetworkError::Unknown(error.to_string())),
        };
        match messages.message_repo.get_by_reaction_scope(
            &scope,
            &sender_id,
            update.target_sent_timestamp,
        ) {
            Ok(Some(message)) if message.conversation_id == channel_id => {}
            Ok(None) => {
                // A recent reaction can refer to a message older than the
                // normal message catch-up cursor. Reconnecting from the same
                // event forever would block every channel without fetching it.
                // Recovery includes the complete current reaction snapshot;
                // do not reapply this older delta over that newer snapshot.
                // Schedule authoritative recovery without applying the stale delta.
                return Ok(Applied::Pending);
            }
            Ok(Some(_)) => {
                tracing::warn!(%channel_id, "Rejected reaction targeting a different channel");
                return Err(crate::error::NetworkError::DataLoss(
                    "reaction channel mismatch".into(),
                ));
            }
            Err(_) => return Ok(Applied::Pending),
        }
        match messages.record_reaction_received(
            session,
            &scope,
            &sender_id,
            update.target_sent_timestamp,
            &reactor_id,
            &update.emoji,
            remove,
        ) {
            Ok(true) => {
                if session.check().is_err() { return Ok(Applied::Pending); }
                let event = if remove {
                    DomainEvent::ReactionRemoved {
                        scope: scope.clone(),
                        sender_id,
                        sent_at_ns: update.target_sent_timestamp,
                        reactor_id,
                        emoji: update.emoji,
                        timestamp: timestamp / 1_000_000,
                    }
                } else {
                    DomainEvent::ReactionAdded {
                        scope: scope.clone(),
                        sender_id,
                        sent_at_ns: update.target_sent_timestamp,
                        reactor_id,
                        emoji: update.emoji,
                        timestamp: timestamp / 1_000_000,
                    }
                };
                event_bus.emit_domain(event);
            }
            Ok(false) => {}
            Err(crate::services::message::MessageError::Storage(_)) => return Ok(Applied::Pending),
            Err(error) => return Err(crate::error::NetworkError::Unknown(error.to_string())),
        }
        Ok(Applied::Complete)
    }

    async fn apply_delete(
        &self,
        message_id: &str,
        channel_id: ChannelId,
        session: crate::state::receive_session::ReceiveSession,
    ) -> Result<(), crate::services::message::MessageError> {
        let messages = self.messages.clone();
        let event_bus = self.event_bus.clone();
        let message_id = message_id.to_owned();
        self.messages.database.run_blocking(move || {
            session.check()?;
            Ok(Self::apply_delete_unit(&messages, &event_bus, &session, &message_id, channel_id))
        }).await?
    }

    fn apply_delete_unit(
        messages: &MessageService,
        event_bus: &EventBus,
        session: &crate::state::receive_session::ReceiveSession,
        message_id: &str,
        channel_id: ChannelId,
    ) -> Result<(), crate::services::message::MessageError> {
        let id = message_id.parse::<MessageId>().map_err(|_| {
            crate::services::message::MessageError::InvalidContent(
                "invalid deleted message id".into(),
            )
        })?;
        let existing = match messages.message_repo.delete_received(&id, channel_id, session) {
            Ok(existing) => existing,
            Err(crate::error::StorageError::NotFound(_)) => None,
            Err(crate::error::StorageError::InvalidData(error)) if error == "delete channel mismatch" => {
                return Err(crate::services::message::MessageError::InvalidContent(error));
            }
            Err(error) => return Err(error.into()),
        };
        session.check()?;
        if let Some(existing) = existing {
            if let Some(author_id) = existing.sender_id {
                event_bus.emit_domain(DomainEvent::MessageDeleted {
                    message_id: id,
                    author_id,
                    channel_id: Some(channel_id),
                    target_sent_timestamp: existing.sent_at,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn rejected_resync_rows_do_not_hold_recovery_or_hide_later_pages() {
        let mut progress = RecoveryProgress::default();
        progress.incomplete |= !resync_row_complete(
            Err(crate::services::message::MessageError::InvalidContent(
                "resync message identity mismatch".into(),
            )),
            "channel",
            "bad-row",
        )
        .unwrap();
        assert_eq!(
            progress.finish_page("next-page".into()).unwrap(),
            Applied::Pending
        );
        progress.incomplete |= !resync_row_complete(Ok(()), "channel", "good-row").unwrap();
        assert_eq!(
            progress.finish_page(String::new()).unwrap(),
            Applied::Complete
        );
    }

    #[test]
    fn an_unprompted_no_channels_answer_waits_the_full_backoff() {
        // A fresh account that has joined nothing should ask again rarely.
        assert_eq!(
            backoff_after_no_channels(INITIAL_BACKOFF, false),
            MAX_BACKOFF
        );
        assert_eq!(backoff_after_no_channels(MAX_BACKOFF, false), MAX_BACKOFF);
    }

    #[test]
    fn a_nudged_no_channels_answer_stays_on_the_short_ladder() {
        // §103: accepting an invitation nudges this loop, and the server can
        // still answer "no channels" if the membership write is not visible to
        // that read yet. Sleeping 30s there is the user watching an empty
        // channel after accepting.
        assert_eq!(
            backoff_after_no_channels(INITIAL_BACKOFF, true),
            INITIAL_BACKOFF
        );
        assert_eq!(
            backoff_after_no_channels(Duration::from_secs(2), true),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn the_nudged_ladder_still_converges_on_the_ceiling() {
        // It must not retry at 1s forever if the accept really did fail.
        let mut backoff = INITIAL_BACKOFF;
        for _ in 0..10 {
            backoff = backoff_after_no_channels(backoff, true);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        assert_eq!(backoff, MAX_BACKOFF);
    }
}

/// §103 — the subscribe/backfill ORDER is the fix, and order is invisible to the
/// type system.
///
/// A cursorless global stream is positioned by the server at each channel's
/// current tail, so it carries only what is appended after the subscribe call
/// returns. Backfilling first therefore leaves a window that neither half
/// covers, and nothing retries it, because a healthy stream never reconnects.
/// `v55` spent three full-suite runs failing on exactly that window.
///
/// This reads the source rather than the behaviour, which is a weak test in
/// general and the right one here: the property is "these two calls are in this
/// order inside one function", there is no seam to inject a fake gRPC stream
/// into, and a refactor that swaps them would compile, pass every other test,
/// and quietly restore the bug.
#[cfg(test)]
mod subscribe_before_backfill {
    #[test]
    fn the_forced_catch_up_runs_after_the_subscribe() {
        let src = include_str!("channel_stream.rs");
        let body = src
            .split_once("async fn run_once")
            .expect("run_once still exists")
            .1;
        let body = body
            .split_once("\n    fn cursor(")
            .map(|(b, _)| b)
            .unwrap_or(body);

        let subscribe = body
            .find(".stream_global_events(")
            .expect("run_once must still open the global stream");
        let backfill = body
            .find("self.catch_up_now().await")
            .expect("run_once must still force a durable catch-up");

        assert!(
            subscribe < backfill,
            "the forced catch-up must come AFTER stream_global_events. Backfilling first \
             leaves every event published in between undelivered by both paths — the server \
             positions a cursorless reader at the channel tail, so the stream will not carry \
             them, and the backfill already ran. See OPEN-ITEMS §103."
        );
    }

    #[test]
    fn the_window_closing_catch_up_is_not_rate_limited() {
        let src = include_str!("channel_stream.rs");
        let forced = src
            .split_once("async fn catch_up_now")
            .expect("catch_up_now still exists")
            .1;
        let forced = forced
            .split_once("\n    fn ")
            .map(|(b, _)| b)
            .unwrap_or(forced);

        assert!(
            !forced.contains("MIN_CATCHUP_INTERVAL"),
            "catch_up_now must ignore the rate limiter: a reconnecting client has usually \
             synced within MIN_CATCHUP_INTERVAL, so throttling it would skip precisely the \
             sync that closes the subscribe window (§103)."
        );
    }
}

/// The heartbeat is the first thing `handle_event` looks at: it carries no
/// event_id and no channel_id, so letting it past would store an empty cursor
/// attempt and log every minute as an "unparseable channel_id" drop.
#[cfg(test)]
mod heartbeat_dispatch {
    #[test]
    fn a_heartbeat_returns_before_the_cursor_and_the_channel_parse() {
        let src = include_str!("channel_stream.rs");
        let body = src
            .split_once("async fn handle_event")
            .expect("handle_event still exists")
            .1;
        let body = body.split_once("\n    fn ").map(|(b, _)| b).unwrap_or(body);
        let beat = body
            .find("event.is_heartbeat()")
            .expect("handle_event must drop heartbeats");
        let cursor = body
            .find("self.acknowledge_recovery(")
            .expect("handle_event still acknowledges applied cursors");
        let parse = body
            .find("event.channel_id.parse")
            .expect("handle_event still parses channel_id");
        assert!(beat < cursor && beat < parse);
    }

    #[test]
    fn a_heartbeat_is_never_mistaken_for_an_event() {
        use crate::network::streaming::heartbeat::IsHeartbeat;
        use crate::proto::chat_v1::{MessageEvent, StreamHeartbeat, message_event::Event};
        let beat = MessageEvent {
            event: Some(Event::Heartbeat(StreamHeartbeat {})),
            ..Default::default()
        };
        assert!(beat.is_heartbeat());
        assert!(
            beat.event_id.is_empty(),
            "the server sends no cursor on a heartbeat"
        );
    }
}

/// §40/§56 — a `ResyncRequired` event names its channels in the payload and
/// leaves `MessageEvent.channel_id` empty, so it has to be dispatched before
/// the channel_id parse that drops unroutable events, and it has to end in a
/// durable sync that the rate limiter cannot skip. Same reasoning as
/// `subscribe_before_backfill` for reading the source: both properties are
/// about order and call choice inside one function.
#[cfg(test)]
mod resync_required {
    #[test]
    fn a_resync_is_handled_before_the_channel_id_parse() {
        let src = include_str!("channel_stream.rs");
        let body = src
            .split_once("async fn handle_event")
            .expect("handle_event still exists")
            .1;
        let body = body.split_once("\n    fn ").map(|(b, _)| b).unwrap_or(body);

        let resync = body
            .find("self.handle_resync(")
            .expect("handle_event must dispatch ResyncRequired");
        let parse = body
            .find("event.channel_id.parse")
            .expect("handle_event still parses channel_id");
        assert!(
            resync < parse,
            "ResyncRequired carries no channel_id; dispatching it after the parse drops it \
             as unroutable and the gap it announces is never backfilled."
        );
    }

    #[test]
    fn a_resync_forces_the_durable_catch_up() {
        let src = include_str!("channel_stream.rs");
        let body = src
            .split_once("async fn handle_resync")
            .expect("handle_resync still exists")
            .1;
        let body = body
            .split_once("\n    /// Dispatch one")
            .map(|(b, _)| b)
            .unwrap_or(body);

        assert!(
            body.contains("self.catch_up_now()"),
            "a resync must run the forced catch-up: the skipped events are not coming on \
             the stream, and the rate-limited catch_up() can skip the sync that fetches them."
        );
    }
}

#[cfg(test)]
mod global_checkpoints {
    use super::*;

    #[test]
    fn checkpoint_validation_rejects_duplicate_and_unbounded_positions() {
        let channel = "11111111-1111-1111-1111-111111111111";
        assert!(validate_global_cursor(&format!("v1:{{\"{channel}\":\"12-0\"}}")).is_ok());
        for invalid in [
            "12-0".into(),
            "v2:{}".into(),
            "v1:[]".into(),
            format!("v1:{{\"{channel}\":\"12-0\",\"{channel}\":\"13-0\"}}"),
            format!("v1:{{\"{channel}\":\"18446744073709551616-0\"}}"),
            "v1:{} false".into(),
            "x".repeat(45004),
        ] {
            assert!(validate_global_cursor(&invalid).is_err());
        }
    }

    #[test]
    fn failures_and_memory_only_deferred_messages_are_not_acknowledged() {
        assert!(require_ingested(Ingested::RetryableFailure) == Applied::Pending);
        assert!(require_ingested(Ingested::DeferredPendingSenderKey) == Applied::Pending);
        assert!(require_ingested(Ingested::Stored) == Applied::Complete);
        assert!(
            require_ingested(Ingested::Dropped) == Applied::Complete,
            "terminal rejection must not poison the stream"
        );
        assert!(require_ingested(Ingested::DeliveryIssue) == Applied::Complete);
    }

    #[test]
    fn an_unresolved_channel_holds_later_unrelated_checkpoint_positions() {
        let a = "11111111-1111-1111-1111-111111111111";
        let b = "22222222-2222-2222-2222-222222222222";
        let mut state = RecoveryState::default();
        state.defer(a).unwrap();
        state.candidate = Some(ValidatedGlobalCursor::parse(format!("v1:{{\"{a}\":\"12-0\",\"{b}\":\"50-0\"}}")).unwrap());
        assert_eq!(state.acknowledgeable(), None);
        state.channels.rotate_left(1);
        assert_eq!(
            state.acknowledgeable(),
            None,
            "a retry is not an acknowledgement"
        );
        state.channels.pop_front();
        assert_eq!(state.acknowledgeable(), state.candidate.as_ref().map(|cursor| cursor.0.as_str()));
    }

    #[test]
    fn retry_rotation_is_fair_and_a_new_gap_restarts_partial_truth() {
        let mut state = RecoveryState::default();
        state.defer("a").unwrap();
        state.defer("b").unwrap();
        let progress = state.progress.get_mut("a").unwrap();
        progress.page_token = Some("page-two".into());
        progress.incomplete = true;
        state.channels.rotate_left(1);
        assert_eq!(state.channels.front().unwrap(), "b");
        state.defer("a").unwrap();
        assert_eq!(state.channels.len(), 2, "no duplicate recovery work");
        assert!(state.progress["a"].page_token.is_none());
        assert!(!state.progress["a"].incomplete);
    }

    #[test]
    fn successful_later_pages_do_not_acknowledge_an_earlier_failed_row() {
        let mut progress = RecoveryProgress::default();
        progress.incomplete = true;
        assert_eq!(
            progress.finish_page("page-two".into()).unwrap(),
            Applied::Pending
        );
        assert_eq!(
            progress.finish_page(String::new()).unwrap(),
            Applied::Pending
        );
        assert!(
            progress.page_token.is_none(),
            "retry starts from authoritative truth"
        );
        assert_eq!(
            progress.finish_page(String::new()).unwrap(),
            Applied::Complete
        );
    }

    #[test]
    fn repeated_page_tokens_are_validation_failures_not_pending_ingestion() {
        let mut progress = RecoveryProgress::default();
        progress.finish_page("page-two".into()).unwrap();
        assert!(matches!(
            progress.finish_page("page-two".into()),
            Err(crate::error::NetworkError::DataLoss(_))
        ));
    }

    #[test]
    fn acknowledgement_follows_event_application() {
        let source = include_str!("channel_stream.rs");
        let body = source
            .split_once("async fn handle_event")
            .unwrap()
            .1
            .split_once("fn handle_reaction")
            .unwrap()
            .0;
        assert!(
            body.find("self.acknowledge_recovery(").unwrap()
                > body.find("match event.event").unwrap()
        );
        assert!(body.contains("require_ingested(self.inbound.ingest"));
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact
                .contains("self.handle_reaction(update,channel_id,false,event.timestamp,session.clone()).await?")
        );
    }
}

#[cfg(test)]
#[path = "channel_stream_admission_tests.rs"]
mod admission_tests;
