//! Who we connect to, and how fast.
//!
//! A port of bitchat's `BLEConnectionScheduler.swift`. Every decision here exists
//! because the radio is a scarce, shared, easily-starved resource:
//!
//! - **A budget.** At most [`MAX_CENTRAL_LINKS`] outbound links. The controller
//!   cannot hold many more, and the ones it does hold get slower as the count rises.
//! - **A rate limit.** One connect attempt per [`CONNECT_RATE_LIMIT_MS`], globally.
//!   In a crowded room every advert would otherwise trigger an immediate connect and
//!   the resulting storm starves the adapter for everything else, including the
//!   connects that would have succeeded.
//! - **An RSSI gate**, which moves. A device with peers can afford to be picky; a
//!   device with none must reach as far as the radio physically allows, or it never
//!   joins the mesh at all. See [`ConnectionScheduler::update_rssi_threshold`].
//! - **Per-peer backoff.** A peer that never answered a connect attempt is treated
//!   differently from one whose established link dropped — the first probably is not
//!   reachable, the second probably walked around a corner and will be back.
//!
//! Sans-IO throughout: time arrives as `now_unix_millis` and nothing here talks to a
//! radio. Swift's `TimeInterval` seconds become `u64` milliseconds; every returned
//! delay is milliseconds.

use std::collections::BTreeMap;

mod backoff;
mod discovery;
mod queue;
mod rssi;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Constants — `bitchat/Services/TransportConfig.swift`
// ---------------------------------------------------------------------------

/// `TransportConfig.bleMaxCentralLinks` — outbound link budget.
pub const MAX_CENTRAL_LINKS: usize = 6;

/// `TransportConfig.bleConnectRateLimitInterval` (0.5 s) — global spacing between
/// connect attempts. The single most important line of defence against a connect
/// storm in a crowded room.
pub const CONNECT_RATE_LIMIT_MS: u64 = 500;

/// `TransportConfig.bleConnectionCandidatesMax` — hard cap on the queue of devices
/// waiting for a slot. Beyond this the weakest are dropped.
pub const CONNECTION_CANDIDATES_MAX: usize = 100;

/// `TransportConfig.bleDynamicRSSIThresholdDefault` (−90 dBm) — the gate when we
/// have peers and spare capacity.
pub const DYNAMIC_RSSI_THRESHOLD_DEFAULT: i16 = -90;

/// `TransportConfig.bleRSSIIsolatedBase` (−95 dBm) — the gate while we have no
/// connected peers at all. A fringe link beats no link.
pub const RSSI_ISOLATED_BASE: i16 = -95;

/// `TransportConfig.bleRSSIIsolatedRelaxed` (−100 dBm) — after prolonged isolation,
/// the floor drops to the practical reporting limit of the radio: at that point the
/// only thing gating a connect is whether the advert decoded at all.
pub const RSSI_ISOLATED_RELAXED: i16 = -100;

/// `TransportConfig.bleRSSIConnectedThreshold` (−85 dBm) — the gate once the budget
/// or the candidate queue is full and a new link would have to displace something.
pub const RSSI_CONNECTED_THRESHOLD: i16 = -85;

/// `TransportConfig.bleIsolationRelaxThresholdSeconds` (30 s) — how long we must be
/// alone before the gate relaxes from [`RSSI_ISOLATED_BASE`] to
/// [`RSSI_ISOLATED_RELAXED`]. Compared with `>`, so exactly 30 s is still base.
pub const ISOLATION_RELAX_THRESHOLD_MS: u64 = 30_000;

/// `TransportConfig.bleWeakLinkCooldownSeconds` (30 s) — how long a peer that timed
/// out stays in weak-link cooldown, during which a weak advert from it is deferred
/// rather than acted on.
pub const WEAK_LINK_COOLDOWN_MS: u64 = 30_000;

/// `TransportConfig.bleWeakLinkRSSICutoff` (−90 dBm) — cooldown only applies to
/// adverts at or below this. A peer that timed out but is now loud gets another go
/// immediately; it probably just moved closer.
pub const WEAK_LINK_RSSI_CUTOFF: i16 = -90;

/// `TransportConfig.bleTimeoutDiscoveryIgnoreSeconds` (15 s) — rediscovery of a peer
/// whose connect attempt timed out is ignored for this long.
pub const TIMEOUT_DISCOVERY_IGNORE_MS: u64 = 15_000;

