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
    /// Tracked apart from timeouts on purpose: a peer we held a link with and lost
    /// usually returns, so it gets a brief settle window rather than backoff.
    recent_disconnects: BTreeMap<String, u64>,
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
            recent_disconnects: BTreeMap::new(),
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

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    /// Decide what an advert is worth.
    ///
    /// The order of the checks is load-bearing. RSSI and budget are tested *before*
    /// link state, so a weak advert for a device we are already connected to is
    /// queued rather than ignored — which is why
    /// [`next_candidate`](Self::next_candidate) has to re-check the disconnect settle
    /// window when it drains.
    pub fn handle_discovery(
        &mut self,
        candidate: &ConnectionCandidate,
        connected_or_connecting: usize,
        existing: Option<ExistingConnectionState>,
        peripheral_state: PeripheralConnectionState,
        now_ms: u64,
    ) -> DiscoveryDecision {
        if !candidate.is_connectable {
            return DiscoveryDecision::Ignore;
        }

        if candidate.rssi <= self.dynamic_rssi_threshold {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::Queued;
        }

        if connected_or_connecting >= self.config.max_central_links {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::Queued;
        }

        if let Some(after_ms) = self.rate_limit_retry_delay(now_ms) {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::ScheduleRetry { after_ms };
        }

        if let Some(existing) = existing {
            if existing.is_connected || existing.is_connecting {
                return DiscoveryDecision::Ignore;
            }
            if let Some(last_attempt) = existing.last_connect_attempt_ms {
                if now_ms.saturating_sub(last_attempt) < RECENT_ATTEMPT_IGNORE_MS {
                    return DiscoveryDecision::Ignore;
                }
            }
        }

        if let Some(&last_timeout) = self.recent_connect_timeouts.get(&candidate.address) {
            if now_ms.saturating_sub(last_timeout) < TIMEOUT_DISCOVERY_IGNORE_MS {
                return DiscoveryDecision::Ignore;
            }
        }

        if let Some(&last_disconnect) = self.recent_disconnects.get(&candidate.address) {
            if now_ms.saturating_sub(last_disconnect) < DISCONNECT_DISCOVERY_IGNORE_MS {
                return DiscoveryDecision::Ignore;
            }
        }

        match peripheral_state {
            PeripheralConnectionState::Disconnected => DiscoveryDecision::ConnectNow,
            PeripheralConnectionState::Connecting | PeripheralConnectionState::Connected => {
                DiscoveryDecision::CancelStaleConnection
            }
        }
    }

    /// Shorthand over [`handle_discovery`](Self::handle_discovery) for the common
    /// radio-layer case: a `LinkEvent::Discovered` for an address we hold no link to.
    ///
    /// Returns `true` when the radio should issue a connect right now. Every other
    /// outcome (queued, rate-limited, backed off) returns `false` and is handled by
    /// the next [`next_candidate`](Self::next_candidate) drain — so a `false` here is
    /// never a dropped opportunity, just a deferred one.
    pub fn should_connect(
        &mut self,
        address: &str,
        rssi: i16,
        connected_or_connecting: usize,
        now_ms: u64,
    ) -> bool {
        let candidate = ConnectionCandidate::new(address, rssi, now_ms);
        let decision = self.handle_discovery(
            &candidate,
            connected_or_connecting,
            None,
            PeripheralConnectionState::Disconnected,
            now_ms,
        );
        decision == DiscoveryDecision::ConnectNow
    }

    /// Add or refresh a candidate, keeping the queue sorted and capped.
    ///
    /// Sort is by signal strength, ties to whoever we saw first; the overflow is
    /// dropped from the weak end. Address is the final tiebreak so the queue is
    /// deterministic — Swift's sort is unstable and leaves exact ties in arbitrary
    /// order, which is untestable and pointless to reproduce.
    pub fn enqueue(&mut self, candidate: ConnectionCandidate) {
        match self
            .candidates
            .iter_mut()
            .find(|existing| existing.address == candidate.address)
        {
            Some(existing) => *existing = candidate,
            None => self.candidates.push(candidate),
        }

        self.candidates.sort_by(|a, b| {
            b.rssi
                .cmp(&a.rssi)
                .then(a.discovered_at_ms.cmp(&b.discovered_at_ms))
                .then(a.address.cmp(&b.address))
        });
        self.candidates.truncate(self.config.candidate_cap);
    }

    /// Drop a candidate outright, e.g. once a link to it is established by the
    /// peripheral role and a second outbound link would be redundant.
    pub fn remove_candidate(&mut self, address: &str) {
        self.candidates.retain(|c| c.address != address);
    }

    // -----------------------------------------------------------------------
    // Draining the queue
    // -----------------------------------------------------------------------

    /// Pick the next device to connect to, if any.
    ///
    /// `is_busy` answers "am I already connected or connecting to this address?" —
    /// the link registry knows, the scheduler does not.
    ///
    /// Candidates are ranked by [`Self::score`]-equivalent ordering: signal first,
    /// discounted by how stale the advert is and by how badly this peer has behaved.
    pub fn next_candidate(
        &mut self,
        connected_or_connecting: usize,
        is_busy: impl Fn(&str) -> bool,
        now_ms: u64,
    ) -> QueueDecision {
        if connected_or_connecting >= self.config.max_central_links {
            return QueueDecision::Idle;
        }

        if let Some(after_ms) = self.rate_limit_retry_delay(now_ms) {
            return QueueDecision::RetryAfter { after_ms };
        }

        // Scoring depends only on `now_ms` and on state that cannot change inside
        // this loop, so one sort is equivalent to Swift's re-sort per iteration.
        self.sort_by_score(now_ms);

        while !self.candidates.is_empty() {
            let candidate = self.candidates.remove(0);

            if !candidate.is_connectable {
                continue;
            }

            if let Some(after_ms) = self.weak_link_retry_delay(&candidate, now_ms) {
                self.enqueue(candidate);
                return QueueDecision::RetryAfter { after_ms };
            }

            if let Some(after_ms) = self.disconnect_settle_delay(&candidate, now_ms) {
                self.enqueue(candidate);
                return QueueDecision::RetryAfter { after_ms };
            }

            if is_busy(&candidate.address) {
                continue;
            }

            return QueueDecision::Connect(candidate);
        }

        QueueDecision::Idle
    }

    // -----------------------------------------------------------------------
    // Outcomes
    // -----------------------------------------------------------------------

    /// Record that a connect was just issued. This is what the global rate limit
    /// measures from, so it must be called for *every* attempt, whatever its origin.
    pub fn note_connect_attempt(&mut self, now_ms: u64) {
        self.last_connect_attempt_ms = Some(now_ms);
    }

    /// A link came up. Clears every form of backoff for that address.
    pub fn note_connected(&mut self, address: &str) {
        self.failure_counts.remove(address);
        self.recent_connect_timeouts.remove(address);
        self.recent_disconnects.remove(address);
    }

    /// A connect attempt failed outright (as opposed to timing out). Counts against
    /// the candidate's score but imposes no ignore window.
    pub fn note_connect_failed(&mut self, address: &str) {
        *self.failure_counts.entry(address.to_string()).or_insert(0) += 1;
    }

    /// An established link dropped. Earns the short settle window, not the timeout
    /// backoff — the peer probably walked around a corner and will be back.
    pub fn note_disconnected(&mut self, address: &str, now_ms: u64) {
        self.recent_disconnects.insert(address.to_string(), now_ms);
    }

    /// A connect attempt ran past [`CONNECT_TIMEOUT_MS`] without completing. Earns
    /// both the long ignore window and a failure count.
    pub fn note_connect_timeout(&mut self, address: &str, now_ms: u64) {
        self.recent_connect_timeouts
            .insert(address.to_string(), now_ms);
        self.note_connect_failed(address);
    }

    /// Garbage-collect per-peer backoff older than
    /// [`CONNECT_TIMEOUT_BACKOFF_WINDOW_MS`]. Call it on the maintenance tick.
    ///
    /// Failure counts for peers with no surviving record and no queued candidate go
    /// too. bitchat keeps those forever, which leaks slowly but genuinely against
    /// rotating BLE addresses; the penalty they carry is at most 16 points, under
    /// 8 dBm of equivalent signal, so forgetting it past the backoff window costs
    /// nothing and bounds the map.
    pub fn prune_backoff(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(CONNECT_TIMEOUT_BACKOFF_WINDOW_MS);
        self.recent_connect_timeouts.retain(|_, &mut at| at >= cutoff);
        self.recent_disconnects.retain(|_, &mut at| at >= cutoff);

        let timeouts = &self.recent_connect_timeouts;
        let disconnects = &self.recent_disconnects;
        let candidates = &self.candidates;
        self.failure_counts.retain(|address, _| {
            timeouts.contains_key(address)
                || disconnects.contains_key(address)
                || candidates.iter().any(|c| &c.address == address)
        });
    }

    /// Forget everything. For adapter loss or a full restart, where none of the
    /// history describes the world any more.
    pub fn reset(&mut self) {
        self.last_connect_attempt_ms = None;
        self.candidates.clear();
        self.failure_counts.clear();
        self.recent_connect_timeouts.clear();
        self.recent_disconnects.clear();
        self.isolated_since_ms = None;
        self.dynamic_rssi_threshold = self.config.initial_rssi_threshold;
    }

    // -----------------------------------------------------------------------
    // The moving RSSI gate
    // -----------------------------------------------------------------------

    /// Recompute the RSSI gate and return it.
    ///
    /// - **No connected peers:** [`RSSI_ISOLATED_BASE`], relaxing to
    ///   [`RSSI_ISOLATED_RELAXED`] once we have been alone for longer than
    ///   [`ISOLATION_RELAX_THRESHOLD_MS`]. This is the part that matters: a node with
    ///   nobody in earshot has to reach further or it never joins the mesh at all,
    ///   and a fringe link it can barely hold still beats no link.
    /// - **Peers, and capacity to spare:** [`DYNAMIC_RSSI_THRESHOLD_DEFAULT`].
    /// - **Peers, but the budget or candidate queue is full:**
    ///   [`RSSI_CONNECTED_THRESHOLD`] — only a clearly better link is worth the churn.
    ///
    /// Note what it deliberately does *not* do: repeated timeouts never tighten the
    /// gate. Flaky peers are punished individually, because one distant flapping
    /// device must not blind us to every other edge-of-range peer.
    ///
    /// `connected_count` is peers with a live link; `connected_or_connecting_links`
    /// counts outbound links against the budget, including in-flight connects.
    pub fn update_rssi_threshold(
        &mut self,
        connected_count: usize,
        connected_or_connecting_links: usize,
        now_ms: u64,
    ) -> i16 {
        if connected_count == 0 {
            let isolated_since = *self.isolated_since_ms.get_or_insert(now_ms);
            let elapsed = now_ms.saturating_sub(isolated_since);
            self.dynamic_rssi_threshold = if elapsed > ISOLATION_RELAX_THRESHOLD_MS {
                RSSI_ISOLATED_RELAXED
            } else {
                RSSI_ISOLATED_BASE
            };
            return self.dynamic_rssi_threshold;
        }

        self.isolated_since_ms = None;

        let saturated = connected_or_connecting_links >= self.config.max_central_links
            || self.candidates.len() >= self.config.candidate_cap;
        self.dynamic_rssi_threshold = if saturated {
            RSSI_CONNECTED_THRESHOLD
        } else {
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        };
        self.dynamic_rssi_threshold
    }

    /// How long we have been without a peer, if we are currently isolated.
    pub fn isolated_for_ms(&self, now_ms: u64) -> Option<u64> {
        self.isolated_since_ms
            .map(|since| now_ms.saturating_sub(since))
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    /// `Some(delay)` while the global connect rate limit is in force.
    fn rate_limit_retry_delay(&self, now_ms: u64) -> Option<u64> {
        let last = self.last_connect_attempt_ms?;
        let elapsed = now_ms.saturating_sub(last);
        if elapsed >= self.config.connect_rate_limit_ms {
            return None;
        }
        Some(self.config.connect_rate_limit_ms - elapsed + RATE_LIMIT_RETRY_SLACK_MS)
    }

    /// `Some(delay)` when this candidate timed out recently *and* is still weak.
    /// A peer that timed out but is now loud gets another go immediately.
    fn weak_link_retry_delay(&self, candidate: &ConnectionCandidate, now_ms: u64) -> Option<u64> {
        let last_timeout = *self.recent_connect_timeouts.get(&candidate.address)?;
        let elapsed = now_ms.saturating_sub(last_timeout);
        if elapsed >= self.config.weak_link_cooldown_ms
            || candidate.rssi > self.config.weak_link_rssi_cutoff
        {
            return None;
        }
        let remaining = self.config.weak_link_cooldown_ms - elapsed;
        Some(remaining.clamp(WEAK_LINK_RETRY_MIN_MS, WEAK_LINK_RETRY_MAX_MS))
    }

    /// `Some(delay)` while this candidate is inside its post-disconnect settle window.
    ///
    /// This has to hold on the drain path too, not just on discovery: a candidate
    /// queued while the device was still connected would otherwise reconnect the
    /// instant the link dropped, bypassing the window and recreating the exact
    /// reconnect/cancel thrash it exists to prevent.
    fn disconnect_settle_delay(
        &self,
        candidate: &ConnectionCandidate,
        now_ms: u64,
    ) -> Option<u64> {
        let last_disconnect = *self.recent_disconnects.get(&candidate.address)?;
        let elapsed = now_ms.saturating_sub(last_disconnect);
        if elapsed >= DISCONNECT_DISCOVERY_IGNORE_MS {
            return None;
        }
        Some(DISCONNECT_DISCOVERY_IGNORE_MS - elapsed + DISCONNECT_SETTLE_SLACK_MS)
    }

    /// Best first. Ties break on the same keys [`Self::enqueue`] uses, for
    /// determinism.
    fn sort_by_score(&mut self, now_ms: u64) {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.sort_by(|a, b| {
            self.score(b, now_ms)
                .cmp(&self.score(a, now_ms))
                .then(a.discovered_at_ms.cmp(&b.discovered_at_ms))
                .then(a.address.cmp(&b.address))
        });
        self.candidates = candidates;
    }

    /// Rank a candidate: connectable and loud is good, stale and flaky is not.
    ///
    /// Signal dominates (2 points per dBm), staleness costs 1 point per 100 ms, and
    /// a peer's own history costs at most 26 — deliberately small, so history nudges
    /// the order without ever letting a distant peer beat a close one.
    pub fn score(&self, candidate: &ConnectionCandidate, now_ms: u64) -> i64 {
        let failures = self.failure_count(&candidate.address);
        let shift = failures.min(FAILURE_PENALTY_MAX_SHIFT);
        let penalty = i64::from(1u32 << shift).min(FAILURE_PENALTY_CAP);

        let timeout_bias = match self.recent_connect_timeouts.get(&candidate.address) {
            Some(&at) if now_ms.saturating_sub(at) < TIMEOUT_SCORE_BIAS_WINDOW_MS => {
                TIMEOUT_SCORE_BIAS
            }
            _ => 0,
        };

        let connectable = if candidate.is_connectable {
            CONNECTABLE_SCORE_BONUS
        } else {
            0
        };
        let base = connectable + (i64::from(candidate.rssi) + 100) * 2;

        // Signed on purpose: an advert timestamped slightly ahead of `now_ms` (clock
        // skew between the radio's stamp and ours) scores as fresh rather than
        // saturating to zero age.
        let age_ms = now_ms as i64 - candidate.discovered_at_ms as i64;
        let recency = -(age_ms / 100);

        base + recency - penalty - timeout_bias
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P1: &str = "AA:BB:CC:DD:EE:01";
    const P2: &str = "AA:BB:CC:DD:EE:02";

    fn candidate(address: &str, rssi: i16, now_ms: u64) -> ConnectionCandidate {
        ConnectionCandidate::new(address, rssi, now_ms)
    }

    fn discover(
        scheduler: &mut ConnectionScheduler,
        address: &str,
        rssi: i16,
        now_ms: u64,
    ) -> DiscoveryDecision {
        scheduler.handle_discovery(
            &candidate(address, rssi, now_ms),
            0,
            None,
            PeripheralConnectionState::Disconnected,
            now_ms,
        )
    }

    fn never_busy(_: &str) -> bool {
        false
    }

    // -- RSSI gate on discovery ------------------------------------------------

    #[test]
    fn advert_above_the_gate_connects_and_one_at_it_does_not() {
        let mut s = ConnectionScheduler::new();
        assert_eq!(s.rssi_threshold(), -90);

        // Strictly greater than the threshold is the only thing that connects.
        assert_eq!(discover(&mut s, P1, -89, 1_000), DiscoveryDecision::ConnectNow);
        assert_eq!(s.candidate_count(), 0);

        assert_eq!(discover(&mut s, P2, -90, 1_000), DiscoveryDecision::Queued);
        assert_eq!(s.candidate_count(), 1);

        assert_eq!(discover(&mut s, P2, -91, 1_000), DiscoveryDecision::Queued);
        assert_eq!(s.candidate_count(), 1, "same address replaces, never duplicates");
    }

    #[test]
    fn unconnectable_adverts_are_dropped_not_queued() {
        let mut s = ConnectionScheduler::new();
        let c = candidate(P1, -40, 1_000).with_connectable(false);
        let decision = s.handle_discovery(
            &c,
            0,
            None,
            PeripheralConnectionState::Disconnected,
            1_000,
        );
        assert_eq!(decision, DiscoveryDecision::Ignore);
        assert_eq!(s.candidate_count(), 0);
    }

    // -- Connection budget -----------------------------------------------------

    #[test]
    fn budget_boundary_five_connects_six_and_seven_queue() {
        let mut s = ConnectionScheduler::new();
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 1_000),
                5,
                None,
                PeripheralConnectionState::Disconnected,
                1_000
            ),
            DiscoveryDecision::ConnectNow
        );

        let mut s = ConnectionScheduler::new();
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 1_000),
                6,
                None,
                PeripheralConnectionState::Disconnected,
                1_000
            ),
            DiscoveryDecision::Queued
        );

        let mut s = ConnectionScheduler::new();
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 1_000),
                7,
                None,
                PeripheralConnectionState::Disconnected,
                1_000
            ),
            DiscoveryDecision::Queued
        );
    }

    #[test]
    fn queue_drain_respects_the_budget_boundary() {
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate(P1, -50, 1_000));

        assert_eq!(s.next_candidate(6, never_busy, 2_000), QueueDecision::Idle);
        assert_eq!(s.next_candidate(7, never_busy, 2_000), QueueDecision::Idle);
        assert!(matches!(
            s.next_candidate(5, never_busy, 2_000),
            QueueDecision::Connect(_)
        ));
    }

    // -- Connect rate limit ----------------------------------------------------

    #[test]
    fn rate_limit_boundary_at_499_500_and_501_ms() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_attempt(10_000);

        // 499 ms: still limited, and the retry is scheduled just past expiry.
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 10_499),
                0,
                None,
                PeripheralConnectionState::Disconnected,
                10_499
            ),
            DiscoveryDecision::ScheduleRetry { after_ms: 51 }
        );
        assert_eq!(s.candidate_count(), 1, "rate-limited adverts are kept, not lost");

        // Exactly 500 ms: the limit has expired.
        let mut s = ConnectionScheduler::new();
        s.note_connect_attempt(10_000);
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 10_500),
                0,
                None,
                PeripheralConnectionState::Disconnected,
                10_500
            ),
            DiscoveryDecision::ConnectNow
        );

        // 501 ms.
        let mut s = ConnectionScheduler::new();
        s.note_connect_attempt(10_000);
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 10_501),
                0,
                None,
                PeripheralConnectionState::Disconnected,
                10_501
            ),
            DiscoveryDecision::ConnectNow
        );
    }

    #[test]
    fn rate_limit_delay_shrinks_as_the_window_expires() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_attempt(10_000);
        s.enqueue(candidate(P1, -50, 10_000));

        assert_eq!(
            s.next_candidate(0, never_busy, 10_000),
            QueueDecision::RetryAfter { after_ms: 550 }
        );
        assert_eq!(
            s.next_candidate(0, never_busy, 10_250),
            QueueDecision::RetryAfter { after_ms: 300 }
        );
        assert_eq!(
            s.next_candidate(0, never_busy, 10_499),
            QueueDecision::RetryAfter { after_ms: 51 }
        );
        assert!(matches!(
            s.next_candidate(0, never_busy, 10_500),
            QueueDecision::Connect(_)
        ));
    }

    #[test]
    fn no_attempt_yet_means_no_rate_limit() {
        // Swift seeds this with `.distantPast`; `None` has to behave the same at
        // t=0, which is exactly when a fresh app meets its first advert.
        let mut s = ConnectionScheduler::new();
        assert_eq!(discover(&mut s, P1, -50, 0), DiscoveryDecision::ConnectNow);
    }

    // -- Existing link state ---------------------------------------------------

    #[test]
    fn adverts_for_links_we_already_hold_are_ignored() {
        let mut s = ConnectionScheduler::new();
        for state in [
            ExistingConnectionState {
                is_connected: true,
                ..Default::default()
            },
            ExistingConnectionState {
                is_connecting: true,
                ..Default::default()
            },
        ] {
            assert_eq!(
                s.handle_discovery(
                    &candidate(P1, -50, 1_000),
                    0,
                    Some(state),
                    PeripheralConnectionState::Disconnected,
                    1_000
                ),
                DiscoveryDecision::Ignore
            );
        }
    }

    #[test]
    fn recent_attempt_window_boundary_is_two_seconds() {
        let mut s = ConnectionScheduler::new();
        let state = ExistingConnectionState {
            last_connect_attempt_ms: Some(10_000),
            ..Default::default()
        };

        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 11_999),
                0,
                Some(state),
                PeripheralConnectionState::Disconnected,
                11_999
            ),
            DiscoveryDecision::Ignore
        );
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 12_000),
                0,
                Some(state),
                PeripheralConnectionState::Disconnected,
                12_000
            ),
            DiscoveryDecision::ConnectNow
        );
    }

    #[test]
    fn radio_state_out_of_step_with_ours_cancels_the_stale_connection() {
        let mut s = ConnectionScheduler::new();
        for state in [
            PeripheralConnectionState::Connecting,
            PeripheralConnectionState::Connected,
        ] {
            assert_eq!(
                s.handle_discovery(&candidate(P1, -50, 1_000), 0, None, state, 1_000),
                DiscoveryDecision::CancelStaleConnection
            );
        }
    }

    // -- Per-peer backoff ------------------------------------------------------

    #[test]
    fn connect_timeout_ignore_window_boundary_is_fifteen_seconds() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 10_000);

        assert_eq!(discover(&mut s, P1, -50, 24_999), DiscoveryDecision::Ignore);
        assert_eq!(
            discover(&mut s, P1, -50, 25_000),
            DiscoveryDecision::ConnectNow
        );
    }

    #[test]
    fn disconnect_ignore_window_boundary_is_three_seconds() {
        let mut s = ConnectionScheduler::new();
        s.note_disconnected(P1, 10_000);

        assert_eq!(discover(&mut s, P1, -50, 12_999), DiscoveryDecision::Ignore);
        assert_eq!(
            discover(&mut s, P1, -50, 13_000),
            DiscoveryDecision::ConnectNow
        );
    }

    #[test]
    fn a_successful_connect_clears_every_form_of_backoff() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 10_000);
        s.note_disconnected(P1, 10_000);
        assert_eq!(s.failure_count(P1), 1);
        assert_eq!(s.backoff_len(), 2);

        s.note_connected(P1);

        assert_eq!(s.failure_count(P1), 0);
        assert_eq!(s.backoff_len(), 0);
        assert_eq!(
            discover(&mut s, P1, -50, 10_100),
            DiscoveryDecision::ConnectNow
        );
    }

    #[test]
    fn weak_link_cooldown_defers_a_timed_out_peer_that_is_still_weak() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 10_000);
        s.enqueue(candidate(P1, -95, 10_000));

        // 25 s of cooldown remain; the retry is clamped to the 15 s ceiling.
        assert_eq!(
            s.next_candidate(0, never_busy, 15_000),
            QueueDecision::RetryAfter { after_ms: 15_000 }
        );
        assert_eq!(s.candidate_count(), 1, "deferred candidates are put back");
    }

    #[test]
    fn weak_link_retry_delay_is_clamped_at_both_ends() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 10_000);

        // 29.5 s in: 500 ms nominally remain, raised to the 2 s floor.
        s.enqueue(candidate(P1, -95, 10_000));
        assert_eq!(
            s.next_candidate(0, never_busy, 39_500),
            QueueDecision::RetryAfter { after_ms: 2_000 }
        );

        // Exactly 30 s: the cooldown has expired, so it connects.
        assert!(matches!(
            s.next_candidate(0, never_busy, 40_000),
            QueueDecision::Connect(_)
        ));
    }

    #[test]
    fn a_timed_out_peer_that_got_louder_skips_the_cooldown() {
        // The cutoff is -90; -89 is above it, so the peer is assumed to have moved
        // closer and is worth another attempt immediately.
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 10_000);

        s.enqueue(candidate(P1, -90, 10_000));
        assert_eq!(
            s.next_candidate(0, never_busy, 15_000),
            QueueDecision::RetryAfter { after_ms: 15_000 }
        );

        s.enqueue(candidate(P1, -89, 10_000));
        assert!(matches!(
            s.next_candidate(0, never_busy, 15_000),
            QueueDecision::Connect(_)
        ));
    }

    #[test]
    fn disconnect_settle_window_holds_on_the_queue_drain_path() {
        // A weak advert is queued even while the link is up, so the post-disconnect
        // drain would reconnect instantly if the window were only checked on
        // discovery.
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate(P1, -95, 10_000));
        s.note_disconnected(P1, 10_000);

        assert_eq!(
            s.next_candidate(0, never_busy, 10_100),
            QueueDecision::RetryAfter { after_ms: 2_950 }
        );
        assert_eq!(s.candidate_count(), 1);

        assert!(matches!(
            s.next_candidate(0, never_busy, 13_000),
            QueueDecision::Connect(_)
        ));
    }

    // -- Candidate queue -------------------------------------------------------

    #[test]
    fn enqueue_replaces_rather_than_duplicating() {
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate(P1, -90, 1_000));
        s.enqueue(candidate(P1, -55, 2_000));
        assert_eq!(s.candidate_count(), 1);

        match s.next_candidate(0, never_busy, 3_000) {
            QueueDecision::Connect(c) => {
                assert_eq!(c.address, P1);
                assert_eq!(c.rssi, -55);
            }
            other => panic!("expected a connect, got {other:?}"),
        }
        assert_eq!(s.candidate_count(), 0);
    }

    #[test]
    fn the_candidate_cap_drops_the_weakest() {
        let config = SchedulerConfig {
            candidate_cap: 3,
            ..Default::default()
        };
        let mut s = ConnectionScheduler::with_config(config);
        for (i, rssi) in [-40i16, -50, -60, -70, -80].iter().enumerate() {
            s.enqueue(candidate(&format!("addr-{i}"), *rssi, 1_000));
        }

        assert_eq!(s.candidate_count(), 3);
        let kept: Vec<i16> = (0..3)
            .filter_map(|_| match s.next_candidate(0, never_busy, 1_000) {
                QueueDecision::Connect(c) => Some(c.rssi),
                _ => None,
            })
            .collect();
        assert_eq!(kept, vec![-40, -50, -60]);
    }

    #[test]
    fn the_hundred_candidate_cap_is_the_ported_default() {
        let mut s = ConnectionScheduler::new();
        for i in 0..150 {
            s.enqueue(candidate(&format!("addr-{i:03}"), -60, 1_000));
        }
        assert_eq!(s.candidate_count(), CONNECTION_CANDIDATES_MAX);
    }

    #[test]
    fn strongest_candidate_wins() {
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate("weak", -88, 1_000));
        s.enqueue(candidate("strong", -60, 2_000));

        match s.next_candidate(0, never_busy, 3_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, "strong"),
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_advert_loses_to_a_fresh_one_of_equal_strength() {
        // 1 point per 100 ms of staleness: 10 s of age is 100 points, enough to
        // overcome 2 points per dBm of a 5 dBm advantage.
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate("stale", -60, 0));
        s.enqueue(candidate("fresh", -65, 10_000));

        match s.next_candidate(0, never_busy, 10_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, "fresh"),
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    #[test]
    fn history_nudges_the_order_without_overturning_signal() {
        // Worst-case history is a 16-point failure penalty plus a 10-point timeout
        // bias: 26 points, or 13 dBm of equivalent signal. A peer with a *worse*
        // record still wins when it is clearly closer, which is the point — history
        // breaks near-ties, it does not override the radio.
        let mut s = ConnectionScheduler::new();
        for _ in 0..10 {
            s.note_connect_failed("flaky");
        }
        s.note_connect_timeout("flaky", 0);

        // 10 dBm ahead (20 points) is not enough to carry 26 points of history.
        s.enqueue(candidate("flaky", -60, 1_000));
        s.enqueue(candidate("clean", -70, 1_000));
        match s.next_candidate(0, never_busy, 1_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, "clean"),
            other => panic!("expected a connect, got {other:?}"),
        }

        // 14 dBm ahead (28 points) is.
        s.enqueue(candidate("flaky", -60, 1_000));
        s.enqueue(candidate("clean", -74, 1_000));
        match s.next_candidate(0, never_busy, 1_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, "flaky"),
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    #[test]
    fn failure_penalty_doubles_then_saturates_at_sixteen() {
        // `1 << min(4, failures)`: even a spotless peer carries 1 point, so the
        // deltas from clean are 1, 3, 7, 15 and then flat.
        let mut s = ConnectionScheduler::new();
        let c = candidate(P1, -60, 0);
        let spotless = s.score(&c, 0);
        assert_eq!(spotless, 1_000 + 80 - 1);

        for expected in [1i64, 3, 7, 15, 15, 15, 15] {
            s.note_connect_failed(P1);
            assert_eq!(spotless - s.score(&c, 0), expected);
        }
    }

    #[test]
    fn the_timeout_score_bias_expires_after_sixty_seconds() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 0);
        let c = candidate(P1, -60, 0);

        // The timeout also counted as a failure, so 2 points of penalty are present
        // throughout; the bias is the extra 10.
        assert_eq!(s.score(&c, 59_999), 1_000 + 80 - 599 - 2 - 10);
        assert_eq!(s.score(&c, 60_000), 1_000 + 80 - 600 - 2);
    }

    #[test]
    fn busy_addresses_are_skipped_and_the_next_candidate_is_tried() {
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate("busy", -50, 1_000));
        s.enqueue(candidate("free", -70, 1_000));

        match s.next_candidate(0, |address| address == "busy", 1_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, "free"),
            other => panic!("expected a connect, got {other:?}"),
        }
        assert_eq!(s.candidate_count(), 0, "the busy candidate is discarded too");
    }

    #[test]
    fn an_empty_queue_is_idle() {
        let mut s = ConnectionScheduler::new();
        assert_eq!(s.next_candidate(0, never_busy, 1_000), QueueDecision::Idle);
    }

    #[test]
    fn remove_candidate_drops_a_pending_address() {
        let mut s = ConnectionScheduler::new();
        s.enqueue(candidate(P1, -50, 1_000));
        s.enqueue(candidate(P2, -50, 1_000));
        s.remove_candidate(P1);

        assert_eq!(s.candidate_count(), 1);
        match s.next_candidate(0, never_busy, 1_000) {
            QueueDecision::Connect(c) => assert_eq!(c.address, P2),
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    // -- The moving RSSI gate --------------------------------------------------

    #[test]
    fn isolation_relaxes_the_gate_strictly_after_thirty_seconds() {
        let mut s = ConnectionScheduler::new();

        assert_eq!(s.update_rssi_threshold(0, 0, 100_000), RSSI_ISOLATED_BASE);
        assert_eq!(
            s.update_rssi_threshold(0, 0, 129_000),
            RSSI_ISOLATED_BASE,
            "29 s alone: still the base gate"
        );
        assert_eq!(
            s.update_rssi_threshold(0, 0, 130_000),
            RSSI_ISOLATED_BASE,
            "exactly 30 s: bitchat compares with `>`, so still base"
        );
        assert_eq!(
            s.update_rssi_threshold(0, 0, 130_001),
            RSSI_ISOLATED_RELAXED,
            "past 30 s: reach as far as the radio physically allows"
        );
    }

    #[test]
    fn finding_a_peer_ends_isolation_and_restarts_the_clock() {
        let mut s = ConnectionScheduler::new();
        s.update_rssi_threshold(0, 0, 0);
        assert_eq!(s.isolated_for_ms(60_000), Some(60_000));

        assert_eq!(
            s.update_rssi_threshold(1, 1, 60_000),
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        );
        assert_eq!(s.isolated_for_ms(60_000), None);

        // Alone again: the relaxation clock starts over rather than resuming.
        assert_eq!(s.update_rssi_threshold(0, 0, 61_000), RSSI_ISOLATED_BASE);
        assert_eq!(
            s.update_rssi_threshold(0, 0, 91_000),
            RSSI_ISOLATED_BASE,
            "30 s after the new isolation began, not the old one"
        );
        assert_eq!(s.update_rssi_threshold(0, 0, 91_001), RSSI_ISOLATED_RELAXED);
    }

    #[test]
    fn a_full_budget_tightens_the_gate() {
        let mut s = ConnectionScheduler::new();
        assert_eq!(
            s.update_rssi_threshold(1, 5, 0),
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        );
        assert_eq!(s.update_rssi_threshold(1, 6, 0), RSSI_CONNECTED_THRESHOLD);
        assert_eq!(s.update_rssi_threshold(1, 7, 0), RSSI_CONNECTED_THRESHOLD);
    }

    #[test]
    fn a_full_candidate_queue_tightens_the_gate() {
        let config = SchedulerConfig {
            candidate_cap: 2,
            ..Default::default()
        };
        let mut s = ConnectionScheduler::with_config(config);
        s.enqueue(candidate(P1, -65, 0));
        assert_eq!(
            s.update_rssi_threshold(1, 1, 0),
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        );

        s.enqueue(candidate(P2, -66, 0));
        assert_eq!(s.update_rssi_threshold(1, 1, 0), RSSI_CONNECTED_THRESHOLD);
    }

    #[test]
    fn timeouts_never_tighten_the_gate_globally() {
        // One flaky distant peer must not blind us to every other edge-of-range peer,
        // so the penalty is per-address only.
        let mut s = ConnectionScheduler::new();
        for address in ["p1", "p2", "p3"] {
            s.note_connect_timeout(address, 0);
        }
        assert_eq!(
            s.update_rssi_threshold(1, 1, 1_000),
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        );
    }

    #[test]
    fn the_gate_moving_changes_which_adverts_are_queued() {
        let mut s = ConnectionScheduler::new();

        // Isolated and relaxed: a -97 dBm advert is now worth connecting to.
        s.update_rssi_threshold(0, 0, 0);
        assert_eq!(discover(&mut s, P1, -97, 1_000), DiscoveryDecision::Queued);

        s.update_rssi_threshold(0, 0, 31_000);
        assert_eq!(s.rssi_threshold(), RSSI_ISOLATED_RELAXED);
        assert_eq!(
            discover(&mut s, P1, -97, 32_000),
            DiscoveryDecision::ConnectNow
        );

        // Saturated: the same advert would not be worth the churn.
        s.update_rssi_threshold(1, 6, 33_000);
        assert_eq!(discover(&mut s, P1, -97, 34_000), DiscoveryDecision::Queued);
        assert_eq!(discover(&mut s, P2, -86, 34_000), DiscoveryDecision::Queued);
        assert_eq!(
            discover(&mut s, P2, -84, 34_000),
            DiscoveryDecision::ConnectNow
        );
    }

    // -- Housekeeping ----------------------------------------------------------

    #[test]
    fn prune_boundary_keeps_records_exactly_at_the_window_edge() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 1_000);
        s.note_disconnected(P2, 1_000);
        assert_eq!(s.backoff_len(), 2);

        // now - 120_000 == 1_000, and records at the cutoff are retained.
        s.prune_backoff(121_000);
        assert_eq!(s.backoff_len(), 2);

        s.prune_backoff(121_001);
        assert_eq!(s.backoff_len(), 0);
    }

    #[test]
    fn pruning_forgets_failure_counts_for_peers_with_nothing_pending() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_failed(P1);
        s.note_connect_timeout(P2, 1_000);
        s.enqueue(candidate("queued", -60, 1_000));
        s.note_connect_failed("queued");

        // P1 has no timeout record and no queued candidate, so it goes; P2's record
        // is still inside the window; "queued" is still in the queue.
        s.prune_backoff(1_000);
        assert_eq!(s.failure_count(P1), 0);
        assert_eq!(s.failure_count(P2), 1);
        assert_eq!(s.failure_count("queued"), 1);

        // Past the window everything that is not queued is forgotten.
        s.prune_backoff(200_000);
        assert_eq!(s.failure_count(P2), 0);
        assert_eq!(s.failure_count("queued"), 1);
    }

    #[test]
    fn pruning_never_underflows_near_the_unix_epoch() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_timeout(P1, 0);
        s.prune_backoff(5);
        assert_eq!(s.backoff_len(), 1);
    }

    #[test]
    fn reset_restores_the_configured_starting_gate() {
        let config = SchedulerConfig {
            initial_rssi_threshold: -80,
            ..Default::default()
        };
        let mut s = ConnectionScheduler::with_config(config);
        s.enqueue(candidate(P1, -60, 1_000));
        s.note_connect_timeout(P1, 1_000);
        s.note_disconnected(P2, 1_000);
        s.note_connect_attempt(1_000);
        s.update_rssi_threshold(0, 0, 1_000);

        s.reset();

        assert_eq!(s.candidate_count(), 0);
        assert_eq!(s.backoff_len(), 0);
        assert_eq!(s.failure_count(P1), 0);
        assert_eq!(s.rssi_threshold(), -80);
        assert_eq!(s.isolated_for_ms(2_000), None);
        // The rate-limit clock is cleared too: the first advert after a reset
        // connects immediately.
        assert_eq!(discover(&mut s, P1, -50, 1_100), DiscoveryDecision::ConnectNow);
    }

    #[test]
    fn clock_going_backwards_fails_safe_towards_waiting() {
        let mut s = ConnectionScheduler::new();
        s.note_connect_attempt(100_000);
        // An earlier `now` reads as zero elapsed, i.e. the full limit, rather than
        // wrapping into a colossal elapsed time that would disable the limit.
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 90_000),
                0,
                None,
                PeripheralConnectionState::Disconnected,
                90_000
            ),
            DiscoveryDecision::ScheduleRetry { after_ms: 550 }
        );
    }

    // -- Convenience wrapper ---------------------------------------------------

    #[test]
    fn should_connect_mirrors_handle_discovery_and_still_queues() {
        let mut s = ConnectionScheduler::new();
        assert!(s.should_connect(P1, -50, 0, 1_000));
        s.note_connect_attempt(1_000);

        // Rate-limited now, but the advert is not thrown away.
        assert!(!s.should_connect(P2, -50, 0, 1_200));
        assert_eq!(s.candidate_count(), 1);
        assert!(matches!(
            s.next_candidate(1, never_busy, 1_500),
            QueueDecision::Connect(_)
        ));

        // Below the gate: queued, not connected.
        assert!(!s.should_connect(P2, -95, 0, 5_000));
        assert_eq!(s.candidate_count(), 1);
    }

    // -- The behaviour the whole module exists for -----------------------------

    #[test]
    fn a_crowded_room_produces_paced_connects_not_a_storm() {
        let mut s = ConnectionScheduler::new();
        let mut connects = 0usize;
        let mut in_flight = 0usize;

        // Thirty loud devices advertise within the same 100 ms.
        for i in 0..30 {
            let address = format!("addr-{i:02}");
            if s.should_connect(&address, -55, in_flight, 1_000 + i as u64 * 3) {
                s.note_connect_attempt(1_000 + i as u64 * 3);
                connects += 1;
                in_flight += 1;
            }
        }

        assert_eq!(connects, 1, "one attempt in the first 100 ms, not thirty");
        assert_eq!(s.candidate_count(), 29, "the rest are queued, not lost");

        // Draining at the rate limit reaches the budget and then stops.
        let mut now = 1_100;
        for _ in 0..40 {
            match s.next_candidate(in_flight, never_busy, now) {
                QueueDecision::Connect(_) => {
                    s.note_connect_attempt(now);
                    connects += 1;
                    in_flight += 1;
                }
                QueueDecision::RetryAfter { after_ms } => now += after_ms,
                QueueDecision::Idle => break,
            }
            now += 10;
        }

        assert_eq!(in_flight, MAX_CENTRAL_LINKS);
        assert_eq!(connects, MAX_CENTRAL_LINKS);
        assert!(
            now >= 1_100 + 5 * CONNECT_RATE_LIMIT_MS,
            "five further connects cannot happen faster than the rate limit allows"
        );
    }
}
