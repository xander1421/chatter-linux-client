//! Presence: when to say "I'm here", and whether to believe someone who does.
//!
//! Ported from `Services/BLE/BLEAnnounceHandler.swift`,
//! `BLEAnnounceHandlingPolicy.swift`, `BLEAnnounceThrottle.swift`, and the announce
//! half of `BLEMaintenancePolicy.swift`. Constants come from `TransportConfig.swift`.
//!
//! Two halves that pull in opposite directions:
//!
//! **Outbound**, an adaptive cadence. A device with no peers announces every 4
//! seconds, because nobody discovers a device that stays quiet and there is no
//! traffic to congest anyway. A connected device backs off to 15–30 seconds with
//! jitter, because in a room of twenty phones a 4-second announce from each is a
//! broadcast storm that starves real messages. The jitter is what keeps peers that
//! powered on together from converging on the same instant and colliding forever;
//! it is injected rather than drawn here, so tests are deterministic and this crate
//! stays free of an RNG.
//!
//! **Inbound**, a trust decision. An announce is a stranger claiming a nickname and
//! two public keys. It is worth nothing until its Ed25519 signature verifies against
//! the signing key it carries — at which point the claim is self-authenticating,
//! which is what lets a mesh with no server admit new peers at all. Verification
//! itself lives in `chatter-mesh-noise`, which is not a dependency here, so it
//! arrives as an injected closure. Nothing in this file does crypto.
//!
//! The one distinction to keep straight: **verified** means a signature checked out.
//! **Direct** means the TTL byte still equals the origination value — and the TTL is
//! excluded from the signature preimage precisely so relays can decrement it, so it
//! is forgeable by anyone. Every "direct" output here is named `*_hint` for that
//! reason. Where the two combine, verified is doing the work.

use chatter_mesh::BitchatPacket;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};
use chatter_mesh::relay::MESSAGE_TTL_DEFAULT;
use chatter_mesh::tlv::announce::AnnouncementPacket;

use crate::guard::{DEFAULT_MAX_AGE_MILLIS, PacketFreshnessPolicy};

// ---------------------------------------------------------------------------
// Cadence constants (TransportConfig.swift)
// ---------------------------------------------------------------------------

/// `bleAnnounceIntervalSeconds` — 4 s while isolated.
///
/// Aggressive on purpose. A device with no links is invisible until it speaks, and
/// with nothing connected there is no mesh to congest, so the cost of being wrong
/// in this direction is nil and the cost of being wrong in the other is never
/// joining at all.
// Even the fastest connected cadence, at the low end of its jitter, must stay
// slower than the isolated one -- backing off once connected is the entire point.
// Checked at compile time: all three are constants, so a runtime assert is folded
// away and would never observe a bad edit.
const _: () = assert!(
    CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS - CONNECTED_ANNOUNCE_JITTER_SPARSE_MILLIS
        > ISOLATED_ANNOUNCE_INTERVAL_MILLIS
);

pub const ISOLATED_ANNOUNCE_INTERVAL_MILLIS: u64 = 4_000;

/// `bleConnectedAnnounceBaseSecondsSparse` — 15 s with a few peers.
pub const CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS: u64 = 15_000;

/// `bleConnectedAnnounceBaseSecondsDense` — 30 s in a crowd.
pub const CONNECTED_ANNOUNCE_BASE_DENSE_MILLIS: u64 = 30_000;

/// `bleConnectedAnnounceJitterSparse` — ±4 s around the sparse base.
pub const CONNECTED_ANNOUNCE_JITTER_SPARSE_MILLIS: u64 = 4_000;

/// `bleConnectedAnnounceJitterDense` — ±8 s around the dense base.
///
/// Wider than sparse because there are more devices to spread out: the worst case
/// is 38 s between announces, which is why peer retention is 45–60 s and not less.
pub const CONNECTED_ANNOUNCE_JITTER_DENSE_MILLIS: u64 = 8_000;

/// `bleHighDegreeThreshold` — 6 links is where a mesh counts as dense.
pub const HIGH_DEGREE_THRESHOLD: usize = 6;

/// The 10 s shortcut in `BLEMaintenancePolicy.shouldSendAnnounce`.
///
/// Recent traffic means peers are actually nearby and listening, so presence is
/// worth refreshing sooner than the backed-off cadence would.
pub const RECENT_TRAFFIC_ANNOUNCE_INTERVAL_MILLIS: u64 = 10_000;

/// `bleAnnounceMinInterval` — 1 s floor between ordinary announces.
pub const ANNOUNCE_MIN_INTERVAL_MILLIS: u64 = 1_000;

/// `bleForceAnnounceMinIntervalSeconds` — 150 ms floor for forced announces.
///
/// Forced announces answer an event (a peer subscribed, a link came up) rather than
/// a timer, so they get a much lower floor — but still a floor, because the events
/// that trigger them arrive in bursts and an unthrottled reply is an amplifier.
pub const FORCE_ANNOUNCE_MIN_INTERVAL_MILLIS: u64 = 150;

/// Afterglow window: `Double.random(in: 0.3...0.6)` in `BLEAnnounceHandler`.
pub const AFTERGLOW_MIN_MILLIS: u64 = 300;
/// Upper bound of the afterglow window.
pub const AFTERGLOW_MAX_MILLIS: u64 = 600;

/// Picks the afterglow delay from an injected source.
///
/// A first-seen peer gets one extra announce shortly after, to push our presence
/// one more hop while the new link is fresh. The delay is randomised so that two
/// devices meeting simultaneously do not both re-announce at the same millisecond.
/// A value outside the window is clamped rather than trusted.
pub fn afterglow_delay_millis(pick: impl FnOnce(u64, u64) -> u64) -> u64 {
    let picked = pick(AFTERGLOW_MIN_MILLIS, AFTERGLOW_MAX_MILLIS);
    picked.clamp(AFTERGLOW_MIN_MILLIS, AFTERGLOW_MAX_MILLIS)
}

// ---------------------------------------------------------------------------
// Outbound cadence
// ---------------------------------------------------------------------------

/// How often we should be announcing, given who we are connected to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceCadence {
    /// No links. Announce hard — nothing else will make us discoverable.
    Isolated,
    /// A handful of links. 15 s ± 4 s.
    ConnectedSparse,
    /// At or above [`HIGH_DEGREE_THRESHOLD`] links. 30 s ± 8 s.
    ConnectedDense,
}

impl AnnounceCadence {
    pub fn for_connection_state(connected_count: usize, high_degree_threshold: usize) -> Self {
        if connected_count == 0 {
            Self::Isolated
        } else if connected_count >= high_degree_threshold {
            Self::ConnectedDense
        } else {
            Self::ConnectedSparse
        }
    }

    /// Interval before jitter.
    pub const fn base_millis(self) -> u64 {
        match self {
            Self::Isolated => ISOLATED_ANNOUNCE_INTERVAL_MILLIS,
            Self::ConnectedSparse => CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS,
            Self::ConnectedDense => CONNECTED_ANNOUNCE_BASE_DENSE_MILLIS,
        }
    }