/// `TransportConfig.bleDisconnectDiscoveryIgnoreSeconds` (3 s) — a peer whose
/// established link dropped usually comes back, so it only gets long enough for the
/// stack to settle, not the timeout treatment.
pub const DISCONNECT_DISCOVERY_IGNORE_MS: u64 = 3_000;

/// `TransportConfig.bleConnectTimeoutBackoffWindowSeconds` (120 s) — age at which
/// per-peer backoff bookkeeping is garbage-collected. See
/// [`ConnectionScheduler::prune_backoff`].
pub const CONNECT_TIMEOUT_BACKOFF_WINDOW_MS: u64 = 120_000;

/// `TransportConfig.bleConnectTimeoutSeconds` (8 s) — how long the driver should
/// wait for a connect to complete before calling
/// [`ConnectionScheduler::note_connect_timeout`]. Not consumed here; the timer lives
/// in the radio layer, but the value belongs with the rest of the connect policy.
pub const CONNECT_TIMEOUT_MS: u64 = 8_000;

/// Literal `2.0` in `handleDiscovery` — a peripheral we attempted this recently is
/// left alone, whatever the link store thinks its state is.
pub const RECENT_ATTEMPT_IGNORE_MS: u64 = 2_000;

/// Minimum gap between connect attempts to the same address after one has
/// *failed outright*, as opposed to timing out.
///
/// # A deliberate divergence from bitchat-iOS
///
/// An address that turned out to be a peer we already hold a link to is ignored
/// for this long.
///
/// Far longer than the other windows because it is not a backoff — nothing failed.
/// The address is a rotation of a peer already reachable, so dialling it again buys
/// a second ACL to the same phone and costs a central slot, a GATT server the peer
/// has to keep serving, and on a desktop a connect/disconnect notification per
/// round. Five minutes outlasts the observed rotation cadence, so the address is
/// stale long before the window reopens.
///
/// Safe only because of two invariants elsewhere, and it becomes an availability
/// bug if either changes.
///
/// A second central link to the *same* address is impossible — `central.rs`
/// hands back the existing link rather than dialling again — so a duplicate can
/// only arise from two distinct addresses, which means the peer rotated. And the
/// prune keeps the *newest* link, so the address marked here is always the older
/// one the peer has already moved off. The surviving link's address is never
/// marked.
///
/// Reverse the keeper rule to "oldest" and this map starts blacklisting the
/// address the peer is currently reachable on: if the kept link then dies, it
/// cannot be redialled for five minutes even though the peer is advertising
/// steadily. That is the failure this note exists to prevent.
pub const DUPLICATE_PEER_IGNORE_MS: u64 = 300_000;

/// iOS has no such window and no attempt cap: [`RECENT_ATTEMPT_IGNORE_MS`] (its
/// literal `2.0`) is the only spacing it applies, because CoreBluetooth's
/// `connect` queues indefinitely rather than failing. BlueZ does fail, promptly
/// and repeatedly — `le-connection-abort-by-local` — and 2 s of spacing with no
/// cap produced an observed seven consecutive failures to one address in 19 s,
/// which hammering `connect` while also scanning and advertising can itself
/// cause.
///
/// bitchat's *Android* client hit the same wall on a stack that behaves like
/// BlueZ and answered it with `CONNECTION_RETRY_DELAY = 5s` and
/// `MAX_CONNECTION_ATTEMPTS = 3`, keeping the attempt record on failure
/// specifically so the cap bites (`MeshConnectionTracker.kt:23-40`, and the
/// commented-out `removePendingConnection` calls in
/// `BluetoothGattClientManager` that say so). Those are the numbers used here.
pub const CONNECT_FAILURE_RETRY_DELAY_MS: u64 = 5_000;

/// Consecutive outright failures to one address before it is left alone until its
/// record expires. From `MeshConnectionTracker.MAX_CONNECTION_ATTEMPTS`.
pub const MAX_CONNECT_FAILURES: u32 = 3;

/// A failure record older than this is forgotten and the count starts again, so a
/// peer that was simply out of range earlier is not penalised forever. Twice the
/// retry delay, matching `ConnectionAttempt.isExpired`.
pub const CONNECT_FAILURE_FORGET_MS: u64 = CONNECT_FAILURE_RETRY_DELAY_MS * 2;

/// Literal `0.05` in `rateLimitRetryDelay` — slack added so the retry lands *after*
/// the limit expires rather than exactly on it and bouncing again.
pub const RATE_LIMIT_RETRY_SLACK_MS: u64 = 50;

