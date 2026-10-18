//! The relay wire protocol and a single-relay client.
//!
//! Ported from `bitchat/Nostr/NostrRelayManager.swift` (framing, filters,
//! connection lifecycle, backoff) and `bitchat/Nostr/NostrRelayURL.swift` (URL
//! normalisation). The many-relay half lives in [`pool`]; the geo relay directory
//! in [`directory`].
//!
//! # Transport is injected, always
//!
//! Nothing in this module opens a socket. [`RelayConnector`] is handed in by the
//! integration layer, which supplies one backed by `chatter-core`'s existing
//! `arti-client` path. Two consequences, both deliberate:
//!
//! * There is exactly one Tor stack in this workspace. A second one here would be
//!   a second set of circuits, a second bootstrap, and a second thing to get wrong.
//! * Every test in this crate runs offline against a mock connector. A relay test
//!   that needs the network is a test that gets deleted the first time CI is
//!   offline.
//!
//! # A socket write is not a delivery
//!
//! [`RelayClient::publish`] returning `Ok` means the bytes reached the socket and
//! nothing more. A relay may still reject the event — rate limit, PoW requirement,
//! kind not served, spam filter — and it says so with `OK <id> false <reason>`.
//! Treat an event as published only on [`RelayMessage::Ok`] with `accepted = true`;
//! [`pool::RelayPool::publish_confirmed`] does exactly that. bitchat added this
//! after shipping the naive version, which showed messages as sent that were
//! dropped on arrival.
//!
//! # Ephemeral kinds are never stored
//!
//! Kinds 20000/20001 are ephemeral: relays forward them live and keep nothing. A
//! subscription with `since` in the past returns *no* stored events for them and
//! goes straight to `EOSE`. That is the protocol working, not a failure — do not
//! retry, do not widen the window, do not report an error.

pub mod directory;
pub mod pool;
mod protocol;
mod url;

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

pub use directory::{DEFAULT_GEO_RELAY_COUNT, GeoRelay, parse_directory, relays_for_geohash};
pub use pool::{InboundDedup, RelayPool, RelayPoolConfig, Verifier};
pub use protocol::{
    RelayFilter, RelayMessage, close_request, parse_relay_message, publish_request,
    subscription_request,
};
pub use url::{directory_address, normalize_relay_url};

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::NostrError;
use crate::types::NostrEvent;

// --- Constants, from `bitchat/Services/TransportConfig.swift` ---------------

/// First reconnect delay after a drop.
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Ceiling on the reconnect delay.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Each successive attempt multiplies the delay by this.
pub const BACKOFF_MULTIPLIER: f64 = 2.0;
/// Attempts before a relay is left alone.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 10;
/// ±20% jitter on every backoff interval.
pub const BACKOFF_JITTER_RATIO: f64 = 0.2;
/// Default `limit` on a fetch filter.
pub const DEFAULT_FETCH_LIMIT: usize = 100;
/// Events held for a relay that is not connected yet, per relay.
pub const PENDING_SEND_QUEUE_CAP: usize = 200;
/// How far back a DM (gift wrap) subscription looks on first connect.
pub const DM_SUBSCRIBE_LOOKBACK_SECS: i64 = 86_400;
/// How far back a geohash channel subscription looks on first connect.
pub const GEOHASH_INITIAL_LOOKBACK_SECS: i64 = 3_600;
/// Event cap on the initial geohash fetch.
pub const GEOHASH_INITIAL_LIMIT: usize = 200;

/// The gift-wrap-capable relays bitchat uses for DMs.
///
/// Geohash traffic goes to the [`directory`] relays instead; these four are the
/// ones both ends of a DM can be assumed to reach.
pub const DEFAULT_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://offchain.pub",
];

// --- Injected transport ----------------------------------------------------

/// A boxed future, because [`RelayConnector`] must be usable as `dyn`.
///
/// `async fn` in traits would read better, but it is not dyn-safe, and the whole
/// point of this trait is that the concrete transport (Tor in production, a mock in
/// tests) is chosen at runtime by the layer above.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The write half of a relay connection.
pub trait RelaySink: Send {
    /// Sends one text frame. The Nostr wire protocol is text-only.
    fn send(&mut self, text: String) -> BoxFuture<'_, Result<(), NostrError>>;
    /// Closes the connection.
    fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>>;
}