    /// Half-width of the jitter window. Zero while isolated: discovery is the whole
    /// job then, and there is no congestion for jitter to relieve.
    pub const fn jitter_millis(self) -> u64 {
        match self {
            Self::Isolated => 0,
            Self::ConnectedSparse => CONNECTED_ANNOUNCE_JITTER_SPARSE_MILLIS,
            Self::ConnectedDense => CONNECTED_ANNOUNCE_JITTER_DENSE_MILLIS,
        }
    }

    pub const fn is_connected(self) -> bool {
        !matches!(self, Self::Isolated)
    }

    /// Interval with jitter applied.
    ///
    /// `jitter` is handed the inclusive bounds `(-half_width, +half_width)` in
    /// milliseconds and returns an offset; anything outside that range is clamped,
    /// so a misbehaving source can widen nothing. The result is floored at zero.
    pub fn interval_millis(self, jitter: impl FnOnce(i64, i64) -> i64) -> u64 {
        let half_width = self.jitter_millis();
        if half_width == 0 {
            return self.base_millis();
        }

        let high = half_width as i64;
        let low = -high;
        let offset = jitter(low, high).max(low).min(high);

        let interval = (self.base_millis() as i64).saturating_add(offset);
        if interval <= 0 { 0 } else { interval as u64 }
    }
}

/// What the cadence depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceConditions {
    /// Number of live links right now.
    pub connected_count: usize,
    /// Whether packets arrived recently (`bleRecentTrafficForceScanSeconds` in the
    /// caller's bookkeeping). Shortens the interval to at most 10 s.
    pub has_recent_traffic: bool,
    /// Link count at which the mesh counts as dense.
    pub high_degree_threshold: usize,
}

impl Default for AnnounceConditions {
    fn default() -> Self {
        Self {
            connected_count: 0,
            has_recent_traffic: false,
            high_degree_threshold: HIGH_DEGREE_THRESHOLD,
        }
    }
}

impl AnnounceConditions {
    pub fn isolated() -> Self {
        Self::default()
    }

    pub fn connected(connected_count: usize) -> Self {
        Self {
            connected_count,
            ..Self::default()
        }
    }

    pub fn cadence(&self) -> AnnounceCadence {
        AnnounceCadence::for_connection_state(self.connected_count, self.high_degree_threshold)
    }

    /// The interval actually in force, recent-traffic shortcut included.
    pub fn interval_millis(&self, jitter: impl FnOnce(i64, i64) -> i64) -> u64 {
        let interval = self.cadence().interval_millis(jitter);
        if self.has_recent_traffic {
            interval.min(RECENT_TRAFFIC_ANNOUNCE_INTERVAL_MILLIS)
        } else {
            interval
        }
    }
}

/// Tracks when we last announced and says when the next one is due.
///
/// Sans-IO: it holds no timer and reads no clock. The caller ticks it with the
/// current time — `BLEMaintenancePolicy` is evaluated on a 5 s maintenance cycle in
/// bitchat, and the same shape works here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnnounceScheduler {
    last_announce_millis: Option<u64>,
}

impl AnnounceScheduler {
    /// A scheduler that has never announced, so the first announce is due at once.
    pub fn new() -> Self {
        Self::default()
    }

    /// A scheduler that just announced at `now_unix_millis`.
    pub fn started_at(now_unix_millis: u64) -> Self {
        Self {
            last_announce_millis: Some(now_unix_millis),
        }
    }

    pub fn last_announce_millis(&self) -> Option<u64> {
        self.last_announce_millis
    }

    /// Records that an announce went out.
    pub fn record_sent(&mut self, now_unix_millis: u64) {
        self.last_announce_millis = Some(now_unix_millis);
    }

    /// Forgets the last announce, so the next one is due immediately. Used when the
    /// adapter comes back or the local identity rotates.
    pub fn reset(&mut self) {
        self.last_announce_millis = None;
    }

    /// Time since the last announce, saturating at zero if the clock went backwards
    /// and at [`u64::MAX`] if we have never announced.
    pub fn elapsed_millis(&self, now_unix_millis: u64) -> u64 {
        match self.last_announce_millis {
            Some(last) => now_unix_millis.saturating_sub(last),
            None => u64::MAX,
        }
    }

    /// Absolute deadline for the next announce, or `0` when one is already overdue
    /// because we have never announced.
    ///
    /// The jitter closure is consulted once per call, so a caller that wants a
    /// stable deadline across ticks must hold onto the value rather than re-deriving
    /// it — re-rolling every tick would bias the effective interval toward the low
    /// end of the window.
    pub fn next_due_millis(
        &self,
        conditions: &AnnounceConditions,
        jitter: impl FnOnce(i64, i64) -> i64,
    ) -> u64 {
        match self.last_announce_millis {
            Some(last) => last.saturating_add(conditions.interval_millis(jitter)),
            None => 0,
        }
    }

    /// How long until the next announce is due; `0` means now.
    pub fn millis_until_due(
        &self,
        conditions: &AnnounceConditions,
        now_unix_millis: u64,
        jitter: impl FnOnce(i64, i64) -> i64,
    ) -> u64 {
        self.next_due_millis(conditions, jitter)
            .saturating_sub(now_unix_millis)
    }

    /// Whether an announce should go out now.
    ///
    /// Equivalent to `BLEMaintenancePolicy.shouldSendAnnounce`: the recent-traffic
    /// shortcut and the cadence are the same rule expressed as `elapsed >=
    /// min(shortcut, cadence)`.
    pub fn is_due(
        &self,
        conditions: &AnnounceConditions,
        now_unix_millis: u64,
        jitter: impl FnOnce(i64, i64) -> i64,
    ) -> bool {
        now_unix_millis >= self.next_due_millis(conditions, jitter)
    }
}

/// The floor under every announce, whatever asked for it.
///
/// Ported from `BLEAnnounceThrottle`. Distinct from [`AnnounceScheduler`], and both
/// are needed: the scheduler paces the timer-driven announce, the throttle catches
/// everything else — subscriptions, link-up events, manual refreshes — that would
/// otherwise bypass the cadence entirely and flood the mesh from the side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceThrottle {
    last_sent_millis: Option<u64>,
    normal_min_interval_millis: u64,
    forced_min_interval_millis: u64,
}

impl Default for AnnounceThrottle {
    fn default() -> Self {
        Self {
            last_sent_millis: None,
            normal_min_interval_millis: ANNOUNCE_MIN_INTERVAL_MILLIS,
            forced_min_interval_millis: FORCE_ANNOUNCE_MIN_INTERVAL_MILLIS,
        }
    }
}