/// Literal `0.05` in `disconnectSettleDelay`, for the same reason.
pub const DISCONNECT_SETTLE_SLACK_MS: u64 = 50;

/// Literal `2.0` lower clamp in `weakLinkRetryDelay`.
pub const WEAK_LINK_RETRY_MIN_MS: u64 = 2_000;

/// Literal `15.0` upper clamp in `weakLinkRetryDelay`.
pub const WEAK_LINK_RETRY_MAX_MS: u64 = 15_000;

/// Literal `60` in `score` — a timeout this recent biases the candidate down.
pub const TIMEOUT_SCORE_BIAS_WINDOW_MS: u64 = 60_000;

/// Literal `10` in `score` — the size of that bias.
pub const TIMEOUT_SCORE_BIAS: i64 = 10;

/// Literal `20` in `score` — cap on the exponential failure penalty. The exponent is
/// itself capped at 4 (so 16), which means this never actually binds; ported anyway
/// so the shape survives a future change to either cap.
pub const FAILURE_PENALTY_CAP: i64 = 20;

/// Literal `4` in `score` — cap on the failure-count exponent.
pub const FAILURE_PENALTY_MAX_SHIFT: u32 = 4;

/// Literal `1000` in `score` — connectable adverts outrank unconnectable ones by
/// more than any RSSI difference can make up.
pub const CONNECTABLE_SCORE_BONUS: i64 = 1_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A device we could connect to, as last advertised.
///
/// `address` is whatever the radio calls the device — a MAC on BlueZ, a UUID on
/// CoreBluetooth. It is emphatically *not* a peer identity: identity arrives later,
/// signed, over the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionCandidate {
    pub address: String,
    pub rssi: i16,
    /// Whether the advert claimed to accept connections. Adverts that do not are
    /// scored to the bottom and skipped when drained.
    pub is_connectable: bool,
    pub discovered_at_ms: u64,
}

impl ConnectionCandidate {
    /// A connectable candidate — the ordinary case, and what a
    /// `LinkEvent::Discovered` implies.
    pub fn new(address: impl Into<String>, rssi: i16, discovered_at_ms: u64) -> Self {
        Self {
            address: address.into(),
            rssi,
            is_connectable: true,
            discovered_at_ms,
        }
    }

    /// Override connectability, for backends that report it.
    #[must_use]
    pub fn with_connectable(mut self, is_connectable: bool) -> Self {
        self.is_connectable = is_connectable;
        self
    }
}

/// What our own link store already believes about this device.
///
/// Supplied by the caller rather than tracked here: the scheduler decides policy,
/// the registry owns link state, and duplicating it would mean two sources of truth
/// that drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ExistingConnectionState {
    pub is_connecting: bool,
    pub is_connected: bool,
    pub last_connect_attempt_ms: Option<u64>,
}

/// What the *radio* believes about this device, which is not always the same thing —
/// a stale `Connecting` with no link-store entry is exactly the case
/// [`DiscoveryDecision::CancelStaleConnection`] exists for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeripheralConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

/// What to do about an advert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryDecision {
    /// Drop it. Not queued, not retried.
    Ignore,
    /// Kept as a candidate for later. Nothing to do now; the queue is drained by
    /// [`ConnectionScheduler::next_candidate`].
    Queued,
    /// Queued, and the driver should drain the queue again after this delay.
    ScheduleRetry { after_ms: u64 },
    /// The radio thinks it is connected or connecting but we hold no link. Cancel it
    /// so the next advert can start cleanly.
    CancelStaleConnection,
    /// Connect to this device now, and call
    /// [`ConnectionScheduler::note_connect_attempt`] when you do.
    ConnectNow,
}

/// The result of draining the candidate queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueDecision {
    /// Nothing to do — no capacity, or nothing worth connecting to.
    Idle,
    /// Something is waiting but not yet eligible. Drain again after this delay.
    RetryAfter { after_ms: u64 },
    /// Connect to this candidate. It has been removed from the queue.
    Connect(ConnectionCandidate),
}

