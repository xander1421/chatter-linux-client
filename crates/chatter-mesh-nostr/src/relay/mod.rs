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

pub use directory::{DEFAULT_GEO_RELAY_COUNT, GeoRelay, parse_directory, relays_for_geohash};
pub use pool::{InboundDedup, RelayPool, RelayPoolConfig, Verifier};

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
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

// --- URL normalisation -----------------------------------------------------

/// Canonicalises a relay URL, or `None` if it is not one.
///
/// Ported from `NostrRelayURL.normalized`. `https`/`wss` collapse to `wss` and
/// `http`/`ws` to `ws`; the host lowercases; the scheme's default port and a bare
/// `"/"` path drop; the fragment is discarded. Canonical form matters beyond
/// tidiness — the URL string is the map key that decides whether we already hold a
/// connection, so `wss://R.example:443/` and `wss://r.example` must not become two
/// sockets to the same relay.
///
/// Two deliberate tightenings over `URLComponents`:
///
/// * An authority carrying userinfo (`user:pass@host`) is rejected. Relay URLs
///   arrive from a downloaded CSV and from user input; credentials in one are
///   either a mistake or an attempt to smuggle something past the host check.
/// * Hosts are ASCII. `URLComponents` would percent-encode or punycode a
///   non-ASCII host; producing a different byte string than bitchat for the same
///   input is worse than refusing it, and no relay uses one.
pub fn normalize_relay_url(raw: &str, default_scheme: Option<&str>) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let with_scheme;
    let value = if trimmed.contains("://") {
        trimmed
    } else {
        with_scheme = format!("{}://{trimmed}", default_scheme?);
        with_scheme.as_str()
    };

    let (raw_scheme, rest) = value.split_once("://")?;
    let (scheme, default_port) = match raw_scheme.to_ascii_lowercase().as_str() {
        "wss" | "https" => ("wss", 443u16),
        "ws" | "http" => ("ws", 80u16),
        _ => return None,
    };

    // The fragment never reaches the wire.
    let rest = match rest.split_once('#') {
        Some((head, _)) => head,
        None => rest,
    };

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end)?;
    let tail = rest.get(authority_end..)?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    let (raw_host, raw_port) = split_host_port(authority)?;
    let host = raw_host.to_ascii_lowercase();
    if !is_valid_host(&host) {
        return None;
    }

    let port = match raw_port {
        // `host:` has no port at all, which is how `URLComponents` reads it too.
        None | Some("") => None,
        Some(digits) => Some(digits.parse::<u16>().ok()?),
    }
    .filter(|port| *port != default_port);

    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    let path = if path == "/" { "" } else { path };

    let mut out = String::with_capacity(value.len() + 8);
    out.push_str(scheme);
    out.push_str("://");
    out.push_str(&host);
    if let Some(port) = port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