impl AnnounceThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_intervals(normal_millis: u64, forced_millis: u64) -> Self {
        Self {
            last_sent_millis: None,
            normal_min_interval_millis: normal_millis,
            forced_min_interval_millis: forced_millis,
        }
    }

    pub fn last_sent_millis(&self) -> Option<u64> {
        self.last_sent_millis
    }

    /// Time since the last *accepted* send. [`u64::MAX`] before the first one,
    /// standing in for Swift's `Date.distantPast`.
    pub fn elapsed_millis(&self, now_unix_millis: u64) -> u64 {
        match self.last_sent_millis {
            Some(last) => now_unix_millis.saturating_sub(last),
            None => u64::MAX,
        }
    }

    /// Asks permission to announce, and takes it if granted.
    ///
    /// Mutating on success is deliberate — a check that did not consume its slot
    /// would let two callers in the same millisecond both believe they may send.
    pub fn should_send(&mut self, force: bool, now_unix_millis: u64) -> bool {
        let minimum = if force {
            self.forced_min_interval_millis
        } else {
            self.normal_min_interval_millis
        };

        if self.elapsed_millis(now_unix_millis) < minimum {
            return false;
        }

        self.last_sent_millis = Some(now_unix_millis);
        true
    }

    pub fn reset(&mut self) {
        self.last_sent_millis = None;
    }
}

// ---------------------------------------------------------------------------
// Inbound announces
// ---------------------------------------------------------------------------

/// Why an announce never got as far as a trust decision.
///
/// Ported from `BLEAnnouncePreflightRejection`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnounceRejection {
    /// The payload is not a decodable announcement TLV.
    Malformed,

    /// The peer ID derived from the announced Noise key is not the sender ID on the
    /// packet. The mesh ID *is* `SHA256(noise key)[..8]`, so this is checkable
    /// without any crypto — and it is what stops an attacker announcing someone
    /// else's keys under their own sender ID.
    SenderMismatch { derived: PeerId },

    /// It claims to be us.
    SelfAnnounce,

    /// Older than the announce staleness window. Multi-hop relaying means a
    /// legitimate announce can be genuinely old, so this window is 15 minutes
    /// rather than the 2 minutes the ingress guard applies — but not unbounded,
    /// because unbounded is a replay licence.
    Stale { age_millis: u64 },
}

/// An announce that decoded and belongs to the sender who sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncePreflight {
    pub announcement: AnnouncementPacket,
    /// Derived from the announced Noise key, and equal to the packet's sender ID —
    /// checked, not assumed.
    pub derived_peer_id: PeerId,
}

/// Everything the preflight needs beyond the packet itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnouncePreflightConfig {
    /// Staleness window. Defaults to [`DEFAULT_MAX_AGE_MILLIS`].
    pub max_age_millis: u64,
}

impl Default for AnnouncePreflightConfig {
    fn default() -> Self {
        Self {
            max_age_millis: DEFAULT_MAX_AGE_MILLIS,
        }
    }
}

/// Decode, attribute, and age-check an announce, before any crypto runs.
///
/// Ported from `BLEAnnouncePreflightPolicy.evaluate`. Cheap checks first is not
/// just an optimisation here: signature verification is the expensive operation an
/// attacker would like us to perform on their schedule, and three cheap tests
/// discard most forgeries before we spend one.
pub fn preflight_announce(
    packet: &BitchatPacket,
    from: &PeerId,
    local_peer: &PeerId,
    now_unix_millis: u64,
    config: &AnnouncePreflightConfig,
) -> Result<AnnouncePreflight, AnnounceRejection> {
    let announcement = match AnnouncementPacket::decode(&packet.payload) {
        Ok(announcement) => announcement,
        Err(_) => return Err(AnnounceRejection::Malformed),
    };

    let derived_peer_id = PeerId::from_noise_key(&announcement.noise_public_key);
    if derived_peer_id != *from {
        return Err(AnnounceRejection::SenderMismatch {
            derived: derived_peer_id,
        });
    }

    if from == local_peer {
        return Err(AnnounceRejection::SelfAnnounce);
    }

    if PacketFreshnessPolicy::is_stale(packet.timestamp, now_unix_millis, config.max_age_millis) {
        return Err(AnnounceRejection::Stale {
            age_millis: PacketFreshnessPolicy::age_millis(packet.timestamp, now_unix_millis),
        });
    }

    Ok(AnnouncePreflight {
        announcement,
        derived_peer_id,
    })
}

/// Why an announce is not trustworthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceDistrust {
    /// Unsigned. bitchat kept no backward-compatible path for this, and neither do
    /// we: an unsigned announce is an unauthenticated claim to an identity.
    MissingSignature,
    /// Signed, but the signature does not verify against the signing key it carries.
    InvalidSignature,
    /// The announced Noise key differs from the one we already recorded for this
    /// peer ID. Checked *before* the signature, because a valid signature over a
    /// swapped key is exactly what a key-substitution attempt looks like.
    KeyMismatch,
    /// The announced SIGNING key differs from the one we already recorded, even
    /// though the Noise key matches.
    ///
    /// Without this check `Verified` meant only "signed by the key this very
    /// packet carries" — which every attacker holds. The peer ID is
    /// `SHA256(noise key)[..8]`, so the Noise key is bound to the ID and cannot
    /// be swapped; the signing key was bound to nothing. Anyone in radio range
    /// who had heard a peer's announce (the Noise key is broadcast in clear)
    /// could re-announce that identity under their own signing key and be
    /// recorded as Verified.
    ///
    /// Never legitimate: `MeshIdentity` holds both secrets and is persisted as
    /// one unit (`chatter-mesh-noise/src/identity.rs:22`), so the two keys only
    /// ever change together — and a changed Noise key is a different peer ID.
    /// OPEN-ITEMS §98.
    SigningKeyMismatch,
}

/// Whether an announce may be acted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceTrust {
    Verified,
    Unverified(AnnounceDistrust),
}

impl AnnounceTrust {
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }

    pub const fn distrust(self) -> Option<AnnounceDistrust> {
        match self {
            Self::Verified => None,
            Self::Unverified(reason) => Some(reason),
        }
    }
}

/// What we have already pinned for a peer ID, if anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinnedKeys<'a> {
    /// The Noise static key recorded for this peer ID.
    pub noise: Option<&'a [u8]>,
    /// The signing key recorded for this peer ID (§98).
    pub signing: Option<&'a [u8]>,
}

/// The trust decision, given a signature result and what we already knew.
///
/// Ported from `BLEAnnounceTrustPolicy.evaluate`, order preserved. `signature_valid`
/// is only consulted when `has_signature` is true, matching Swift, so a caller that
/// skips verification for an unsigned packet still gets the right answer.
///
/// Both pinned keys are checked BEFORE the signature, for the same reason: a
/// valid signature over a substituted key is what an impersonation attempt looks
/// like, not evidence against one.
pub fn evaluate_announce_trust(
    has_signature: bool,
    signature_valid: bool,
    pinned: PinnedKeys<'_>,
    announced_noise_public_key: &[u8],
    announced_signing_public_key: &[u8],
) -> AnnounceTrust {
    if let Some(existing) = pinned.noise {
        if existing != announced_noise_public_key {
            return AnnounceTrust::Unverified(AnnounceDistrust::KeyMismatch);
        }
    }

    if let Some(existing) = pinned.signing {
        if existing != announced_signing_public_key {
            return AnnounceTrust::Unverified(AnnounceDistrust::SigningKeyMismatch);
        }
    }

    if !has_signature {
        return AnnounceTrust::Unverified(AnnounceDistrust::MissingSignature);
    }

    if !signature_valid {
        return AnnounceTrust::Unverified(AnnounceDistrust::InvalidSignature);
    }

    AnnounceTrust::Verified
}