/// Tunables, all defaulting to the ported bitchat values. Present because the Swift
/// initialiser takes them, and because tests want smaller caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub max_central_links: usize,
    pub connect_rate_limit_ms: u64,
    pub candidate_cap: usize,
    pub weak_link_cooldown_ms: u64,
    pub weak_link_rssi_cutoff: i16,
    /// Starting value of the dynamic gate, and what [`ConnectionScheduler::reset`]
    /// restores.
    pub initial_rssi_threshold: i16,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_central_links: MAX_CENTRAL_LINKS,
            connect_rate_limit_ms: CONNECT_RATE_LIMIT_MS,
            candidate_cap: CONNECTION_CANDIDATES_MAX,
            weak_link_cooldown_ms: WEAK_LINK_COOLDOWN_MS,
            weak_link_rssi_cutoff: WEAK_LINK_RSSI_CUTOFF,
            initial_rssi_threshold: DYNAMIC_RSSI_THRESHOLD_DEFAULT,
        }
    }
}

/// Connection policy for the central role.
///
/// Holds the candidate queue, the global rate-limit clock, per-peer failure history
/// and the dynamic RSSI gate. Drive it from the radio's event loop:
///
/// 1. `LinkEvent::Discovered` → [`handle_discovery`](Self::handle_discovery)
/// 2. Anything that frees a slot (connect, disconnect, failure, timeout, or just the
///    maintenance tick) → [`next_candidate`](Self::next_candidate)
/// 3. Actually issuing a connect → [`note_connect_attempt`](Self::note_connect_attempt)
/// 4. Periodically → [`update_rssi_threshold`](Self::update_rssi_threshold) and
///    [`prune_backoff`](Self::prune_backoff)
#[derive(Clone, Debug)]
pub struct ConnectionScheduler {
    config: SchedulerConfig,
    candidates: Vec<ConnectionCandidate>,
    /// `None` until the first attempt — Swift's `.distantPast`, which is never
    /// within the rate-limit window.
    last_connect_attempt_ms: Option<u64>,
    failure_counts: BTreeMap<String, u32>,
    recent_connect_timeouts: BTreeMap<String, u64>,
    /// Consecutive outright connect failures per address: `(count, last_ms)`.
    /// See [`CONNECT_FAILURE_RETRY_DELAY_MS`] for why this exists on BlueZ when
    /// it does not on iOS.
    recent_connect_failures: BTreeMap<String, (u32, u64)>,
    /// Tracked apart from timeouts on purpose: a peer we held a link with and lost
    /// usually returns, so it gets a brief settle window rather than backoff.
    recent_disconnects: BTreeMap<String, u64>,
    /// Addresses found to be a rotation of a peer we already hold. Not a failure
    /// record: see [`DUPLICATE_PEER_IGNORE_MS`].
    duplicate_peers: BTreeMap<String, u64>,
    /// When the current run of isolation began. `None` whenever we have peers.
    isolated_since_ms: Option<u64>,
    dynamic_rssi_threshold: i16,
}

impl Default for ConnectionScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionScheduler {
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    pub fn with_config(config: SchedulerConfig) -> Self {
        Self {
            candidates: Vec::new(),
            last_connect_attempt_ms: None,
            failure_counts: BTreeMap::new(),
            recent_connect_timeouts: BTreeMap::new(),
            recent_connect_failures: BTreeMap::new(),
            recent_disconnects: BTreeMap::new(),
            duplicate_peers: BTreeMap::new(),
            isolated_since_ms: None,
            dynamic_rssi_threshold: config.initial_rssi_threshold,
            config,
        }
    }

    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    /// How many devices are waiting for a slot.
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// The RSSI gate as it currently stands. Adverts at or below this are queued
    /// rather than connected to. Moved by
    /// [`update_rssi_threshold`](Self::update_rssi_threshold).
    pub fn rssi_threshold(&self) -> i16 {
        self.dynamic_rssi_threshold
    }

    /// Consecutive connect failures recorded for a device.
    pub fn failure_count(&self, address: &str) -> u32 {
        self.failure_counts.get(address).copied().unwrap_or(0)
    }

    /// Number of per-peer backoff records held (timeouts plus disconnects).
    /// Bounded by [`prune_backoff`](Self::prune_backoff).
    pub fn backoff_len(&self) -> usize {
        self.recent_connect_timeouts.len() + self.recent_disconnects.len()
    }

    /// Forget everything. For adapter loss or a full restart, where none of the
    /// history describes the world any more.
    pub fn reset(&mut self) {
        self.last_connect_attempt_ms = None;
        self.candidates.clear();
        self.failure_counts.clear();
        self.recent_connect_timeouts.clear();
        self.recent_connect_failures.clear();
        self.recent_disconnects.clear();
        self.duplicate_peers.clear();
        self.isolated_since_ms = None;
        self.dynamic_rssi_threshold = self.config.initial_rssi_threshold;
    }
}
