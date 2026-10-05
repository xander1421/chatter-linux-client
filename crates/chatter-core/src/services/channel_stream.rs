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
//! - a durable catch-up (`SyncService::trigger_sync`) on every connect, so
//!   correctness never depends on the live stream having been up, and
//! - a resume cursor (`event_id`) persisted across runs so a short drop
//!   replays from the server's event log instead of relying on the catch-up.
//!
//! The catch-up is the correctness path; the cursor is a latency
//! optimisation. The server's event log is a bounded replay buffer, and when
//! a cursor has fallen out of it (or replaying would exceed the server's
//! budget) the server says so with a `ResyncRequired` event instead of
//! replaying a partial history. That event means "what you missed is not
//! coming on this stream": we take the cursor it hands us and run the durable
//! sync again (OPEN-ITEMS §40/§56).

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
fn validate_global_cursor(raw: &str) -> Result<(), crate::error::NetworkError> {
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

fn require_ingested(outcome: Ingested) -> Result<(), crate::error::NetworkError> {
    match outcome {
        Ingested::Stored | Ingested::OwnMessage | Ingested::Dropped => Ok(()),
        Ingested::RetryableFailure | Ingested::DeferredPendingSenderKey => Err(
            crate::error::NetworkError::DataLoss("channel event not durably applied".into()),
        ),
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
    async fn run_once(
        &self,
        connected_at: &mut Option<std::time::Instant>,
    ) -> Result<(), crate::error::NetworkError> {
        let cursor = self.cursor()?;
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
        // call above would skip precisely the one sync that closes the window.
        // Duplicates are free — the message store inserts with
        // INSERT OR IGNORE on the message id.
        self.catch_up_now().await;

        tokio::pin!(stream);
        loop {
            tokio::select! {
                item = heartbeat::next_or_silent(&mut stream, heartbeat::STREAM_SILENCE_LIMIT) => match item {
                    Next::Item(result) => self.handle_event(result?).await?,
                    Next::Ended => return Ok(()),
                    Next::Silent => {
                        tracing::warn!("Channel message stream missed its heartbeats; reconnecting");
                        return Err(crate::error::NetworkError::Timeout);
                    }
                },
                _ = self.resubscribe.notified() => {
                    tracing::info!("Channel membership changed; resubscribing global stream");
                    return Ok(());
                }
            }
        }
    }

    /// Pull anything missed from the durable store (channel list, then
    /// messages per channel). Rate-limited so a flapping stream cannot spam
    /// the server: skipped when any sync cycle finished recently.
    async fn catch_up(&self) {
        self.sync_service.sync_if_stale(MIN_CATCHUP_INTERVAL).await;
    }

    /// A durable sync that always runs, whatever the rate limiter thinks.
    ///
    /// Used once per successful subscribe (§103). The throttle exists to stop a
    /// flapping stream hammering the server; it must not be able to skip the
    /// sync that closes the subscribe window. The engine coalesces it with a
    /// cycle already in flight, but only into one that STARTED after this call,
    /// so the window is still covered.
    async fn catch_up_now(&self) {
        if let Some(result) = self.sync_service.trigger_sync().await {
            tracing::info!(
                channels = result.channels.synced,
                messages = result.messages.total_inserted,
                "Channel stream catch-up complete"
            );
        }
    }

    fn cursor(&self) -> Result<Option<String>, crate::error::NetworkError> {
        let cursor = self
            .settings_repo
            .get::<String>(CURSOR_KEY)
            .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
        if let Some(raw) = &cursor {
            validate_global_cursor(raw)?;
        }
        Ok(cursor)
    }

    fn store_cursor(&self, event_id: &str) -> Result<(), crate::error::NetworkError> {
        validate_global_cursor(event_id)?;
        self.settings_repo
            .set(CURSOR_KEY, &event_id.to_string())
            .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))
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
        new_cursor: &str,
        resync: &chat_v1::ResyncRequired,
    ) -> Result<(), crate::error::NetworkError> {
        validate_global_cursor(new_cursor)?;
        if resync.channel_ids.len() > 500 {
            return Err(crate::error::NetworkError::DataLoss(
                "too many resync channels".into(),
            ));
        }
        tracing::info!(
            channels = resync.channel_ids.len(),
            new_cursor,
            "Server could not replay from our cursor; resyncing from the durable store"
        );
        // Complete recovery before adopting the replacement cursor. On failure
        // the stream reconnects from the old cursor and requests resync again.
        self.catch_up_now().await;
        for channel in &resync.channel_ids {
            self.recover_channel_changes(channel).await?;
        }
        self.store_cursor(new_cursor)?;
        Ok(())
    }

    /// Reconcile one channel from an authoritative retained mutation snapshot.
    /// Used for explicit resync and a reaction whose old target is not cached;
    /// new-message catch-up alone cannot recover an old edited/reaction target.
    async fn recover_channel_changes(
        &self,
        channel: &str,
    ) -> Result<(), crate::error::NetworkError> {
        let channel_id = channel.parse::<ChannelId>().map_err(|_| {
            crate::error::NetworkError::InvalidRequest("invalid resync channel".into())
        })?;
        let mut page_token: Option<String> = None;
        loop {
            let page = self
                .grpc_pool
                .message_client()
                .get_message_changes(channel, page_token.as_deref())
                .await?;
            for message in &page.messages {
                if message.channel_id != channel {
                    return Err(crate::error::NetworkError::DataLoss(
                        "resync channel mismatch".into(),
                    ));
                }
                if message.deleted_at != 0 {
                    self.apply_delete(&message.id, channel_id)
                        .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
                } else {
                    self.inbound
                        .ingest_resynced(message, channel_id)
                        .await
                        .map_err(|e| match e {
                            crate::services::message::MessageError::InvalidContent(e) => {
                                crate::error::NetworkError::DataLoss(e)
                            }
                            e => crate::error::NetworkError::Unknown(e.to_string()),
                        })?;
                }
            }
            if page.next_page_token.is_empty() {
                break;
            }
            page_token = Some(page.next_page_token);
        }
        Ok(())
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
    ) -> Result<(), crate::error::NetworkError> {
        // The server's keepalive (also its first item, once subscribed). Not
        // data and never a cursor, so it goes before anything else.
        if event.is_heartbeat() {
            tracing::trace!("Channel message stream heartbeat");
            return Ok(());
        }
        // Before anything that reads channel_id: a resync covers several
        // channels and carries none of its own.
        if let Some(MsgEventType::ResyncRequired(resync)) = &event.event {
            self.handle_resync(&event.event_id, resync).await?;
            return Ok(());
        }
        validate_global_cursor(&event.event_id)?;

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

        match event.event {
            Some(MsgEventType::MessageSent(msg)) => {
                if msg.channel_id != event.channel_id {
                    return Err(crate::error::NetworkError::DataLoss(
                        "message channel mismatch".into(),
                    ));
                }
                require_ingested(self.inbound.ingest(&msg, channel_id, true).await)?;
            }
            Some(MsgEventType::MessageUpdated(msg)) => {
                if msg.channel_id != event.channel_id {
                    return Err(crate::error::NetworkError::DataLoss(
                        "edit channel mismatch".into(),
                    ));
                }
                require_ingested(self.inbound.ingest_edit(&msg, channel_id).await)?;
            }
            Some(MsgEventType::MessageDeletedId(id)) => {
                self.apply_delete(&id, channel_id)
                    .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
            }
            Some(MsgEventType::ReactionAdded(update)) => {
                self.handle_reaction(update, channel_id, false, event.timestamp)
                    .await?;
            }
            Some(MsgEventType::ReactionRemoved(update)) => {
                self.handle_reaction(update, channel_id, true, event.timestamp)
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
        self.store_cursor(&event.event_id)
    }

    async fn handle_reaction(
        &self,
        update: chat_v1::ReactionUpdate,
        channel_id: ChannelId,
        remove: bool,
        timestamp: u64,
    ) -> Result<(), crate::error::NetworkError> {
        let (Ok(sender_id), Ok(reactor_id)) =
            (update.target_author_aci.parse(), update.reactor_id.parse())
        else {
            tracing::warn!(%channel_id, "Reaction event has invalid author or reactor identity");
            return Ok(());
        };
        // A stream must never mutate a target in another conversation.
        match self
            .messages
            .get_by_sender_timestamp(&sender_id, update.target_sent_timestamp)
        {
            Ok(Some(message)) if message.conversation_id == channel_id => {}
            Ok(None) => {
                // A recent reaction can refer to a message older than the
                // normal message catch-up cursor. Reconnecting from the same
                // event forever would block every channel without fetching it.
                // Recovery includes the complete current reaction snapshot;
                // do not reapply this older delta over that newer snapshot.
                self.recover_channel_changes(&channel_id.to_string())
                    .await?;
                return Ok(());
            }
            Ok(Some(_)) => {
                tracing::warn!(%channel_id, "Rejected reaction targeting a different channel");
                return Ok(());
            }
            Err(e) => return Err(crate::error::NetworkError::Unknown(e.to_string())),
        }
        match self.messages.record_reaction(
            &sender_id,
            update.target_sent_timestamp,
            &reactor_id,
            &update.emoji,
            remove,
        ) {
            Ok(true) => {
                let event = if remove {
                    DomainEvent::ReactionRemoved {
                        sender_id,
                        sent_at_ns: update.target_sent_timestamp,
                        reactor_id,
                        emoji: update.emoji,
                        timestamp: timestamp / 1_000_000,
                    }
                } else {
                    DomainEvent::ReactionAdded {
                        sender_id,
                        sent_at_ns: update.target_sent_timestamp,
                        reactor_id,
                        emoji: update.emoji,
                        timestamp: timestamp / 1_000_000,
                    }
                };
                self.event_bus.emit_domain(event);
            }
            Ok(false) => {}
            Err(error) => return Err(crate::error::NetworkError::Unknown(error.to_string())),
        }
        Ok(())
    }

    fn apply_delete(
        &self,
        message_id: &str,
        channel_id: ChannelId,
    ) -> Result<(), crate::services::message::MessageError> {
        let id = message_id.parse::<MessageId>().map_err(|_| {
            crate::services::message::MessageError::InvalidContent(
                "invalid deleted message id".into(),
            )
        })?;
        let existing = self.messages.get_message_local(&id)?;
        if existing
            .as_ref()
            .is_some_and(|m| m.conversation_id != channel_id)
        {
            return Err(crate::services::message::MessageError::InvalidContent(
                "delete channel mismatch".into(),
            ));
        }
        // Record even unseen tombstones: a delayed/deferred message must not
        // reappear after recovery told us it had been deleted.
        match self.messages.delete_message_local(&id) {
            Ok(())
            | Err(crate::services::message::MessageError::Storage(
                crate::error::StorageError::NotFound(_),
            )) => {}
            Err(error) => return Err(error),
        }
        if let Some(existing) = existing {
            if let Some(author_id) = existing.sender_id {
                self.event_bus.emit_domain(DomainEvent::MessageDeleted {
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
            .find("self.store_cursor(")
            .expect("handle_event still stores cursors");
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
            body.contains("self.catch_up_now().await"),
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
        assert!(require_ingested(Ingested::RetryableFailure).is_err());
        assert!(require_ingested(Ingested::DeferredPendingSenderKey).is_err());
        assert!(require_ingested(Ingested::Stored).is_ok());
        assert!(
            require_ingested(Ingested::Dropped).is_ok(),
            "terminal rejection must not poison the stream"
        );
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
        assert!(body.find("self.store_cursor(").unwrap() > body.find("match event.event").unwrap());
        assert!(body.contains("require_ingested(self.inbound.ingest"));
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact
                .contains("self.handle_reaction(update,channel_id,false,event.timestamp).await?")
        );
    }
}