/// What we know about the link an announce arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceLinkContext<'a> {
    /// TTL an announce carries at origination, for the direct hint.
    pub direct_announce_ttl: u8,
    /// We hold a peripheral-role link to this peer.
    pub has_peripheral_link: bool,
    /// This peer is subscribed to us in the central role.
    pub has_central_link: bool,
    /// The link this packet arrived on is already bound to a *different* peer.
    ///
    /// Suppresses the "direct means connected" shortcut. Directness rides on the
    /// unsigned TTL, so a replayed announce can look direct on the replayer's link;
    /// letting that shortcut mark an absent peer connected would forge presence for
    /// free. Only a containment-checked rebind may promote such a link.
    pub link_bound_to_other_peer: bool,
    /// The Noise key already recorded for this peer ID, if any.
    pub existing_noise_public_key: Option<&'a [u8]>,
    /// The signing key already recorded for this peer ID, if any (§98).
    pub existing_signing_public_key: Option<&'a [u8]>,
}

impl Default for AnnounceLinkContext<'_> {
    fn default() -> Self {
        Self {
            direct_announce_ttl: MESSAGE_TTL_DEFAULT,
            has_peripheral_link: false,
            has_central_link: false,
            link_bound_to_other_peer: false,
            existing_noise_public_key: None,
            existing_signing_public_key: None,
        }
    }
}

/// An announce that passed preflight, with its trust decision attached.
///
/// Note that failing the trust check is *not* a rejection: Swift still returns a
/// result for an unverified announce, it just does nothing with it. Modelling it the
/// same way keeps the "why was this ignored" answer available to the caller instead
/// of collapsing every distrust into a bare `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedAnnounce {
    pub peer_id: PeerId,
    pub announcement: AnnouncementPacket,
    pub trust: AnnounceTrust,

    /// **Unauthenticated hint.** The TTL still equals the origination value, which
    /// usually means this came straight from its author. The TTL is outside the
    /// signature preimage, so anyone can restore it. Never proof of adjacency.
    pub direct_hint: bool,

    /// Whether the link is bound to someone else, which is what disqualifies
    /// [`Self::direct_hint`] from implying connectivity.
    pub link_bound_to_other_peer: bool,

    /// We already hold a link to this peer in either role. Unlike
    /// [`Self::direct_hint`] this is a fact about our own connection table rather
    /// than a claim in the packet, so it is not forgeable.
    pub has_direct_link: bool,
}

impl AcceptedAnnounce {
    pub fn is_verified(&self) -> bool {
        self.trust.is_verified()
    }

    /// Whether the peer registry should be updated at all.
    ///
    /// Only verified announces touch the registry. An unverified one is dropped:
    /// nickname, keys, and presence all come from the announce, and accepting them
    /// unverified is identity poisoning by design.
    pub fn should_upsert_peer(&self) -> bool {
        self.is_verified()
    }

    /// The `is_connected` flag for the registry upsert.
    ///
    /// `Some` only when the announce is verified, so an unverified announce cannot
    /// mark anyone present even by accident. The direct hint contributes only when
    /// the link is not already someone else's.
    pub fn connected_flag(&self) -> Option<bool> {
        if !self.is_verified() {
            return None;
        }
        Some(self.connected_hint())
    }

    /// The connectivity conclusion before the verification gate, exposed for tests
    /// and diagnostics. Prefer [`Self::connected_flag`].
    pub fn connected_hint(&self) -> bool {
        self.has_direct_link || (self.direct_hint && !self.link_bound_to_other_peer)
    }

    /// Whether the announced neighbour claims may enter the topology.
    ///
    /// Verified only. Unverified neighbour lists would let anyone rewrite the mesh's
    /// idea of who is next to whom, which is a routing attack with no crypto in it.
    pub fn should_update_topology(&self) -> bool {
        self.is_verified() && self.announcement.direct_neighbors.is_some()
    }

    /// The neighbour claims, when they may be used.
    pub fn topology_neighbors(&self) -> Option<&[[u8; SENDER_ID_LEN]]> {
        if !self.is_verified() {
            return None;
        }
        self.announcement.direct_neighbors.as_deref()
    }

    /// Whether the announced identity may be persisted for offline verification.
    ///
    /// Verified only. Persisting an unverified announce would let an attacker who
    /// replays a victim's Noise key overwrite the victim's stored signing key and
    /// nickname — the poisoning survives the session, which is what makes it worse
    /// than a transient forgery.
    pub fn should_persist_identity(&self) -> bool {
        self.is_verified()
    }

    /// Whether this announce may rebind the link it arrived on to its sender.
    ///
    /// Both conditions are load-bearing and neither is sufficient. Verified proves
    /// the sender owns the identity; direct is the (forgeable) claim that they own
    /// the link. bitchat is explicit that this residue cannot be closed here: a
    /// replay that wins the containment-checked rebind still forges *presence*.
    /// What it cannot forge is delivery — private messages stay gated on an
    /// established Noise session, so a forged presence yields a courier-parked
    /// message, not a readable one.
    pub fn may_rebind_link(&self) -> bool {
        self.is_verified() && self.direct_hint
    }
}

/// Run the inbound announce policy: preflight, then trust.
///
/// Ported from `BLEAnnounceHandler.handle`, minus the queue hops and the registry
/// mutation — those belong to the caller, which owns the registry. What is preserved
/// exactly is the *order*: decode and attribute, then check age, then check the
/// recorded key, then verify the signature. Verification is the last and only
/// expensive step, and it is injected because it lives in `chatter-mesh-noise`.
///
/// `verify` is handed the packet and the signing key the announcement carries. It
/// must compute the signature preimage itself ([`BitchatPacket::signing_preimage`]);
/// this module never touches key material.
pub fn evaluate_announce(
    packet: &BitchatPacket,
    from: &PeerId,
    local_peer: &PeerId,
    now_unix_millis: u64,
    link: &AnnounceLinkContext<'_>,
    config: &AnnouncePreflightConfig,
    verify: impl Fn(&BitchatPacket, &[u8]) -> bool,
) -> Result<AcceptedAnnounce, AnnounceRejection> {
    let preflight = preflight_announce(packet, from, local_peer, now_unix_millis, config)?;

    let has_signature = packet.signature.is_some();
    let signature_valid =
        has_signature && verify(packet, &preflight.announcement.signing_public_key);

    let trust = evaluate_announce_trust(
        has_signature,
        signature_valid,
        PinnedKeys {
            noise: link.existing_noise_public_key,
            signing: link.existing_signing_public_key,
        },
        &preflight.announcement.noise_public_key,
        &preflight.announcement.signing_public_key,
    );

    let direct_hint = packet.msg_type == MessageType::Announce.as_u8()
        && packet.ttl == link.direct_announce_ttl;

    Ok(AcceptedAnnounce {
        peer_id: preflight.derived_peer_id,
        announcement: preflight.announcement,
        trust,
        direct_hint,
        link_bound_to_other_peer: direct_hint && link.link_bound_to_other_peer,
        has_direct_link: link.has_peripheral_link || link.has_central_link,
    })
}