/// The read half of a relay connection.
pub trait RelayStream: Send {
    /// The next text frame, or `Ok(None)` when the relay closed cleanly.
    ///
    /// Implementations should swallow non-text frames (ping/pong/binary) rather
    /// than surfacing them; only Nostr frames concern this layer.
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>>;
}

/// A connection, split so that a send never has to wait on a read.
///
/// Splitting is not a style preference: the read half parks in `recv().await` for
/// as long as the relay is quiet, which with a single combined object would mean a
/// lock held across that await and a publish that can never make progress. It also
/// maps one-to-one onto `WebSocketStream::split()`.
pub type RelayConnection = (Box<dyn RelaySink>, Box<dyn RelayStream>);

/// Dials relays. Supplied by the integration layer — see the module docs.
pub trait RelayConnector: Send + Sync {
    fn connect<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<RelayConnection, NostrError>>;
}

// --- Client configuration --------------------------------------------------

/// A jitter source returning a value in `[0, 1)`.
pub type JitterFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Tunables for [`RelayClient`]. The defaults are bitchat's.
#[derive(Clone)]
pub struct RelayClientConfig {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
    pub max_reconnect_attempts: u32,
    pub backoff_jitter_ratio: f64,
    pub pending_send_cap: usize,
    /// Injected so tests are deterministic. Production uses [`entropy_jitter`].
    pub jitter: JitterFn,
}

impl Default for RelayClientConfig {
    fn default() -> Self {
        Self {
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            backoff_multiplier: BACKOFF_MULTIPLIER,
            max_reconnect_attempts: MAX_RECONNECT_ATTEMPTS,
            backoff_jitter_ratio: BACKOFF_JITTER_RATIO,
            pending_send_cap: PENDING_SEND_QUEUE_CAP,
            jitter: Arc::new(entropy_jitter),
        }
    }
}

impl fmt::Debug for RelayClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayClientConfig")
            .field("initial_backoff", &self.initial_backoff)
            .field("max_backoff", &self.max_backoff)
            .field("backoff_multiplier", &self.backoff_multiplier)
            .field("max_reconnect_attempts", &self.max_reconnect_attempts)
            .field("backoff_jitter_ratio", &self.backoff_jitter_ratio)
            .field("pending_send_cap", &self.pending_send_cap)
            .field("jitter", &"<fn>")
            .finish()
    }
}

