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
//! optimisation. Redis cannot tell a client "your cursor was trimmed", so a
//! resume that lands past the server's replay buffer silently returns less
//! than was missed — which is exactly why every connect also syncs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
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
    last_catchup: Mutex<Option<Instant>>,
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
            last_catchup: Mutex::new(None),
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
            loop {
                match self.run_once().await {
                    Ok(()) => {
                        tracing::info!("Channel message stream ended; reconnecting");
                        backoff = INITIAL_BACKOFF;
                    }
                    Err(e) if is_no_channels(&e) => {
                        // The server refuses a global stream for a user who is
                        // in no channels. That is a normal state for a fresh
                        // account, not an error to shout about every second —
                        // wait out the full backoff quietly and try again
                        // (joining a channel nudges us awake immediately).
                        tracing::debug!("No channels to stream yet; will retry");
                        backoff = MAX_BACKOFF;
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
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        })
    }

    /// One connect → consume → disconnect cycle.
    async fn run_once(&self) -> Result<(), crate::error::NetworkError> {
        // Durable catch-up first: whatever was missed while we were down is
        // fetched from the server's message store, not from a live stream
        // that by definition was not running.
        self.catch_up().await;

        let cursor = self.cursor();
        let mut client = self.grpc_pool.message_client();
        // Empty channel_ids => the server subscribes us to every non-archived
        // channel we are a member of, including ones joined in a previous
        // session. That is the property the old per-channel subscriptions
        // lacked.
        let stream = client
            .stream_global_events(Vec::new(), cursor.as_deref())
            .await?;

        tracing::info!(
            resume_from = cursor.as_deref().unwrap_or("<none>"),
            "Channel message stream connected"
        );

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
    /// the server.
    async fn catch_up(&self) {
        {
            let mut last = self.last_catchup.lock();
            if let Some(at) = *last {
                if at.elapsed() < MIN_CATCHUP_INTERVAL {
                    return;
                }
            }
            *last = Some(Instant::now());
        }

        let result = self.sync_service.trigger_sync().await;
        tracing::info!(
            channels = result.channels.synced,
            messages = result.messages.total_inserted,
            "Channel stream catch-up complete"
        );
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
            Some(MsgEventType::ReactionAdded(update)) | Some(MsgEventType::ReactionRemoved(update)) => {
                // Deliberately not applied. `ReactionUpdate` carries the
                // target message and emoji but NOT the reactor's user id, and
                // per-user reaction state cannot be reconstructed without it —
                // applying this would mean inventing a reactor. Nothing in
                // this client calls the server's AddReaction RPC either: every
                // reaction we send or receive rides E2E content through the
                // device inbox (`inbox_processor`), which does carry the
                // reactor. Adding `reactor_id` to the proto is WP5's call; if
                // it lands, apply it here.
                tracing::debug!(
                    %channel_id,
                    emoji = %update.emoji,
                    target_author = %update.target_author_aci,
                    "Server reaction event ignored: proto carries no reactor id (E2E path is authoritative)"
                );
            }
            None => {
                tracing::warn!(
                    %channel_id,
                    event_id = %event.event_id,
                    "Message event with no payload arm"
                );
            }
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