/// What the registry said about a peer after a verified announce was applied.
///
/// The caller supplies this from its own peer registry; it is the second half of the
/// Swift handler, which reads these two flags back out of the barrier block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnnounceRegistryOutcome {
    /// We had never seen this peer.
    pub is_new_peer: bool,
    /// We knew this peer and had marked them gone.
    pub is_reconnected_peer: bool,
}

/// Everything to do after an announce has been applied.
///
/// Ported from `BLEAnnounceResponsePolicy.plan`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnnounceResponsePlan {
    /// Raise a "peer connected" event. Gated on the direct hint, so a relayed
    /// announce from three hops away never reads as someone walking into the room.
    pub notify_peer_connected: bool,
    /// Kick off the initial gossip sync with this peer.
    pub schedule_initial_sync: bool,
    /// Reciprocate, so they discover us too. Deduplicated by the caller — bitchat
    /// keys `"announce-back-{peer}"` in the message deduplicator — because otherwise
    /// two peers announce back at each other indefinitely.
    pub schedule_announce_back: bool,
    /// Re-announce shortly, to push our presence one more hop past a brand-new peer.
    pub schedule_afterglow: bool,
}

/// Plan the follow-up work for an announce.
///
/// `direct_hint` is the unauthenticated TTL-derived flag; it gates *notification*,
/// which is presentation, not authorisation. `should_announce_back` is the caller's
/// dedup answer.
pub fn plan_announce_response(
    direct_hint: bool,
    outcome: AnnounceRegistryOutcome,
    should_announce_back: bool,
) -> AnnounceResponsePlan {
    let notify_peer_connected =
        direct_hint && (outcome.is_new_peer || outcome.is_reconnected_peer);

    AnnounceResponsePlan {
        notify_peer_connected,
        schedule_initial_sync: notify_peer_connected,
        schedule_announce_back: should_announce_back,
        schedule_afterglow: outcome.is_new_peer,
    }
}