/// A jitter value in `[0, 1)` derived from the wall clock and a process-local
/// counter.
///
/// Jitter does not need to be unpredictable, only *uncorrelated between clients*:
/// its whole job is to stop every client that lost the same relay from reconnecting
/// in the same millisecond and turning an outage into a thundering herd. Nanosecond
/// clock skew plus a counter gives that without a CSPRNG dependency, and nothing
/// security-relevant is derived from it.
pub fn entropy_jitter() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 8];
    match digest.get(..8) {
        Some(head) => bytes.copy_from_slice(head),
        None => return 0.5,
    }
    // 53 bits is the mantissa width, so this maps uniformly into [0, 1).
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// The delay before reconnect attempt `attempt` (1-based).
///
/// `min(initial * multiplier^(attempt-1), max) * (1 + (jitter*2 - 1) * ratio)`,
/// exactly bitchat's formula. The result is clamped into `[0, max * (1 + ratio)]`
/// and any non-finite intermediate falls back to `max` — a relay that manages to
/// drive this to NaN must not become an unbounded sleep or a panicking
/// `Duration::from_secs_f64`.
pub fn backoff_delay(attempt: u32, jitter_unit: f64, config: &RelayClientConfig) -> Duration {
    let exponent = attempt.saturating_sub(1).min(32);
    let base =
        config.initial_backoff.as_secs_f64() * config.backoff_multiplier.powi(exponent as i32);
    let base = base.min(config.max_backoff.as_secs_f64());

    let jitter_unit = if jitter_unit.is_finite() {
        jitter_unit.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let ratio = if config.backoff_jitter_ratio.is_finite() {
        config.backoff_jitter_ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let factor = 1.0 + (jitter_unit * 2.0 - 1.0) * ratio;

    let seconds = base * factor;
    if !seconds.is_finite() || seconds < 0.0 {
        return config.max_backoff;
    }
    Duration::try_from_secs_f64(seconds).unwrap_or(config.max_backoff)
}

// --- Client events ---------------------------------------------------------

/// Everything a [`RelayClient`] reports upward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayEvent {
    Connected {
        url: String,
    },
    Disconnected {
        url: String,
        reason: String,
    },
    /// The client has stopped trying this relay: `max_reconnect_attempts` reached.
    GaveUp {
        url: String,
    },
    Event {
        url: String,
        subscription_id: String,
        event: NostrEvent,
    },
    Eose {
        url: String,
        subscription_id: String,
    },
    /// Every relay targeted by this subscription has finished its stored-event
    /// phase — by sending `EOSE`, by dropping, or by the fallback timeout.
    ///
    /// Emitted by [`pool::RelayPool`] only, since a single client cannot know what
    /// the other relays did. This is the "initial load is done" signal; the
    /// per-relay [`RelayEvent::Eose`] still fires alongside it.
    SubscriptionComplete {
        subscription_id: String,
    },
    Ok {
        url: String,
        event_id: String,
        accepted: bool,
        message: String,
    },
    Notice {
        url: String,
        message: String,
    },
    Closed {
        url: String,
        subscription_id: String,
        message: String,
    },
}

type EventSender = tokio::sync::mpsc::UnboundedSender<RelayEvent>;

// --- The client ------------------------------------------------------------

#[derive(Default)]
struct ClientState {
    sink: Option<Box<dyn RelaySink>>,
    connected: bool,
    shutdown: bool,
    reconnect_attempts: u32,
    /// Subscription id → its `REQ` frame. Relays forget subscriptions with the
    /// socket, so every one of these is replayed on each (re)connect.
    subscriptions: BTreeMap<String, String>,
    /// `EVENT` frames waiting for a live socket.
    pending: VecDeque<String>,
    dropped_sends: u64,
    malformed_frames: u64,
}

/// One relay: its socket, its subscriptions, its backoff.
///
/// Cheap to clone behind an `Arc`; [`RelayClient::run`] is the connection loop and
/// everything else may be called from anywhere while it runs.
pub struct RelayClient {
    url: String,
    connector: Arc<dyn RelayConnector>,
    config: RelayClientConfig,
    events: EventSender,
    state: Mutex<ClientState>,
}

impl RelayClient {
    /// Builds a client for `url`, which must carry an explicit `ws`/`wss` scheme.
    ///
    /// The URL is normalised here so that the string this client answers to is the
    /// same string the pool keys its map by.
    pub fn new(
        url: &str,
        connector: Arc<dyn RelayConnector>,
        events: EventSender,
        config: RelayClientConfig,
    ) -> Result<Self, NostrError> {
        let url = normalize_relay_url(url, None)
            .ok_or_else(|| NostrError::Relay(format!("not a relay URL: {}", clip(url))))?;
        Ok(Self {
            url,
            connector,
            config,
            events,
            state: Mutex::new(ClientState::default()),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn is_connected(&self) -> bool {
        self.state.lock().await.connected
    }

    pub async fn reconnect_attempts(&self) -> u32 {
        self.state.lock().await.reconnect_attempts
    }

    /// Events queued because the socket was down when they were published.
    pub async fn pending_send_count(&self) -> usize {
        self.state.lock().await.pending.len()
    }

    /// Events discarded because the pending queue was full.
    pub async fn dropped_send_count(&self) -> u64 {
        self.state.lock().await.dropped_sends
    }

    /// Frames a relay sent that were not parseable Nostr messages.
    pub async fn malformed_frame_count(&self) -> u64 {
        self.state.lock().await.malformed_frames
    }

    pub async fn subscription_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .subscriptions
            .keys()
            .cloned()
            .collect()
    }

    /// Registers a subscription and sends it if the socket is up.
    ///
    /// The `REQ` is remembered either way: a relay drops every subscription when
    /// the socket dies, so reconnecting without replaying them leaves a client that
    /// looks connected and receives nothing.
    pub async fn subscribe(&self, id: &str, filters: &[RelayFilter]) -> Result<(), NostrError> {
        if filters.is_empty() {
            return Err(NostrError::Relay(
                "a subscription needs at least one filter".to_string(),
            ));
        }
        let frame = subscription_request(id, filters);
        let mut state = self.state.lock().await;
        state.subscriptions.insert(id.to_string(), frame.clone());
        Self::send_now(&mut state, frame).await;
        Ok(())
    }

    /// Forgets a subscription and tells the relay, if we are connected.
    pub async fn unsubscribe(&self, id: &str) {
        let mut state = self.state.lock().await;
        if state.subscriptions.remove(id).is_none() {
            return;
        }
        Self::send_now(&mut state, close_request(id)).await;
    }

    /// Sends an event, or queues it until the relay is reachable.
    ///
    /// Returning `Ok` means only that the frame was written or queued. Delivery is
    /// an `OK … true` and nothing less — see the module docs.
    pub async fn publish(&self, event: &NostrEvent) -> Result<(), NostrError> {
        let frame = publish_request(event)?;
        let mut state = self.state.lock().await;
        Self::send_or_queue(&mut state, frame, self.config.pending_send_cap).await;
        Ok(())
    }

    /// Writes a frame if there is a live socket, and reports whether it went.
    ///
    /// A failed write drops the sink: the socket is going away and the read loop
    /// will notice, but until it does nothing else should try to use a half-dead
    /// connection.
    ///
    /// Control frames use this and *only* this. A `REQ` that could not be sent must
    /// not enter the pending-send queue — the subscription map is already the
    /// replay mechanism, and queueing it as well would send every subscription
    /// twice on the next connect. A `CLOSE` we could not send needs no queueing at
    /// all: the relay dropped the subscription along with the socket.
    async fn send_now(state: &mut ClientState, frame: String) -> bool {
        let sent = match state.sink.as_mut() {
            None => false,
            Some(sink) => sink.send(frame).await.is_ok(),
        };
        if !sent && state.sink.is_some() {
            state.sink = None;
            state.connected = false;
        }
        sent
    }

    /// Writes a frame, queueing it for the next connection if that is not possible.
    /// Events use this: they have no other replay path.
    async fn send_or_queue(state: &mut ClientState, frame: String, cap: usize) {
        if !Self::send_now(state, frame.clone()).await {
            Self::enqueue(state, frame, cap);
        }
    }

    /// Queues a frame, dropping the *oldest* on overflow.
    ///
    /// Oldest-first is the right end to lose: what queues here is presence beacons
    /// and geohash chatter, where a stale beacon is worthless and the newest one is
    /// the whole signal. An unbounded queue would be a memory leak handed to us by
    /// any relay that simply stops answering.
    fn enqueue(state: &mut ClientState, frame: String, cap: usize) {
        state.pending.push_back(frame);
        while state.pending.len() > cap {
            if state.pending.pop_front().is_some() {
                state.dropped_sends = state.dropped_sends.saturating_add(1);
            }
        }
    }

    /// Stops the connection loop and closes the socket.
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        state.shutdown = true;
        state.connected = false;
        if let Some(mut sink) = state.sink.take() {
            let _ = sink.close().await;
        }
    }

    /// The connect → read → backoff loop. Run it once, on its own task.
    pub async fn run(self: Arc<Self>) {
        loop {
            if self.state.lock().await.shutdown {
                return;
            }

            match self.connector.connect(&self.url).await {
                Ok((sink, mut stream)) => {
                    if !self.on_connected(sink).await {
                        return;
                    }
                    let reason = loop {
                        match stream.recv().await {
                            Ok(Some(text)) => self.dispatch(&text).await,
                            Ok(None) => break "relay closed the connection".to_string(),
                            Err(error) => break error.to_string(),
                        }
                    };
                    self.on_disconnected(reason).await;
                }
                Err(error) => self.on_disconnected(error.to_string()).await,
            }

            let mut state = self.state.lock().await;
            if state.shutdown {
                return;
            }
            state.reconnect_attempts = state.reconnect_attempts.saturating_add(1);
            let attempt = state.reconnect_attempts;
            drop(state);

            if attempt >= self.config.max_reconnect_attempts {
                let _ = self.events.send(RelayEvent::GaveUp {
                    url: self.url.clone(),
                });
                return;
            }

            let delay = backoff_delay(attempt, (self.config.jitter)(), &self.config);
            tokio::time::sleep(delay).await;
        }
    }

    /// Installs a fresh socket and replays everything the relay has forgotten.
    /// Returns `false` if a shutdown landed while we were dialling.
    async fn on_connected(&self, sink: Box<dyn RelaySink>) -> bool {
        let mut state = self.state.lock().await;
        if state.shutdown {
            let mut sink = sink;
            let _ = sink.close().await;
            return false;
        }
        state.sink = Some(sink);
        state.connected = true;
        state.reconnect_attempts = 0;

        let _ = self.events.send(RelayEvent::Connected {
            url: self.url.clone(),
        });

        // Subscriptions first: an event published now may be answered on one of
        // them, and a `REQ` that arrives after the answer misses it.
        let cap = self.config.pending_send_cap;
        let subscriptions: Vec<String> = state.subscriptions.values().cloned().collect();
        for frame in subscriptions {
            if !Self::send_now(&mut state, frame).await {
                // The socket died mid-replay. The remaining subscriptions are still
                // in the map and go out on the next connection.
                return true;
            }
        }
        // Anything that cannot go now goes back on the queue, in order.
        let pending = std::mem::take(&mut state.pending);
        for frame in pending {
            Self::send_or_queue(&mut state, frame, cap).await;
        }
        true
    }

    async fn on_disconnected(&self, reason: String) {
        let mut state = self.state.lock().await;
        let was_connected = state.connected;
        state.connected = false;
        state.sink = None;
        drop(state);

        // A dial that never succeeded still reports `Disconnected`: the pool tracks
        // "this relay is not usable right now", and how it got there does not
        // change that.
        let _ = self.events.send(RelayEvent::Disconnected {
            url: self.url.clone(),
            reason: if was_connected {
                reason
            } else {
                format!("connect failed: {reason}")
            },
        });
    }

    async fn dispatch(&self, text: &str) {
        let url = self.url.clone();
        let message = match parse_relay_message(text) {
            Ok(message) => message,
            Err(_) => {
                // Unparseable frames are dropped, exactly as bitchat drops them. A
                // relay is not trusted to be well-behaved, and one bad frame must
                // not take down a connection that is otherwise working.
                self.state.lock().await.malformed_frames += 1;
                return;
            }
        };

        let event = match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } => RelayEvent::Event {
                url,
                subscription_id,
                event,
            },
            RelayMessage::Eose { subscription_id } => RelayEvent::Eose {
                url,
                subscription_id,
            },
            RelayMessage::Ok {
                event_id,
                accepted,
                message,
            } => RelayEvent::Ok {
                url,
                event_id,
                accepted,
                message,
            },
            RelayMessage::Notice(message) => RelayEvent::Notice { url, message },
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                // The relay ended this subscription; stop replaying it on reconnect.
                self.state
                    .lock()
                    .await
                    .subscriptions
                    .remove(&subscription_id);
                RelayEvent::Closed {
                    url,
                    subscription_id,
                    message,
                }
            }
        };
        let _ = self.events.send(event);
    }
}

impl fmt::Debug for RelayClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayClient")
            .field("url", &self.url)
            .finish()
    }
}

/// Truncates untrusted text before it reaches an error message or a log line.
fn clip(value: &str) -> String {
    value.chars().take(64).collect()
}