/// The normalised host (and port, and path) with the scheme stripped — the form
/// the geo relay CSV stores. Ported from `NostrRelayURL.directoryAddress`.
pub fn directory_address(raw: &str) -> Option<String> {
    let normalized = normalize_relay_url(raw, Some("wss"))?;
    for prefix in ["wss://", "ws://"] {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    Some(normalized)
}

/// Splits `host:port`, keeping a bracketed IPv6 literal intact.
fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = authority.get(..=close)?;
        // `]` is one byte, so this index is on a character boundary.
        let after = authority.get(close + 1..)?;
        return match after {
            "" => Some((host, None)),
            other => Some((host, Some(other.strip_prefix(':')?))),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

fn is_valid_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

// --- Filters ---------------------------------------------------------------

/// A NIP-01 subscription filter.
///
/// Empty `kinds`/`authors` mean "no constraint" and are omitted from the JSON, so
/// `RelayFilter::default()` serialises to `{}` and matches everything. A tag whose
/// value list is empty is *not* omitted: `{"#g": []}` is a filter that matches
/// nothing, and silently widening it to "any geohash" would subscribe a user to
/// traffic they never asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayFilter {
    pub kinds: Vec<u16>,
    pub authors: Vec<String>,
    /// Tag filters, serialised as `#<name>`. Order is preserved.
    pub tags: Vec<(String, Vec<String>)>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<usize>,
}

impl RelayFilter {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        if !self.kinds.is_empty() {
            map.insert(
                "kinds".to_string(),
                Value::Array(self.kinds.iter().map(|k| Value::from(*k)).collect()),
            );
        }
        if !self.authors.is_empty() {
            map.insert(
                "authors".to_string(),
                Value::Array(
                    self.authors
                        .iter()
                        .map(|a| Value::from(a.as_str()))
                        .collect(),
                ),
            );
        }
        for (name, values) in &self.tags {
            map.insert(
                format!("#{name}"),
                Value::Array(values.iter().map(|v| Value::from(v.as_str())).collect()),
            );
        }
        if let Some(since) = self.since {
            map.insert("since".to_string(), Value::from(since));
        }
        if let Some(until) = self.until {
            map.insert("until".to_string(), Value::from(until));
        }
        if let Some(limit) = self.limit {
            map.insert("limit".to_string(), Value::from(limit));
        }
        Value::Object(map)
    }

    /// NIP-17 gift wraps addressed to us.
    pub fn gift_wraps_for(pubkey: &str, since: Option<i64>) -> Self {
        Self {
            kinds: vec![1059],
            tags: vec![("p".to_string(), vec![pubkey.to_string()])],
            since,
            limit: Some(DEFAULT_FETCH_LIMIT),
            ..Self::default()
        }
    }

    /// Geohash channel traffic: chat (20000) and presence (20001).
    ///
    /// Both kinds are ephemeral, so `since` only bounds what a relay forwards live
    /// — it can never produce history.
    pub fn geohash_ephemeral(geohash: &str, since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![20000, 20001],
            tags: vec![("g".to_string(), vec![geohash.to_string()])],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Persistent location notes (kind 1) for one or more cells.
    pub fn geohash_notes(geohashes: &[String], since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![1],
            tags: vec![("g".to_string(), geohashes.to_vec())],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Mesh-bridge rendezvous, tagged `#r` with the cell and its neighbours.
    pub fn bridge_rendezvous(cells: &[String], since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![20000, 20001],
            tags: vec![("r".to_string(), cells.to_vec())],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Courier drops (kind 1401) parked under rotating `#x` recipient tags.
    pub fn courier_drops(recipient_tags_hex: &[String], since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![1401],
            tags: vec![("x".to_string(), recipient_tags_hex.to_vec())],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }
}

// --- Framing ---------------------------------------------------------------

/// A frame from a relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMessage {
    Event {
        subscription_id: String,
        event: NostrEvent,
    },
    /// End of stored events: everything that follows on this subscription is live.
    Eose {
        subscription_id: String,
    },
    /// NIP-20 publish result. `accepted = false` means the event was **dropped**.
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    Notice(String),
    /// The relay terminated a subscription from its side.
    Closed {
        subscription_id: String,
        message: String,
    },
}

/// What bitchat substitutes when a relay sends an `OK` with no reason.
const NO_REASON_GIVEN: &str = "no reason given";

/// Parses one relay frame.
///
/// Everything here runs on bytes chosen by an untrusted relay, so every access is
/// checked and nothing is indexed. A frame we cannot make sense of is an error the
/// caller drops, never a panic and never a partially-applied state change.
pub fn parse_relay_message(raw: &str) -> Result<RelayMessage, NostrError> {
    let value: Value = serde_json::from_str(raw).map_err(|e| NostrError::Json(e.to_string()))?;
    let array = value
        .as_array()
        .ok_or_else(|| NostrError::Relay("relay frame is not a JSON array".to_string()))?;

    let kind = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| NostrError::Relay("relay frame has no type".to_string()))?;

    match kind {
        "EVENT" => {
            let subscription_id = string_at(array, 1, "EVENT subscription id")?;
            let raw_event = array
                .get(2)
                .ok_or_else(|| NostrError::Relay("EVENT frame has no event".to_string()))?;
            let event: NostrEvent = serde_json::from_value(raw_event.clone())
                .map_err(|e| NostrError::Json(e.to_string()))?;
            Ok(RelayMessage::Event {
                subscription_id,
                event,
            })
        }
        "EOSE" => Ok(RelayMessage::Eose {
            subscription_id: string_at(array, 1, "EOSE subscription id")?,
        }),
        "OK" => {
            let event_id = string_at(array, 1, "OK event id")?;
            let accepted = array
                .get(2)
                .and_then(Value::as_bool)
                .ok_or_else(|| NostrError::Relay("OK frame has no accepted flag".to_string()))?;
            let message = array
                .get(3)
                .and_then(Value::as_str)
                .unwrap_or(NO_REASON_GIVEN)
                .to_string();
            Ok(RelayMessage::Ok {
                event_id,
                accepted,
                message,
            })
        }
        "NOTICE" => Ok(RelayMessage::Notice(string_at(array, 1, "NOTICE message")?)),
        "CLOSED" => Ok(RelayMessage::Closed {
            subscription_id: string_at(array, 1, "CLOSED subscription id")?,
            message: array
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        other => Err(NostrError::Relay(format!(
            "unknown relay frame type {}",
            // Truncated: the type is attacker-chosen and this string ends up in a
            // log line.
            other.chars().take(32).collect::<String>()
        ))),
    }
}

fn string_at(array: &[Value], index: usize, what: &'static str) -> Result<String, NostrError> {
    array
        .get(index)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| NostrError::Relay(format!("relay frame is missing {what}")))
}

/// `["REQ", <id>, <filter>, ...]`.
///
/// A relay requires at least one filter; passing none produces a frame it will
/// reject. [`RelayClient::subscribe`] refuses that case up front — this function
/// stays infallible so framing can be tested as pure data.
pub fn subscription_request(id: &str, filters: &[RelayFilter]) -> String {
    let mut frame = vec![Value::from("REQ"), Value::from(id)];
    frame.extend(filters.iter().map(RelayFilter::to_json));
    Value::Array(frame).to_string()
}

/// `["EVENT", <event>]`.
pub fn publish_request(event: &NostrEvent) -> Result<String, NostrError> {
    let value = serde_json::to_value(event).map_err(|e| NostrError::Json(e.to_string()))?;
    Ok(Value::Array(vec![Value::from("EVENT"), value]).to_string())
}

/// `["CLOSE", <id>]`.
pub fn close_request(id: &str) -> String {
    Value::Array(vec![Value::from("CLOSE"), Value::from(id)]).to_string()
}

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

#[cfg(test)]
pub(crate) mod mock {
    //! An in-memory relay, so no test in this crate touches the network.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;

    /// What a mock connection does when dialled.
    pub enum Script {
        /// Accept, then serve these frames, then stay open until closed.
        Serve(Vec<String>),
        /// Accept, serve these frames, then close cleanly.
        ServeThenClose(Vec<String>),
        /// Refuse to connect.
        Fail(&'static str),
    }

    #[derive(Default)]
    pub struct MockRelay {
        scripts: StdMutex<VecDeque<Script>>,
        /// Every frame the client wrote, in order.
        pub sent: Arc<StdMutex<Vec<String>>>,
        pub connect_count: Arc<StdMutex<usize>>,
        /// Feeds frames to whichever connection is currently live.
        live: StdMutex<Option<mpsc::UnboundedSender<Option<String>>>>,
    }

    impl MockRelay {
        pub fn new(scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts.into()),
                ..Self::default()
            })
        }

        pub fn sent_frames(&self) -> Vec<String> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }

        pub fn connects(&self) -> usize {
            self.connect_count.lock().map(|c| *c).unwrap_or(0)
        }

        /// Pushes a frame into the live connection.
        pub fn push(&self, frame: &str) {
            if let Ok(live) = self.live.lock() {
                if let Some(tx) = live.as_ref() {
                    let _ = tx.send(Some(frame.to_string()));
                }
            }
        }

        /// Closes the live connection from the relay's side.
        pub fn drop_connection(&self) {
            if let Ok(live) = self.live.lock() {
                if let Some(tx) = live.as_ref() {
                    let _ = tx.send(None);
                }
            }
        }
    }

    impl RelayConnector for MockRelay {
        fn connect<'a>(
            &'a self,
            _url: &'a str,
        ) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
            Box::pin(async move {
                if let Ok(mut count) = self.connect_count.lock() {
                    *count += 1;
                }
                let script = self
                    .scripts
                    .lock()
                    .ok()
                    .and_then(|mut s| s.pop_front())
                    .unwrap_or(Script::Serve(Vec::new()));

                let (frames, close_after) = match script {
                    Script::Fail(reason) => {
                        return Err(NostrError::Relay(reason.to_string()));
                    }
                    Script::Serve(frames) => (frames, false),
                    Script::ServeThenClose(frames) => (frames, true),
                };

                let (tx, rx) = mpsc::unbounded_channel();
                for frame in frames {
                    let _ = tx.send(Some(frame));
                }
                if close_after {
                    let _ = tx.send(None);
                }
                if let Ok(mut live) = self.live.lock() {
                    *live = Some(tx);
                }

                let sink: Box<dyn RelaySink> = Box::new(MockSink {
                    sent: Arc::clone(&self.sent),
                });
                let stream: Box<dyn RelayStream> = Box::new(MockStream { rx });
                Ok((sink, stream))
            })
        }
    }

    pub struct MockSink {
        sent: Arc<StdMutex<Vec<String>>>,
    }

    impl RelaySink for MockSink {
        fn send(&mut self, text: String) -> BoxFuture<'_, Result<(), NostrError>> {
            if let Ok(mut sent) = self.sent.lock() {
                sent.push(text);
            }
            Box::pin(async { Ok(()) })
        }

        fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>> {
            Box::pin(async { Ok(()) })
        }
    }

    pub struct MockStream {
        rx: mpsc::UnboundedReceiver<Option<String>>,
    }

    impl RelayStream for MockStream {
        fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>> {
            Box::pin(async move { Ok(self.rx.recv().await.flatten()) })
        }
    }

    /// A stream that never yields and never ends — a connection that is simply
    /// quiet, which is what a healthy relay looks like most of the time.
    pub struct PendingStream;

    impl RelayStream for PendingStream {
        fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>> {
            Box::pin(std::future::pending())
        }
    }

    /// A connector whose sink always fails, to exercise the write-failure path.
    pub struct BrokenSinkRelay;

    impl RelayConnector for BrokenSinkRelay {
        fn connect<'a>(
            &'a self,
            _url: &'a str,
        ) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
            Box::pin(async move {
                let sink: Box<dyn RelaySink> = Box::new(BrokenSink);
                let stream: Box<dyn RelayStream> = Box::new(PendingStream);
                Ok((sink, stream))
            })
        }
    }

    struct BrokenSink;

    impl RelaySink for BrokenSink {
        fn send(&mut self, _text: String) -> BoxFuture<'_, Result<(), NostrError>> {
            Box::pin(async { Err(NostrError::Relay("socket is gone".to_string())) })
        }

        fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A config with millisecond backoffs and no jitter, so lifecycle tests are
    /// fast and deterministic.
    pub fn fast_config() -> RelayClientConfig {
        RelayClientConfig {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            backoff_jitter_ratio: 0.0,
            jitter: Arc::new(|| 0.5),
            ..RelayClientConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{BrokenSinkRelay, MockRelay, Script, fast_config};
    use super::*;
    use tokio::sync::mpsc;

    fn event(id: &str) -> NostrEvent {
        NostrEvent {
            id: id.to_string(),
            pubkey: "ab".repeat(32),
            created_at: 1_700_000_000,
            kind: 20000,
            tags: vec![vec!["g".to_string(), "u4pruy".to_string()]],
            content: "hello".to_string(),
            sig: Some("cd".repeat(64)),
        }
    }

    fn event_frame(sub: &str, id: &str) -> String {
        let value = serde_json::to_value(event(id)).expect("event serialises");
        Value::Array(vec![Value::from("EVENT"), Value::from(sub), value]).to_string()
    }

    // --- URL normalisation ---

    #[test]
    fn normalisation_collapses_schemes_and_default_ports() {
        let cases = [
            ("wss://relay.damus.io", "wss://relay.damus.io"),
            ("https://relay.damus.io", "wss://relay.damus.io"),
            ("wss://relay.damus.io:443", "wss://relay.damus.io"),
            ("wss://relay.damus.io:8443", "wss://relay.damus.io:8443"),
            ("ws://localhost:8080", "ws://localhost:8080"),
            ("http://localhost", "ws://localhost"),
            ("ws://localhost:80", "ws://localhost"),
            // wss keeps :80 — only *its* default port drops.
            ("https://relay.example:80", "wss://relay.example:80"),
            ("WSS://RELAY.Example.COM/", "wss://relay.example.com"),
            ("  wss://relay.example  ", "wss://relay.example"),
            ("wss://relay.example/path", "wss://relay.example/path"),
            // A bare `/` path drops, so what is left is the query.
            ("wss://relay.example/?a=1", "wss://relay.example?a=1"),
            ("wss://relay.example/x?a=1", "wss://relay.example/x?a=1"),
            ("wss://relay.example#frag", "wss://relay.example"),
            ("wss://[::1]:443", "wss://[::1]"),
            ("wss://[::1]:9000", "wss://[::1]:9000"),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                normalize_relay_url(raw, None).as_deref(),
                Some(expected),
                "{raw}"
            );
        }
    }

    #[test]
    fn normalisation_rejects_what_is_not_a_relay_url() {
        for raw in [
            "",
            "   ",
            "relay.damus.io",          // no scheme and no default offered
            "ftp://relay.damus.io",    // wrong scheme
            "wss://",                  // no host
            "wss:///path",             // no host
            "wss://user:pass@relay.x", // userinfo
            "wss://relay.x:notaport",
            "wss://relay.x:99999", // out of u16 range
            "wss://relä.example",  // non-ASCII host
            "wss://[::1",          // unterminated bracket
        ] {
            assert_eq!(normalize_relay_url(raw, None), None, "{raw}");
        }
    }

    #[test]
    fn a_default_scheme_is_only_applied_when_none_is_present() {
        assert_eq!(
            normalize_relay_url("relay.damus.io", Some("wss")).as_deref(),
            Some("wss://relay.damus.io")
        );
        assert_eq!(
            normalize_relay_url("ws://relay.damus.io", Some("wss")).as_deref(),
            Some("ws://relay.damus.io")
        );
    }

    #[test]
    fn directory_addresses_drop_the_scheme() {
        assert_eq!(
            directory_address("relay.paulstephenborile.com:443").as_deref(),
            Some("relay.paulstephenborile.com")
        );
        assert_eq!(
            directory_address("relay.lab.rytswd.com").as_deref(),
            Some("relay.lab.rytswd.com")
        );
        assert_eq!(
            directory_address("ws://relay.example:8080").as_deref(),
            Some("relay.example:8080")
        );
        assert_eq!(directory_address("not a host"), None);
    }

    // --- Filters ---

    #[test]
    fn an_empty_filter_is_an_empty_object() {
        assert_eq!(RelayFilter::default().to_json(), serde_json::json!({}));
    }

    #[test]
    fn filters_serialise_tags_with_a_hash_prefix() {
        let filter = RelayFilter {
            kinds: vec![20000, 20001],
            authors: vec!["aa".to_string()],
            tags: vec![("g".to_string(), vec!["u4pruy".to_string()])],
            since: Some(1_700_000_000),
            until: Some(1_700_003_600),
            limit: Some(200),
        };
        assert_eq!(
            filter.to_json(),
            serde_json::json!({
                "kinds": [20000, 20001],
                "authors": ["aa"],
                "#g": ["u4pruy"],
                "since": 1_700_000_000,
                "until": 1_700_003_600,
                "limit": 200,
            })
        );
    }

    #[test]
    fn a_tag_with_no_values_stays_a_matches_nothing_filter() {
        let filter = RelayFilter {
            tags: vec![("g".to_string(), Vec::new())],
            ..RelayFilter::default()
        };
        assert_eq!(filter.to_json(), serde_json::json!({ "#g": [] }));
    }

    #[test]
    fn the_ported_filter_constructors_match_bitchat() {
        assert_eq!(
            RelayFilter::gift_wraps_for("abcd", Some(100)).to_json(),
            serde_json::json!({ "kinds": [1059], "#p": ["abcd"], "since": 100, "limit": 100 })
        );
        assert_eq!(
            RelayFilter::geohash_ephemeral("u4pruy", None, GEOHASH_INITIAL_LIMIT).to_json(),
            serde_json::json!({ "kinds": [20000, 20001], "#g": ["u4pruy"], "limit": 200 })
        );
        assert_eq!(
            RelayFilter::geohash_notes(&["u4p".to_string()], None, 200).to_json(),
            serde_json::json!({ "kinds": [1], "#g": ["u4p"], "limit": 200 })
        );
        assert_eq!(
            RelayFilter::bridge_rendezvous(&["u4p".to_string()], None, 200).to_json(),
            serde_json::json!({ "kinds": [20000, 20001], "#r": ["u4p"], "limit": 200 })
        );
        assert_eq!(
            RelayFilter::courier_drops(&["ff".to_string()], None, 100).to_json(),
            serde_json::json!({ "kinds": [1401], "#x": ["ff"], "limit": 100 })
        );
    }

    // --- Outbound framing ---

    #[test]
    fn subscription_requests_are_req_id_then_filters() {
        let frame = subscription_request(
            "sub-1",
            &[
                RelayFilter {
                    kinds: vec![1],
                    ..RelayFilter::default()
                },
                RelayFilter {
                    kinds: vec![2],
                    ..RelayFilter::default()
                },
            ],
        );
        let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!(["REQ", "sub-1", { "kinds": [1] }, { "kinds": [2] }])
        );
    }

    #[test]
    fn publish_and_close_requests_have_the_right_shape() {
        let frame = publish_request(&event("ff")).expect("serialises");
        let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
        let array = parsed.as_array().expect("array");
        assert_eq!(array.first().and_then(Value::as_str), Some("EVENT"));
        assert_eq!(
            array
                .get(1)
                .and_then(|e| e.get("id"))
                .and_then(Value::as_str),
            Some("ff")
        );

        let parsed: Value = serde_json::from_str(&close_request("sub-1")).expect("valid JSON");
        assert_eq!(parsed, serde_json::json!(["CLOSE", "sub-1"]));
    }

    #[test]
    fn subscription_ids_that_need_escaping_survive_the_round_trip() {
        let id = "sub\"1\\/\n";
        let frame = subscription_request(id, &[RelayFilter::default()]);
        let parsed: Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(
            parsed
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_str),
            Some(id)
        );
    }

    // --- Inbound parsing ---

    #[test]
    fn parses_every_frame_type() {
        let sub = "sub-1";
        match parse_relay_message(&event_frame(sub, "ff")).expect("EVENT parses") {
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                assert_eq!(subscription_id, sub);
                assert_eq!(event.id, "ff");
                assert_eq!(event.kind, 20000);
                assert_eq!(event.tag_value("g"), Some("u4pruy"));
            }
            other => panic!("expected EVENT, got {other:?}"),
        }

        assert_eq!(
            parse_relay_message(r#"["EOSE","sub-1"]"#).expect("EOSE parses"),
            RelayMessage::Eose {
                subscription_id: "sub-1".to_string()
            }
        );
        assert_eq!(
            parse_relay_message(r#"["OK","ff",true,"stored"]"#).expect("OK parses"),
            RelayMessage::Ok {
                event_id: "ff".to_string(),
                accepted: true,
                message: "stored".to_string(),
            }
        );
        assert_eq!(
            parse_relay_message(r#"["NOTICE","slow down"]"#).expect("NOTICE parses"),
            RelayMessage::Notice("slow down".to_string())
        );
        assert_eq!(
            parse_relay_message(r#"["CLOSED","sub-1","auth-required"]"#).expect("CLOSED parses"),
            RelayMessage::Closed {
                subscription_id: "sub-1".to_string(),
                message: "auth-required".to_string(),
            }
        );
    }

    #[test]
    fn a_rejecting_ok_parses_as_a_rejection() {
        // The case that matters: a socket write succeeded and the event was still
        // dropped. Nothing may read this as success.
        let parsed =
            parse_relay_message(r#"["OK","ff",false,"blocked: pow required"]"#).expect("OK parses");
        assert_eq!(
            parsed,
            RelayMessage::Ok {
                event_id: "ff".to_string(),
                accepted: false,
                message: "blocked: pow required".to_string(),
            }
        );
    }

    #[test]
    fn an_ok_without_a_reason_gets_the_placeholder() {
        assert_eq!(
            parse_relay_message(r#"["OK","ff",false]"#).expect("OK parses"),
            RelayMessage::Ok {
                event_id: "ff".to_string(),
                accepted: false,
                message: NO_REASON_GIVEN.to_string(),
            }
        );
    }

    #[test]
    fn malformed_frames_are_errors_never_panics() {
        let bad = [
            "",
            "not json",
            "{}",
            "[]",
            "[1,2,3]",
            r#"["EVENT"]"#,
            r#"["EVENT","sub"]"#,
            r#"["EVENT",5,{}]"#,
            r#"["EVENT","sub",{"id":"ff"}]"#, // event missing required fields
            r#"["EVENT","sub","not an object"]"#,
            r#"["EOSE"]"#,
            r#"["EOSE",null]"#,
            r#"["OK","ff"]"#,
            r#"["OK","ff","yes"]"#, // accepted must be a bool
            r#"["OK",5,true]"#,
            r#"["NOTICE"]"#,
            r#"["CLOSED"]"#,
            r#"["UNKNOWN","x"]"#,
            r#"["REQ","sub",{}]"#, // a client frame, not a relay frame
            "[\"EVENT\",\"sub\",{\"id\":\"ff\",\"pubkey\":\"aa\",\"created_at\":\"soon\",\"kind\":1,\"tags\":[],\"content\":\"\"}]",
        ];
        for frame in bad {
            assert!(parse_relay_message(frame).is_err(), "{frame}");
        }
    }

    #[test]
    fn an_event_without_a_signature_still_parses() {
        // Signature verification belongs to the crypto layer, and the pool's
        // verifier hook is where it plugs in. Parsing must not be the gate.
        let frame = r#"["EVENT","s",{"id":"ff","pubkey":"aa","created_at":1,"kind":1,"tags":[],"content":"x"}]"#;
        match parse_relay_message(frame).expect("parses") {
            RelayMessage::Event { event, .. } => assert_eq!(event.sig, None),
            other => panic!("expected EVENT, got {other:?}"),
        }
    }

    #[test]
    fn an_absurd_frame_type_is_truncated_in_the_error() {
        let kind = "A".repeat(5000);
        let frame = Value::Array(vec![Value::from(kind), Value::from("x")]).to_string();
        let error = parse_relay_message(&frame).expect_err("unknown type");
        assert!(error.to_string().len() < 128, "{error}");
    }

    // --- Backoff ---

    #[test]
    fn backoff_doubles_and_saturates_at_the_ceiling() {
        let config = RelayClientConfig {
            backoff_jitter_ratio: 0.0,
            ..RelayClientConfig::default()
        };
        let secs = |attempt| backoff_delay(attempt, 0.5, &config).as_secs_f64();
        assert!((secs(1) - 1.0).abs() < 1e-9);
        assert!((secs(2) - 2.0).abs() < 1e-9);
        assert!((secs(3) - 4.0).abs() < 1e-9);
        assert!((secs(9) - 256.0).abs() < 1e-9);
        // 512 would exceed the 300 s ceiling.
        assert!((secs(10) - 300.0).abs() < 1e-9);
        assert!((secs(100) - 300.0).abs() < 1e-9);
    }

    #[test]
    fn backoff_jitter_stays_within_twenty_percent() {
        let config = RelayClientConfig::default();
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            let base = (INITIAL_BACKOFF.as_secs_f64() * 2f64.powi(attempt as i32 - 1))
                .min(MAX_BACKOFF.as_secs_f64());
            for unit in [0.0, 0.25, 0.5, 0.75, 0.999] {
                let delay = backoff_delay(attempt, unit, &config).as_secs_f64();
                assert!(delay >= base * 0.8 - 1e-9, "attempt {attempt} unit {unit}");
                assert!(delay <= base * 1.2 + 1e-9, "attempt {attempt} unit {unit}");
            }
        }
    }

    #[test]
    fn jitter_actually_spreads_reconnects() {
        // Synchronised reconnects are how a relay outage becomes a stampede, so a
        // constant delay here would be a bug, not a simplification.
        let config = RelayClientConfig::default();
        let low = backoff_delay(5, 0.0, &config);
        let high = backoff_delay(5, 1.0, &config);
        assert!(high > low);
        assert!(high.as_secs_f64() / low.as_secs_f64() > 1.4);
    }

    #[test]
    fn backoff_survives_hostile_inputs() {
        let config = RelayClientConfig {
            backoff_multiplier: f64::INFINITY,
            ..RelayClientConfig::default()
        };
        assert!(backoff_delay(3, f64::NAN, &config) <= MAX_BACKOFF);
        assert!(backoff_delay(u32::MAX, -5.0, &config) <= MAX_BACKOFF);

        let negative = RelayClientConfig {
            backoff_jitter_ratio: -1.0,
            ..RelayClientConfig::default()
        };
        assert!(backoff_delay(1, 0.0, &negative) >= Duration::ZERO);
    }

    #[test]
    fn entropy_jitter_is_in_range_and_not_constant() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let unit = entropy_jitter();
            assert!((0.0..1.0).contains(&unit), "{unit}");
            seen.insert(unit.to_bits());
        }
        assert!(
            seen.len() > 32,
            "jitter must vary, got {} values",
            seen.len()
        );
    }

    // --- Client lifecycle, all against the mock ---

    fn client<C: RelayConnector + 'static>(
        relay: Arc<C>,
    ) -> (Arc<RelayClient>, mpsc::UnboundedReceiver<RelayEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let client =
            RelayClient::new("wss://relay.test", relay, tx, fast_config()).expect("valid URL");
        (Arc::new(client), rx)
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<RelayEvent>) -> RelayEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an event arrives")
            .expect("channel open")
    }

    #[tokio::test]
    async fn rejects_a_url_it_cannot_normalise() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let relay: Arc<dyn RelayConnector> = MockRelay::new(vec![]);
        assert!(RelayClient::new("not a url", relay, tx, fast_config()).is_err());
    }

    #[tokio::test]
    async fn connects_replays_subscriptions_and_forwards_events() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));

        client
            .subscribe(
                "sub-1",
                &[RelayFilter::geohash_ephemeral("u4pruy", None, 200)],
            )
            .await
            .expect("subscribes");
        // Remembered while disconnected, not sent — and specifically *not* parked in
        // the pending-send queue, or connecting would emit the REQ twice.
        assert!(relay.sent_frames().is_empty());
        assert_eq!(client.pending_send_count().await, 0);
        assert_eq!(client.subscription_ids().await, vec!["sub-1".to_string()]);

        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        // The REQ was replayed on connect.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let sent = relay.sent_frames();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(
            sent.first()
                .is_some_and(|f| f.starts_with(r#"["REQ","sub-1""#)),
            "{sent:?}"
        );

        relay.push(&event_frame("sub-1", "ff"));
        match next_event(&mut rx).await {
            RelayEvent::Event {
                subscription_id,
                event,
                ..
            } => {
                assert_eq!(subscription_id, "sub-1");
                assert_eq!(event.id, "ff");
            }
            other => panic!("expected an event, got {other:?}"),
        }

        relay.push(r#"["EOSE","sub-1"]"#);
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Eose {
                url: "wss://relay.test".to_string(),
                subscription_id: "sub-1".to_string()
            }
        );

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn publishes_queue_while_disconnected_and_flush_on_connect() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));

        client.publish(&event("aa")).await.expect("queues");
        client.publish(&event("bb")).await.expect("queues");
        assert_eq!(client.pending_send_count().await, 2);

        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(client.pending_send_count().await, 0);
        let sent = relay.sent_frames();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(
            sent.iter().all(|f| f.starts_with(r#"["EVENT""#)),
            "{sent:?}"
        );

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn the_pending_queue_is_bounded_and_drops_the_oldest() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, _rx) = client(Arc::clone(&relay));

        for index in 0..PENDING_SEND_QUEUE_CAP + 25 {
            client
                .publish(&event(&format!("{index:04}")))
                .await
                .expect("queues");
        }
        assert_eq!(client.pending_send_count().await, PENDING_SEND_QUEUE_CAP);
        assert_eq!(client.dropped_send_count().await, 25);

        // The survivors are the newest ones: a stale presence beacon is worthless.
        let handle = tokio::spawn(Arc::clone(&client).run());
        tokio::time::sleep(Duration::from_millis(30)).await;
        let sent = relay.sent_frames();
        assert_eq!(sent.len(), PENDING_SEND_QUEUE_CAP);
        assert!(
            sent.first().is_some_and(|f| f.contains("\"0025\"")),
            "{:?}",
            sent.first()
        );
        assert!(
            sent.last().is_some_and(|f| f.contains("\"0224\"")),
            "{:?}",
            sent.last()
        );

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn a_failed_write_requeues_and_marks_the_socket_dead() {
        let (client, mut rx) = client(Arc::new(BrokenSinkRelay));
        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        client.publish(&event("aa")).await.expect("queues");
        assert_eq!(client.pending_send_count().await, 1);
        assert!(!client.is_connected().await);

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn reconnects_after_a_drop_and_replays_the_subscription() {
        let relay = MockRelay::new(vec![Script::ServeThenClose(vec![]), Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));
        client
            .subscribe("sub-1", &[RelayFilter::default()])
            .await
            .expect("subscribes");

        let handle = tokio::spawn(Arc::clone(&client).run());

        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );
        match next_event(&mut rx).await {
            RelayEvent::Disconnected { .. } => {}
            other => panic!("expected a disconnect, got {other:?}"),
        }
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(relay.connects(), 2);
        // Replayed once per connection: a relay forgets subscriptions with the socket.
        assert_eq!(relay.sent_frames().len(), 2);
        assert_eq!(
            client.reconnect_attempts().await,
            0,
            "a success resets the counter"
        );

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn gives_up_after_the_maximum_attempts() {
        let relay = MockRelay::new((0..40).map(|_| Script::Fail("refused")).collect());
        let (client, mut rx) = client(Arc::clone(&relay));
        let handle = tokio::spawn(Arc::clone(&client).run());

        let mut disconnects = 0;
        loop {
            match next_event(&mut rx).await {
                RelayEvent::Disconnected { reason, .. } => {
                    assert!(reason.contains("connect failed"), "{reason}");
                    disconnects += 1;
                }
                RelayEvent::GaveUp { url } => {
                    assert_eq!(url, "wss://relay.test");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(disconnects, MAX_RECONNECT_ATTEMPTS as usize);
        assert_eq!(relay.connects(), MAX_RECONNECT_ATTEMPTS as usize);

        // The loop is over: the task ends on its own.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run returns")
            .expect("no panic");
    }

    #[tokio::test]
    async fn malformed_frames_are_counted_and_the_connection_survives() {
        let relay = MockRelay::new(vec![Script::Serve(vec![
            "not json".to_string(),
            r#"["EVENT","sub"]"#.to_string(),
            r#"["GARBAGE"]"#.to_string(),
        ])]);
        let (client, mut rx) = client(Arc::clone(&relay));
        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(client.malformed_frame_count().await, 3);
        assert!(
            client.is_connected().await,
            "one bad frame must not kill the socket"
        );

        // Still working afterwards.
        relay.push(r#"["EOSE","sub-1"]"#);
        match next_event(&mut rx).await {
            RelayEvent::Eose { .. } => {}
            other => panic!("expected EOSE, got {other:?}"),
        }

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn a_relay_closing_a_subscription_stops_us_replaying_it() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));
        client
            .subscribe("sub-1", &[RelayFilter::default()])
            .await
            .expect("subscribes");
        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        relay.push(r#"["CLOSED","sub-1","auth-required: restricted"]"#);
        match next_event(&mut rx).await {
            RelayEvent::Closed {
                subscription_id, ..
            } => assert_eq!(subscription_id, "sub-1"),
            other => panic!("expected CLOSED, got {other:?}"),
        }
        assert!(client.subscription_ids().await.is_empty());

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn unsubscribing_sends_close_and_forgets_the_subscription() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));
        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        client
            .subscribe("sub-1", &[RelayFilter::default()])
            .await
            .expect("subscribes");
        client.unsubscribe("sub-1").await;
        assert!(client.subscription_ids().await.is_empty());

        tokio::time::sleep(Duration::from_millis(20)).await;
        let sent = relay.sent_frames();
        assert!(sent.iter().any(|f| f == r#"["CLOSE","sub-1"]"#), "{sent:?}");

        // Unsubscribing twice is a no-op, not a second CLOSE.
        client.unsubscribe("sub-1").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            relay
                .sent_frames()
                .iter()
                .filter(|f| f.starts_with(r#"["CLOSE""#))
                .count(),
            1
        );

        client.shutdown().await;
        handle.abort();
    }

    #[tokio::test]
    async fn a_subscription_needs_at_least_one_filter() {
        let relay = MockRelay::new(vec![]);
        let (client, _rx) = client(Arc::clone(&relay));
        assert!(client.subscribe("sub-1", &[]).await.is_err());
        assert!(client.subscription_ids().await.is_empty());
    }

    #[tokio::test]
    async fn shutdown_stops_the_loop() {
        let relay = MockRelay::new(vec![Script::Serve(vec![])]);
        let (client, mut rx) = client(Arc::clone(&relay));
        let handle = tokio::spawn(Arc::clone(&client).run());
        assert_eq!(
            next_event(&mut rx).await,
            RelayEvent::Connected {
                url: "wss://relay.test".to_string()
            }
        );

        client.shutdown().await;
        relay.drop_connection();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run returns after shutdown")
            .expect("no panic");
        assert_eq!(relay.connects(), 1, "no reconnect after shutdown");
    }
}
