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

use tokio_stream::StreamExt;

use crate::domain::ids::{ChannelId, MessageId};
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{self, message_event::Event as MsgEventType};
use crate::services::message::channel_inbound::ChannelInbound;
use crate::services::message::MessageService;
use crate::services::sync_service::SyncService;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Settings key holding the last delivered `event_id` (a resume cursor).
const CURSOR_KEY: &str = "stream:global_events:cursor";

/// Sentinel `event_id` the server sends first to flush HTTP/2 headers.
/// Carries no payload and must never be treated as data or as a cursor.
const STREAM_READY_EVENT_ID: &str = "STREAM_READY";

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
                match self.run_once().await {
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

    /// One connect → consume → disconnect cycle.
    async fn run_once(&self) -> Result<(), crate::error::NetworkError> {
        let cursor = self.cursor();
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
                item = stream.next() => match item {
                    Some(result) => self.handle_event(result?).await,
                    None => return Ok(()),
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

    fn cursor(&self) -> Option<String> {
        self.settings_repo
            .get::<String>(CURSOR_KEY)
            .ok()
            .flatten()
            .filter(|c| !c.is_empty())
    }

    fn store_cursor(&self, event_id: &str) {
        if event_id.is_empty() || event_id == STREAM_READY_EVENT_ID {
            return;
        }
        if let Err(e) = self.settings_repo.set(CURSOR_KEY, &event_id.to_string()) {
            tracing::debug!(error = %e, "Failed to persist stream resume cursor");
        }
    }

    /// The server could not replay everything after our cursor: it was older
    /// than the event log retains, a channel's buffer was trimmed past it, or
    /// the stream's replay budget ran out. What it skipped is not coming on
    /// this stream, so fetch it from the durable store — the sync cycle covers
    /// the channel list, members and messages (OPEN-ITEMS §40/§56).
    ///
    /// Forced, not rate-limited, for the same reason as the connect-time
    /// catch-up: a throttle that skips this one leaves the gap open.
    async fn handle_resync(&self, new_cursor: &str, resync: &chat_v1::ResyncRequired) {
        tracing::info!(
            channels = resync.channel_ids.len(),
            new_cursor,
            "Server could not replay from our cursor; resyncing from the durable store"
        );
        if new_cursor.is_empty() {
            // Nothing is buffered to resume against. Keeping the old cursor
            // would present the same dead position on every reconnect and be
            // told to resync every time.
            if let Err(e) = self.settings_repo.delete(CURSOR_KEY) {
                tracing::debug!(error = %e, "Failed to clear stream resume cursor");
            }
        } else {
            self.store_cursor(new_cursor);
        }
        self.catch_up_now().await;
    }

    /// Dispatch one `MessageEvent`.
    ///
    /// INFRA-4: the oneof has five arms and the server constructs all five.
    /// The old consumer matched `MessageSent` only, with no `else` and no log,
    /// so edits, deletions and reactions from other members were received and
    /// dropped on the floor. This match is exhaustive.
    async fn handle_event(&self, event: chat_v1::MessageEvent) {
        if event.event_id == STREAM_READY_EVENT_ID {
            tracing::debug!("Channel message stream ready");
            return;
        }
        // Before anything that reads channel_id: a resync covers several
        // channels and carries none of its own.
        if let Some(MsgEventType::ResyncRequired(resync)) = &event.event {
            self.handle_resync(&event.event_id, resync).await;
            return;
        }
        self.store_cursor(&event.event_id);

        let channel_id = match event.channel_id.parse::<uuid::Uuid>() {
            Ok(id) => ChannelId::from(id),
            Err(_) => {
                // message_deleted events and reaction events still carry a
                // channel_id; without one we cannot route anything.
                tracing::warn!(
                    channel_id = %event.channel_id,
                    "Message event with unparseable channel_id; dropping"
                );
                return;
            }
        };

        match event.event {
            Some(MsgEventType::MessageSent(msg)) => {
                self.inbound.ingest(&msg, channel_id, true).await;
            }
            Some(MsgEventType::MessageUpdated(msg)) => {
                self.inbound.ingest_edit(&msg, channel_id).await;
            }
            Some(MsgEventType::MessageDeletedId(id)) => {
                self.handle_delete(&id, channel_id);
            }
            Some(MsgEventType::ReactionAdded(update)) => {
                self.handle_reaction(update, channel_id, false, event.timestamp);
            }
            Some(MsgEventType::ReactionRemoved(update)) => {
                self.handle_reaction(update, channel_id, true, event.timestamp);
            }
            // Handled above, before the channel_id parse.
            Some(MsgEventType::ResyncRequired(_)) => {}
            None => {
                tracing::warn!(
                    %channel_id,
                    event_id = %event.event_id,
                    "Message event with no payload arm"
                );
            }
        }
    }

    fn handle_reaction(
        &self,
        update: chat_v1::ReactionUpdate,
        channel_id: ChannelId,
        remove: bool,
        timestamp: u64,
    ) {
        let (Ok(sender_id), Ok(reactor_id)) =
            (update.target_author_aci.parse(), update.reactor_id.parse())
        else {
            tracing::warn!(%channel_id, "Reaction event has invalid author or reactor identity");
            return;
        };
        // A stream must never mutate a target in another conversation.
        match self.messages.get_by_sender_timestamp(&sender_id, update.target_sent_timestamp) {
            Ok(Some(message)) if message.conversation_id == channel_id => {}
            _ => return,
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
            Err(error) => tracing::warn!(%error, %channel_id, "Failed to persist server reaction"),
        }
    }

    /// Apply a remote "delete for everyone" to local storage and tell the UI.
    fn handle_delete(&self, message_id: &str, channel_id: ChannelId) {
        let Ok(uuid) = message_id.parse::<uuid::Uuid>() else {
            tracing::warn!(%channel_id, message_id, "Delete event with unparseable message id");
            return;
        };
        let id = MessageId::from(uuid);

        // The UI matches on (sender, sent_at) Signal-style, so read the local
        // row before removing it.
        let existing = match self.messages.get_message_local(&id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                tracing::debug!(%channel_id, %id, "Delete event for a message we never stored");
                return;
            }
            Err(e) => {
                tracing::warn!(%channel_id, %id, error = %e, "Failed to read message for delete event");
                return;
            }
        };

        if let Err(e) = self.messages.delete_message_local(&id) {
            tracing::warn!(%channel_id, %id, error = %e, "Failed to delete message locally");
            return;
        }

        let Some(author_id) = existing.sender_id else {
            tracing::debug!(%channel_id, %id, "Deleted message had no known author; not announcing");
            return;
        };

        let _ = self.event_bus.domain_sender().send(DomainEvent::MessageDeleted {
            message_id: id,
            author_id,
            channel_id: Some(channel_id),
            target_sent_timestamp: existing.sent_at,
        });
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
        let body = body.split_once("\n    fn cursor(").map(|(b, _)| b).unwrap_or(body);

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
        let forced = forced.split_once("\n    fn ").map(|(b, _)| b).unwrap_or(forced);

        assert!(
            !forced.contains("MIN_CATCHUP_INTERVAL"),
            "catch_up_now must ignore the rate limiter: a reconnecting client has usually \
             synced within MIN_CATCHUP_INTERVAL, so throttling it would skip precisely the \
             sync that closes the subscribe window (§103)."
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
        let body = body.split_once("\n    /// Dispatch one").map(|(b, _)| b).unwrap_or(body);

        assert!(
            body.contains("self.catch_up_now().await"),
            "a resync must run the forced catch-up: the skipped events are not coming on \
             the stream, and the rate-limited catch_up() can skip the sync that fetches them."
        );
    }
}