/// The dedup key bitchat uses to send at most one announce-back per peer.
pub fn announce_back_dedup_id(peer: &PeerId) -> String {
    format!("announce-back-{}", peer.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh::packet::SIGNATURE_LEN;
    use std::cell::Cell;

    const LOCAL: [u8; SENDER_ID_LEN] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
    const NOISE_KEY: &[u8] = b"noise-static-public-key-of-a-peer";
    const OTHER_NOISE_KEY: &[u8] = b"a-completely-different-noise-key!";
    const SIGNING_KEY: &[u8] = b"ed25519-signing-public-key------";
    const OTHER_SIGNING_KEY: &[u8] = b"an-attackers-own-signing-key----";

    fn no_jitter(_low: i64, _high: i64) -> i64 {
        0
    }

    fn announcement() -> AnnouncementPacket {
        AnnouncementPacket::new(
            "alice".to_string(),
            NOISE_KEY.to_vec(),
            SIGNING_KEY.to_vec(),
            None,
            None,
            None,
        )
    }

    fn sender_peer() -> PeerId {
        PeerId::from_noise_key(NOISE_KEY)
    }

    /// The wire sender ID is the first 8 bytes of the derived mesh ID.
    fn sender_bytes() -> [u8; SENDER_ID_LEN] {
        let peer = sender_peer();
        let mut out = [0u8; SENDER_ID_LEN];
        let hex = peer.as_str().as_bytes();
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = (hex[i * 2] as char).to_digit(16).unwrap() as u8;
            let lo = (hex[i * 2 + 1] as char).to_digit(16).unwrap() as u8;
            *byte = (hi << 4) | lo;
        }
        out
    }

    fn announce_packet(timestamp: u64, ttl: u8, signed: bool) -> BitchatPacket {
        let payload = announcement().encode().unwrap();
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            sender_bytes(),
            None,
            timestamp,
            payload,
            ttl,
        );
        if signed {
            packet.signature = Some([0x5A; SIGNATURE_LEN]);
        }
        packet
    }

    // --- cadence ---------------------------------------------------------

    #[test]
    fn cadence_tracks_connection_state_and_density() {
        let threshold = HIGH_DEGREE_THRESHOLD;
        assert_eq!(
            AnnounceCadence::for_connection_state(0, threshold),
            AnnounceCadence::Isolated
        );
        assert_eq!(
            AnnounceCadence::for_connection_state(1, threshold),
            AnnounceCadence::ConnectedSparse
        );
        assert_eq!(
            AnnounceCadence::for_connection_state(threshold - 1, threshold),
            AnnounceCadence::ConnectedSparse
        );
        // The threshold itself is dense: `connectedCount >= highDegreeThreshold`.
        assert_eq!(
            AnnounceCadence::for_connection_state(threshold, threshold),
            AnnounceCadence::ConnectedDense
        );
        assert!(!AnnounceCadence::Isolated.is_connected());
        assert!(AnnounceCadence::ConnectedSparse.is_connected());
    }

    #[test]
    fn an_isolated_device_announces_every_four_seconds() {
        let scheduler = AnnounceScheduler::started_at(1_000_000);
        let isolated = AnnounceConditions::isolated();

        assert!(!scheduler.is_due(&isolated, 1_003_999, no_jitter));
        assert!(scheduler.is_due(&isolated, 1_004_000, no_jitter));
        assert_eq!(
            scheduler.next_due_millis(&isolated, no_jitter),
            1_000_000 + ISOLATED_ANNOUNCE_INTERVAL_MILLIS
        );
        // Isolation is not jittered — discovery is the whole job.
        assert_eq!(AnnounceCadence::Isolated.jitter_millis(), 0);
        assert_eq!(
            AnnounceCadence::Isolated.interval_millis(|_, _| 10_000),
            ISOLATED_ANNOUNCE_INTERVAL_MILLIS
        );
    }

    #[test]
    fn a_connected_device_backs_off_and_sparse_differs_from_dense() {
        let scheduler = AnnounceScheduler::started_at(0);
        let sparse = AnnounceConditions::connected(2);
        let dense = AnnounceConditions::connected(HIGH_DEGREE_THRESHOLD);

        assert_eq!(sparse.cadence(), AnnounceCadence::ConnectedSparse);
        assert_eq!(dense.cadence(), AnnounceCadence::ConnectedDense);

        // Sparse: 15 s.
        assert!(!scheduler.is_due(&sparse, CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS - 1, no_jitter));
        assert!(scheduler.is_due(&sparse, CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS, no_jitter));

        // Dense: 30 s — a crowd gets half the announce rate.
        assert!(!scheduler.is_due(&dense, CONNECTED_ANNOUNCE_BASE_DENSE_MILLIS - 1, no_jitter));
        assert!(scheduler.is_due(&dense, CONNECTED_ANNOUNCE_BASE_DENSE_MILLIS, no_jitter));
        assert!(!scheduler.is_due(&dense, CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS, no_jitter));

    }

    #[test]
    fn jitter_widths_differ_by_density_and_are_clamped_to_their_window() {
        assert_eq!(
            AnnounceCadence::ConnectedSparse.jitter_millis(),
            CONNECTED_ANNOUNCE_JITTER_SPARSE_MILLIS
        );
        assert_eq!(
            AnnounceCadence::ConnectedDense.jitter_millis(),
            CONNECTED_ANNOUNCE_JITTER_DENSE_MILLIS
        );

        // The jitter closure is handed the exact window bounds.
        let mut seen = None;
        AnnounceCadence::ConnectedDense.interval_millis(|low, high| {
            seen = Some((low, high));
            0
        });
        assert_eq!(seen, Some((-8_000, 8_000)));

        // Sparse spans 11–19 s, dense 22–38 s.
        assert_eq!(
            AnnounceCadence::ConnectedSparse.interval_millis(|low, _| low),
            11_000
        );
        assert_eq!(
            AnnounceCadence::ConnectedSparse.interval_millis(|_, high| high),
            19_000
        );
        assert_eq!(
            AnnounceCadence::ConnectedDense.interval_millis(|low, _| low),
            22_000
        );
        assert_eq!(
            AnnounceCadence::ConnectedDense.interval_millis(|_, high| high),
            38_000
        );

        // A source returning nonsense cannot widen the window in either direction.
        assert_eq!(
            AnnounceCadence::ConnectedDense.interval_millis(|_, _| i64::MIN),
            22_000
        );
        assert_eq!(
            AnnounceCadence::ConnectedDense.interval_millis(|_, _| i64::MAX),
            38_000
        );
    }

    #[test]
    fn recent_traffic_shortens_the_connected_interval_to_ten_seconds() {
        let scheduler = AnnounceScheduler::started_at(0);
        let mut dense = AnnounceConditions::connected(HIGH_DEGREE_THRESHOLD);
        dense.has_recent_traffic = true;

        assert!(!scheduler.is_due(&dense, RECENT_TRAFFIC_ANNOUNCE_INTERVAL_MILLIS - 1, no_jitter));
        assert!(scheduler.is_due(&dense, RECENT_TRAFFIC_ANNOUNCE_INTERVAL_MILLIS, no_jitter));

        // It only ever shortens: an isolated device already announces faster.
        let mut isolated = AnnounceConditions::isolated();
        isolated.has_recent_traffic = true;
        assert_eq!(
            isolated.interval_millis(no_jitter),
            ISOLATED_ANNOUNCE_INTERVAL_MILLIS
        );
    }

    #[test]
    fn a_scheduler_that_never_announced_is_due_immediately() {
        let mut scheduler = AnnounceScheduler::new();
        let conditions = AnnounceConditions::connected(3);

        assert_eq!(scheduler.elapsed_millis(1_000_000), u64::MAX);
        assert_eq!(scheduler.next_due_millis(&conditions, no_jitter), 0);
        assert!(scheduler.is_due(&conditions, 0, no_jitter));
        assert_eq!(scheduler.millis_until_due(&conditions, 0, no_jitter), 0);

        scheduler.record_sent(1_000_000);
        assert!(!scheduler.is_due(&conditions, 1_000_000, no_jitter));
        assert_eq!(
            scheduler.millis_until_due(&conditions, 1_000_000, no_jitter),
            CONNECTED_ANNOUNCE_BASE_SPARSE_MILLIS
        );

        scheduler.reset();
        assert!(scheduler.is_due(&conditions, 1_000_000, no_jitter));
    }

    #[test]
    fn a_clock_that_jumps_backwards_delays_rather_than_underflowing() {
        let scheduler = AnnounceScheduler::started_at(1_000_000);
        let isolated = AnnounceConditions::isolated();

        assert_eq!(scheduler.elapsed_millis(999_000), 0);
        assert!(!scheduler.is_due(&isolated, 999_000, no_jitter));
    }

    // --- throttle --------------------------------------------------------

    #[test]
    fn the_minimum_interval_throttle_holds_exactly_at_its_boundary() {
        let mut throttle = AnnounceThrottle::new();

        assert!(throttle.should_send(false, 1_000_000));
        // 1 s floor: one millisecond short is refused, the boundary itself passes.
        assert!(!throttle.should_send(
            false,
            1_000_000 + ANNOUNCE_MIN_INTERVAL_MILLIS - 1
        ));
        assert!(throttle.should_send(false, 1_000_000 + ANNOUNCE_MIN_INTERVAL_MILLIS));
        assert_eq!(
            throttle.last_sent_millis(),
            Some(1_000_000 + ANNOUNCE_MIN_INTERVAL_MILLIS)
        );
    }

    #[test]
    fn a_forced_announce_gets_a_lower_floor_but_still_a_floor() {
        let mut throttle = AnnounceThrottle::new();
        assert!(throttle.should_send(false, 0));

        // The normal floor would refuse these; the forced floor is 150 ms.
        assert!(!throttle.should_send(true, FORCE_ANNOUNCE_MIN_INTERVAL_MILLIS - 1));
        assert!(throttle.should_send(true, FORCE_ANNOUNCE_MIN_INTERVAL_MILLIS));
        assert!(!throttle.should_send(false, FORCE_ANNOUNCE_MIN_INTERVAL_MILLIS + 1));
    }

    #[test]
    fn a_refused_send_does_not_move_the_window() {
        let mut throttle = AnnounceThrottle::with_intervals(10_000, 2_000);

        assert!(throttle.should_send(false, 100_000));
        assert!(!throttle.should_send(false, 109_900));
        // The refusal at 109_900 must not have reset the clock: 110_000 is still
        // exactly 10 s after the accepted send.
        assert!(throttle.should_send(false, 110_000));
        assert_eq!(throttle.elapsed_millis(113_000), 3_000);

        throttle.reset();
        assert_eq!(throttle.elapsed_millis(113_000), u64::MAX);
        assert!(throttle.should_send(false, 113_000));
    }

    #[test]
    fn the_first_announce_is_never_throttled() {
        let mut throttle = AnnounceThrottle::new();
        assert!(throttle.should_send(false, 0));

        let mut forced = AnnounceThrottle::new();
        assert!(forced.should_send(true, 0));
    }

    // --- inbound preflight -----------------------------------------------

    #[test]
    fn a_well_formed_announce_passes_preflight() {
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        let preflight = preflight_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnouncePreflightConfig::default(),
        )
        .expect("a fresh, correctly attributed announce");

        assert_eq!(preflight.derived_peer_id, sender_peer());
        assert_eq!(preflight.announcement.nickname, "alice");
    }

    #[test]
    fn an_undecodable_payload_is_rejected_as_malformed() {
        let mut packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        packet.payload = vec![0xFF, 0xFF, 0xFF];

        assert_eq!(
            preflight_announce(
                &packet,
                &sender_peer(),
                &PeerId::from_bytes(&LOCAL),
                1_000_000,
                &AnnouncePreflightConfig::default(),
            ),
            Err(AnnounceRejection::Malformed)
        );
    }

    #[test]
    fn an_announce_whose_key_does_not_derive_its_sender_is_rejected() {
        // Someone announcing another peer's advertised identity under their own
        // sender ID. No crypto needed to catch it: the mesh ID *is* the key's hash.
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        let impostor = PeerId::from_noise_key(OTHER_NOISE_KEY);

        assert_eq!(
            preflight_announce(
                &packet,
                &impostor,
                &PeerId::from_bytes(&LOCAL),
                1_000_000,
                &AnnouncePreflightConfig::default(),
            ),
            Err(AnnounceRejection::SenderMismatch {
                derived: sender_peer()
            })
        );
    }

    #[test]
    fn an_announce_claiming_to_be_us_is_rejected() {
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        assert_eq!(
            preflight_announce(
                &packet,
                &sender_peer(),
                &sender_peer(),
                1_000_000,
                &AnnouncePreflightConfig::default(),
            ),
            Err(AnnounceRejection::SelfAnnounce)
        );
    }

    #[test]
    fn announce_staleness_is_the_fifteen_minute_window_and_one_sided() {
        let now = 10_000_000;
        let config = AnnouncePreflightConfig::default();
        let local = PeerId::from_bytes(&LOCAL);

        // Exactly on the window: fresh.
        let edge = announce_packet(now - DEFAULT_MAX_AGE_MILLIS, MESSAGE_TTL_DEFAULT, true);
        assert!(preflight_announce(&edge, &sender_peer(), &local, now, &config).is_ok());

        // One millisecond past it: stale, with the age reported.
        let stale = announce_packet(now - DEFAULT_MAX_AGE_MILLIS - 1, MESSAGE_TTL_DEFAULT, true);
        assert_eq!(
            preflight_announce(&stale, &sender_peer(), &local, now, &config),
            Err(AnnounceRejection::Stale {
                age_millis: DEFAULT_MAX_AGE_MILLIS + 1
            })
        );

        // A future announce is not stale by this policy — the ingress guard's
        // symmetric skew window is what catches those.
        let future = announce_packet(now + 60_000, MESSAGE_TTL_DEFAULT, true);
        assert!(preflight_announce(&future, &sender_peer(), &local, now, &config).is_ok());
    }

    // --- inbound trust ---------------------------------------------------

    #[test]
    fn trust_requires_a_signature_that_actually_verifies() {
        assert_eq!(
            evaluate_announce_trust(true, true, PinnedKeys::default(), NOISE_KEY, SIGNING_KEY),
            AnnounceTrust::Verified
        );
        assert_eq!(
            evaluate_announce_trust(false, false, PinnedKeys::default(), NOISE_KEY, SIGNING_KEY),
            AnnounceTrust::Unverified(AnnounceDistrust::MissingSignature)
        );
        assert_eq!(
            evaluate_announce_trust(true, false, PinnedKeys::default(), NOISE_KEY, SIGNING_KEY),
            AnnounceTrust::Unverified(AnnounceDistrust::InvalidSignature)
        );
    }

    #[test]
    fn a_key_that_contradicts_what_we_recorded_loses_even_with_a_good_signature() {
        // Order matters: the key check runs first, so a validly signed announce
        // carrying a swapped Noise key is reported as substitution rather than
        // silently accepted.
        assert_eq!(
            evaluate_announce_trust(
                true,
                true,
                PinnedKeys { noise: Some(OTHER_NOISE_KEY), signing: Some(SIGNING_KEY) },
                NOISE_KEY,
                SIGNING_KEY
            ),
            AnnounceTrust::Unverified(AnnounceDistrust::KeyMismatch)
        );
        // The same key we already had is no obstacle.
        assert_eq!(
            evaluate_announce_trust(
                true,
                true,
                PinnedKeys { noise: Some(NOISE_KEY), signing: Some(SIGNING_KEY) },
                NOISE_KEY,
                SIGNING_KEY
            ),
            AnnounceTrust::Verified
        );
    }

    /// §98 — the impersonation this check exists for.
    ///
    /// A peer's Noise public key is broadcast in the clear in every announce, and
    /// the peer ID is `SHA256(noise key)[..8]`, so it cannot be swapped without
    /// changing the ID. The signing key was bound to nothing: an attacker in
    /// radio range could replay a victim's Noise key under their OWN signing key,
    /// self-sign the announce, and the signature verified — against the
    /// attacker's key, which the packet itself supplied. The result was recorded
    /// as `Verified`.
    #[test]
    fn a_stolen_noise_key_under_a_new_signing_key_is_not_verified() {
        let attacker = evaluate_announce_trust(
            true,
            true, // the attacker's self-signature verifies: they hold that key
            PinnedKeys { noise: Some(NOISE_KEY), signing: Some(SIGNING_KEY) },
            NOISE_KEY,         // the victim's key, heard off the air
            OTHER_SIGNING_KEY, // the attacker's own
        );
        assert_eq!(
            attacker,
            AnnounceTrust::Unverified(AnnounceDistrust::SigningKeyMismatch),
            "a valid self-signature over a substituted signing key is the attack, not the defence"
        );

        // Before the peer is known, first contact still establishes the pin.
        assert_eq!(
            evaluate_announce_trust(
                true,
                true,
                PinnedKeys::default(),
                NOISE_KEY,
                OTHER_SIGNING_KEY
            ),
            AnnounceTrust::Verified,
            "trust on first use: with nothing pinned there is nothing to contradict"
        );
    }

    #[test]
    fn an_unverified_announce_is_never_treated_as_verified() {
        let local = PeerId::from_bytes(&LOCAL);
        let config = AnnouncePreflightConfig::default();
        let link = AnnounceLinkContext {
            has_peripheral_link: true,
            ..AnnounceLinkContext::default()
        };

        // Signed, direct, arriving on a link we hold — everything an attacker can
        // fake — but the signature does not verify.
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &local,
            1_000_000,
            &link,
            &config,
            |_, _| false,
        )
        .expect("preflight passes; trust is what fails");

        assert!(!accepted.is_verified());
        assert_eq!(
            accepted.trust.distrust(),
            Some(AnnounceDistrust::InvalidSignature)
        );
        // Nothing downstream may act on it.
        assert!(!accepted.should_upsert_peer());
        assert_eq!(accepted.connected_flag(), None);
        assert!(!accepted.should_update_topology());
        assert_eq!(accepted.topology_neighbors(), None);
        assert!(!accepted.should_persist_identity());
        assert!(!accepted.may_rebind_link());
        // The direct hint is still reported — it is just worthless on its own.
        assert!(accepted.direct_hint);
    }

    #[test]
    fn an_unsigned_announce_is_never_treated_as_verified() {
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, false);
        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnounceLinkContext::default(),
            &AnnouncePreflightConfig::default(),
            // A verifier that says yes to everything must not rescue an unsigned
            // packet: `has_signature` gates whether it is consulted at all.
            |_, _| true,
        )
        .expect("preflight passes");

        assert!(!accepted.is_verified());
        assert_eq!(
            accepted.trust.distrust(),
            Some(AnnounceDistrust::MissingSignature)
        );
        assert!(!accepted.should_persist_identity());
    }

    #[test]
    fn a_verified_announce_unlocks_the_registry_topology_and_identity_paths() {
        let neighbors = vec![[0x01; SENDER_ID_LEN], [0x02; SENDER_ID_LEN]];
        let announcement = AnnouncementPacket::new(
            "alice".to_string(),
            NOISE_KEY.to_vec(),
            SIGNING_KEY.to_vec(),
            Some(neighbors.clone()),
            None,
            None,
        );
        let packet = BitchatPacket {
            signature: Some([0x5A; SIGNATURE_LEN]),
            payload: announcement.encode().unwrap(),
            ..announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true)
        };

        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnounceLinkContext::default(),
            &AnnouncePreflightConfig::default(),
            |p, signing_key| {
                // The verifier is handed the packet and the key the announcement
                // carries — no key material is chosen here.
                assert_eq!(signing_key, SIGNING_KEY);
                p.signature.is_some()
            },
        )
        .expect("a verified announce");

        assert!(accepted.is_verified());
        assert!(accepted.should_upsert_peer());
        assert!(accepted.should_update_topology());
        assert_eq!(accepted.topology_neighbors(), Some(neighbors.as_slice()));
        assert!(accepted.should_persist_identity());
        assert!(accepted.may_rebind_link());
        assert_eq!(accepted.peer_id, sender_peer());
    }

    #[test]
    fn a_verified_relayed_announce_may_not_rebind_a_link() {
        // Verified proves identity; it says nothing about which link the peer is on.
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT - 1, true);
        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnounceLinkContext::default(),
            &AnnouncePreflightConfig::default(),
            |_, _| true,
        )
        .expect("a verified announce");

        assert!(accepted.is_verified());
        assert!(!accepted.direct_hint);
        assert!(!accepted.may_rebind_link());
        // Nor does a relayed announce alone imply presence.
        assert_eq!(accepted.connected_flag(), Some(false));
    }

    #[test]
    fn a_direct_announce_on_someone_elses_link_does_not_shortcut_to_connected() {
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        let contested = AnnounceLinkContext {
            link_bound_to_other_peer: true,
            ..AnnounceLinkContext::default()
        };

        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &contested,
            &AnnouncePreflightConfig::default(),
            |_, _| true,
        )
        .expect("a verified announce");

        // Direct is forgeable, so on a link that already belongs to someone else it
        // buys nothing: only a containment-checked rebind may promote this.
        assert!(accepted.direct_hint);
        assert_eq!(accepted.connected_flag(), Some(false));

        // With a real link of our own, the same announce does mark them present.
        let held = AnnounceLinkContext {
            link_bound_to_other_peer: true,
            has_central_link: true,
            ..AnnounceLinkContext::default()
        };
        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &held,
            &AnnouncePreflightConfig::default(),
            |_, _| true,
        )
        .expect("a verified announce");
        assert_eq!(accepted.connected_flag(), Some(true));
    }

    #[test]
    fn a_direct_announce_on_a_free_link_marks_the_peer_connected() {
        let packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        let accepted = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnounceLinkContext::default(),
            &AnnouncePreflightConfig::default(),
            |_, _| true,
        )
        .expect("a verified announce");

        assert_eq!(accepted.connected_flag(), Some(true));
    }

    #[test]
    fn preflight_rejections_short_circuit_before_verification_runs() {
        let verify_calls = Cell::new(0u32);
        let mut packet = announce_packet(1_000_000, MESSAGE_TTL_DEFAULT, true);
        packet.payload = vec![0x00];

        let result = evaluate_announce(
            &packet,
            &sender_peer(),
            &PeerId::from_bytes(&LOCAL),
            1_000_000,
            &AnnounceLinkContext::default(),
            &AnnouncePreflightConfig::default(),
            |_, _| {
                verify_calls.set(verify_calls.get() + 1);
                true
            },
        );

        assert!(result.is_err());
        // Signature verification is the expensive step; a malformed payload must
        // never be able to make us spend one.
        assert_eq!(verify_calls.get(), 0);
    }

    // --- response plan ---------------------------------------------------

    #[test]
    fn only_a_direct_announce_from_a_new_or_returning_peer_raises_connected() {
        let new_peer = AnnounceRegistryOutcome {
            is_new_peer: true,
            is_reconnected_peer: false,
        };
        let returning = AnnounceRegistryOutcome {
            is_new_peer: false,
            is_reconnected_peer: true,
        };
        let known = AnnounceRegistryOutcome::default();

        assert!(plan_announce_response(true, new_peer, false).notify_peer_connected);
        assert!(plan_announce_response(true, returning, false).notify_peer_connected);
        assert!(!plan_announce_response(true, known, false).notify_peer_connected);
        // Relayed: someone three hops away has not walked into the room.
        assert!(!plan_announce_response(false, new_peer, false).notify_peer_connected);
    }

    #[test]
    fn initial_sync_follows_the_connected_notification_exactly() {
        for direct in [true, false] {
            for outcome in [
                AnnounceRegistryOutcome {
                    is_new_peer: true,
                    is_reconnected_peer: false,
                },
                AnnounceRegistryOutcome {
                    is_new_peer: false,
                    is_reconnected_peer: true,
                },
                AnnounceRegistryOutcome::default(),
            ] {
                let plan = plan_announce_response(direct, outcome, true);
                assert_eq!(plan.notify_peer_connected, plan.schedule_initial_sync);
            }
        }
    }

    #[test]
    fn afterglow_fires_for_first_sightings_even_when_relayed() {
        let new_peer = AnnounceRegistryOutcome {
            is_new_peer: true,
            is_reconnected_peer: false,
        };
        // A first sighting is worth one extra announce regardless of hop count: it
        // pushes our presence a hop further toward whoever just appeared.
        assert!(plan_announce_response(false, new_peer, false).schedule_afterglow);
        assert!(
            !plan_announce_response(
                true,
                AnnounceRegistryOutcome {
                    is_new_peer: false,
                    is_reconnected_peer: true
                },
                false
            )
            .schedule_afterglow
        );
    }

    #[test]
    fn announce_back_is_the_callers_dedup_answer_passed_through() {
        let outcome = AnnounceRegistryOutcome::default();
        assert!(plan_announce_response(false, outcome, true).schedule_announce_back);
        assert!(!plan_announce_response(true, outcome, false).schedule_announce_back);
        assert_eq!(
            announce_back_dedup_id(&PeerId::from_bytes(&LOCAL)),
            "announce-back-0011223344556677"
        );
    }

    #[test]
    fn the_afterglow_delay_stays_inside_its_window() {
        assert_eq!(afterglow_delay_millis(|low, _| low), AFTERGLOW_MIN_MILLIS);
        assert_eq!(afterglow_delay_millis(|_, high| high), AFTERGLOW_MAX_MILLIS);
        assert_eq!(afterglow_delay_millis(|_, _| 450), 450);
        assert_eq!(afterglow_delay_millis(|_, _| 0), AFTERGLOW_MIN_MILLIS);
        assert_eq!(afterglow_delay_millis(|_, _| u64::MAX), AFTERGLOW_MAX_MILLIS);
    }
}
