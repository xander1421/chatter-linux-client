//! Which physical connection belongs to which peer identity.
//!
//! Ported from `BLELinkStateStore.swift`, `BLEPeerRegistry.swift` and
//! `BLEIngressLinkRegistry.swift`, together with the rebind guards that live in
//! `BLEService.rebindLinkAfterVerifiedDirectAnnounce`. Three types, one concern
//! each:
//!
//! - [`LinkStateStore`] — link↔peer bindings, both directions, plus the guarded
//!   rebind path.
//! - [`PeerRegistry`] — per-peer records, last-seen, staleness eviction.
//! - [`IngressRegistry`] — which link a packet arrived on, for split-horizon.
//!
//! # Why the binding is security-critical
//!
//! A [`LinkId`] is a physical connection. A [`PeerId`] is a claimed identity. The
//! mapping between them is the only thing that makes "this packet came from Bob"
//! mean anything, and it is established from data an attacker in radio range can
//! replay.
//!
//! bitchat signs its announces but **excludes the TTL from the signature**,
//! because relays have to decrement it. So "this announce arrived directly, one
//! hop, from the device on the other end of this link" is an *unauthenticated*
//! property: anyone can capture Bob's announce, restore its TTL to the origin
//! value and emit it on their own link, and it will verify and look direct.
//!
//! Every guard here exists to bound what that forgery buys. The design rule is
//! that the type system, not caller discipline, enforces them:
//!
//! - An unverified announce reaches only [`LinkStateStore::bind_if_unbound`],
//!   which refuses to move an already-bound link.
//! - Moving a bound link requires a [`VerifiedDirectAnnounce`], which cannot be
//!   constructed without asserting both signature verification and directness.
//! - Even then [`LinkStateStore::rebind_after_verified_direct`] applies the
//!   containment checks: no stealing an identity that already owns a live link,
//!   and at most one rebind per link per [`LINK_REBIND_COOLDOWN_MS`] window.
//!
//! # Sans-IO
//!
//! No clocks and no async. Every time-dependent entry point takes
//! `now_unix_millis`. Elapsed times are computed with `saturating_sub`, so a
//! caller whose clock steps backwards sees "zero elapsed" — which refuses a
//! rebind and keeps a peer reachable, the conservative direction in both cases.

use std::collections::{BTreeMap, BTreeSet};

use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BitchatPacket, MessageType};
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::capabilities::PeerCapabilities;

use crate::link::{LinkId, LinkRole};

/// `TransportConfig.bleReachabilityRetentionVerifiedSeconds` (60 s).
///
/// How long a *verified* peer stays reachable after its last sighting. Verified
/// means we checked a signature against the signing key in its announce, so the
/// longer window cannot be claimed by simply asserting a nickname.
pub const REACHABILITY_RETENTION_VERIFIED_MS: u64 = 60_000;

/// `TransportConfig.bleReachabilityRetentionUnverifiedSeconds` (45 s).
pub const REACHABILITY_RETENTION_UNVERIFIED_MS: u64 = 45_000;

/// `TransportConfig.blePeerInactivityTimeoutSeconds` (8 s).
///
/// A *connected* peer that has said nothing for this long is demoted to
/// disconnected — but only when it also has no live link, because an idle link
/// is not a dead one.
pub const PEER_INACTIVITY_TIMEOUT_MS: u64 = 8_000;

/// `TransportConfig.bleIngressRecordLifetimeSeconds` (3 s).
///
/// Ingress records exist to suppress the echo of a packet back down the link it
/// arrived on, which happens within milliseconds of receiving it. They are
/// deliberately short-lived: this is not a routing table, and a long-lived
/// link↔packet memory would be another surface for a replay to steer.
pub const INGRESS_RECORD_LIFETIME_MS: u64 = 3_000;

/// `TransportConfig.bleLinkRebindCooldownSeconds` (60 s).
///
/// At most one identity rotation per link per window. Without it, two identities
/// replaying each other's announces could flip a single link back and forth
/// indefinitely, and every flip is an opportunity to receive traffic addressed
/// to whoever currently owns it.
pub const LINK_REBIND_COOLDOWN_MS: u64 = 60_000;

/// How long a peer stays reachable after its last sighting.
pub fn reachability_retention_ms(verified: bool) -> u64 {
    if verified {
        REACHABILITY_RETENTION_VERIFIED_MS
    } else {
        REACHABILITY_RETENTION_UNVERIFIED_MS
    }
}

/// Whether a packet is an announce that arrived without being relayed.
///
/// `direct_announce_ttl` is the origination TTL — an announce still carrying it
/// has not been decremented by any relay. **This is not authenticated**: the TTL
/// is excluded from the signature preimage so relays can decrement it, which
/// means a replayer can restore it. Treat the answer as a hint that gates
/// further checks, never as proof of who is on the other end of the link.
pub fn is_direct_announce(packet: &BitchatPacket, direct_announce_ttl: u8) -> bool {
    packet.message_type() == Some(MessageType::Announce) && packet.ttl == direct_announce_ttl
}

// ---------------------------------------------------------------------------
// Link state
// ---------------------------------------------------------------------------

/// Which roles a peer is currently reachable on.
///
/// **Naming differs from Swift deliberately.** `BLEDirectLinkState.hasPeripheral`
/// means "we hold a connection to their peripheral", i.e. *we* are the central;
/// `hasCentral` means "a central subscribed to us", i.e. *we* are the peripheral.
/// Reading those names as our own role is backwards, and the mistake silently
/// sends directed traffic down the wrong path. These fields are named for *our*
/// role, matching [`LinkRole`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkPresence {
    /// We hold at least one outbound link to this peer.
    pub as_central: bool,
    /// This peer holds at least one inbound link to us.
    pub as_peripheral: bool,
}

impl LinkPresence {
    /// Any live link at all, in either role.
    pub fn any(&self) -> bool {
        self.as_central || self.as_peripheral
    }
}

/// Everything the policy layer knows about one live link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkState {
    /// Which side of this link we are.
    pub role: LinkRole,
    /// The identity bound to it, once one has claimed it.
    pub peer: Option<PeerId>,
    /// Whether the link can currently carry a write: the characteristic has been
    /// discovered (central) or a subscription is active (peripheral). A link can
    /// be up and not writable, and offering it to the planner in that state
    /// silently drops whatever gets routed to it.
    pub writable: bool,
    /// The identity that completed a Noise handshake *on this physical link*.
    ///
    /// Peer-level Noise sessions outlive the links they were made on, so a
    /// session alone does not prove the device currently on this link is that
    /// peer. This is the per-link proof, and it is dropped on any rebind.
    pub noise_authenticated_owner: Option<PeerId>,
}

impl LinkState {
    fn new(role: LinkRole) -> Self {
        Self {
            role,
            peer: None,
            writable: false,
            noise_authenticated_owner: None,
        }
    }
}

/// A signature-verified announce that also arrived direct.
///
/// The only key that opens [`LinkStateStore::rebind_after_verified_direct`]. It
/// exists as a type rather than a pair of `bool` parameters because a rebind is
/// the single operation that can hand one identity another identity's link, and
/// "did anyone actually check the signature?" must not be answerable by reading
/// the call site's argument order.
///
/// Constructing one asserts the signature verified against the signing key the
/// announce carries. It does **not** assert directness is genuine — nothing can,
/// since the TTL is unsigned. That is why the store still applies containment
/// checks to every rebind a valid witness requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDirectAnnounce {
    peer: PeerId,
}

impl VerifiedDirectAnnounce {
    /// `Some` only when the announce's signature verified *and* it arrived with
    /// its origination TTL intact. `None` otherwise — an unverified or relayed
    /// announce has no rebinding power at all.
    pub fn new(peer: PeerId, signature_verified: bool, arrived_direct: bool) -> Option<Self> {
        if signature_verified && arrived_direct {
            Some(Self { peer })
        } else {
            None
        }
    }

    /// Builds the witness straight from a packet, checking directness here.
    pub fn from_packet(
        peer: PeerId,
        packet: &BitchatPacket,
        signature_verified: bool,
        direct_announce_ttl: u8,
    ) -> Option<Self> {
        Self::new(
            peer,
            signature_verified,
            is_direct_announce(packet, direct_announce_ttl),
        )
    }

    /// The identity claiming the link.
    pub fn peer(&self) -> &PeerId {
        &self.peer
    }
}

/// What a first-time bind did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindOutcome {
    /// The link had no owner and now has one.
    Bound,
    /// The link was already bound to this same identity. No change beyond
    /// refreshing which link is preferred for directed sends.
    AlreadyBound,
    /// The link belongs to someone else. Moving it needs a verified direct
    /// announce and the containment checks — see
    /// [`LinkStateStore::rebind_after_verified_direct`].
    RefusedBoundToOtherPeer(PeerId),
    /// No such live link.
    UnknownLink,
}

/// A rebind that happened, and the cleanup it implies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebind {
    /// The link that changed hands.
    pub link: LinkId,
    /// Who owned it before.
    pub previous_peer: PeerId,
    /// Who owns it now.
    pub new_peer: PeerId,
    /// Outbound links that were still bound to `previous_peer` and have been
    /// **removed from the store already**. They are stale duplicates of the same
    /// physical device — its old connections outliving the relaunch that rotated
    /// its ID — and the caller must cancel them on the radio, or they keep
    /// spraying duplicate traffic until the inactivity timeout.
    pub retired_links: Vec<LinkId>,
    /// True when `previous_peer` has no links left. The caller should drop that
    /// identity from the [`PeerRegistry`] rather than let a ghost linger for the
    /// whole reachability window; a rotation is an implicit leave.
    pub retire_previous_peer: bool,
}

/// Why a rebind was refused. Each variant is a bound on what a replayed announce
/// can accomplish, so refusals are worth logging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebindRefusal {
    /// No such live link.
    UnknownLink,
    /// The link has no current owner — that is a first bind, not a rebind. Use
    /// [`LinkStateStore::bind_if_unbound`].
    LinkUnbound,
    /// The claimed identity already owns this link. Nothing to do.
    SamePeer,
    /// The claimed identity already owns another live link.
    ///
    /// Bounds identity theft: a replayer can at worst take over a link the
    /// victim was not already using, never displace the victim's real
    /// connection. Without this check, a replayed announce would move the
    /// victim's identity onto the attacker's link while the victim's own link
    /// still existed, silently redirecting directed traffic.
    IdentityOwnsAnotherLink,
    /// This link was already rebound inside the cooldown window.
    ///
    /// Bounds flip-flopping: two identities replaying each other cannot fight
    /// over one link, because the second rebind within the window is refused.
    CooldownActive {
        /// Milliseconds until this link may be rebound again.
        remaining_ms: u64,
    },
}

impl core::fmt::Display for RebindRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownLink => f.write_str("no such live link"),
            Self::LinkUnbound => f.write_str("link has no previous owner"),
            Self::SamePeer => f.write_str("link already bound to this identity"),
            Self::IdentityOwnsAnotherLink => {
                f.write_str("identity already owns another live link")
            }
            Self::CooldownActive { remaining_ms } => {
                write!(f, "rebind cooldown active for {remaining_ms} ms")
            }
        }
    }
}

/// The result of asking for a rebind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebindOutcome {
    Rebound(Rebind),
    Refused(RebindRefusal),
}

#[cfg(test)]
impl RebindOutcome {
    fn rebound(&self) -> Option<&Rebind> {
        match self {
            Self::Rebound(r) => Some(r),
            Self::Refused(_) => None,
        }
    }

    fn refusal(&self) -> Option<&RebindRefusal> {
        match self {
            Self::Rebound(_) => None,
            Self::Refused(r) => Some(r),
        }
    }
}

/// Link↔peer bindings in both directions.
///
/// Only **live** links are held: one is added when the radio reports it
/// connected and removed when it reports it gone. That makes "does this identity
/// already own a live link?" a plain lookup, which is the question the rebind
/// guard turns on.
///
/// Ported from `BLELinkStateStore`, collapsing its two parallel worlds — a
/// peripheral map for links we opened and a central map for links opened to us —
/// into one map keyed by [`LinkId`], with [`LinkRole`] carrying the distinction.
/// The Swift split exists because CoreBluetooth hands back two unrelated object
/// types; `LinkId` is already the common handle, so the split would be pure
/// duplication here.
///
/// Not `Sync`-guarded and not internally locked, matching Swift: the caller owns
/// it on one task. Rust makes that structural rather than a debug assertion.
#[derive(Debug, Default)]
pub struct LinkStateStore {
    links: BTreeMap<LinkId, LinkState>,
    /// Peer → the outbound link directed sends should prefer. Mirrors
    /// `peerToPeripheralUUID`; see [`LinkStateStore::preferred_links`].
    preferred: BTreeMap<PeerId, LinkId>,
    /// Link → when it was last rebound. Mirrors `BLEService.lastLinkRebindAt`.
    last_rebind_ms: BTreeMap<LinkId, u64>,
}

impl LinkStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a link the radio just reported connected.
    ///
    /// Idempotent: re-reporting an existing link keeps its binding, writability
    /// and Noise proof, since a duplicate `Connected` event is not evidence that
    /// anything about the link changed.
    pub fn link_up(&mut self, link: LinkId, role: LinkRole) {
        self.links.entry(link).or_insert_with(|| LinkState::new(role));
    }

    /// Drops a link the radio reported gone. Returns the identity that owned it.
    ///
    /// The rebind cooldown for this link is deliberately **not** cleared: link
    /// identifiers are stable per device, so clearing it would let a replayer
    /// reset the cooldown by forcing a reconnect.
    pub fn link_down(&mut self, link: &LinkId) -> Option<PeerId> {
        self.drop_link(link)
    }

    pub fn contains(&self, link: &LinkId) -> bool {
        self.links.contains_key(link)
    }

    pub fn state(&self, link: &LinkId) -> Option<&LinkState> {
        self.links.get(link)
    }


    /// Every live link, in a stable order.
    pub fn links(&self) -> impl Iterator<Item = (&LinkId, &LinkState)> {
        self.links.iter()
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// How many live links we hold in a given role. The central count is what the
    /// connection budget is spent against.
    pub fn count_role(&self, role: LinkRole) -> usize {
        self.links.values().filter(|s| s.role == role).count()
    }

    /// Marks whether a link can currently carry a write. Returns false for an
    /// unknown link.
    pub fn set_writable(&mut self, link: &LinkId, writable: bool) -> bool {
        match self.links.get_mut(link) {
            Some(state) => {
                state.writable = writable;
                true
            }
            None => false,
        }
    }

    pub fn is_writable(&self, link: &LinkId) -> bool {
        self.links.get(link).is_some_and(|s| s.writable)
    }

    /// The identity bound to a link, if any.
    pub fn peer_for(&self, link: &LinkId) -> Option<&PeerId> {
        self.links.get(link).and_then(|s| s.peer.as_ref())
    }

    /// Every live link bound to an identity.
    ///
    /// Scans rather than consulting the reverse map, matching Swift: one peer can
    /// legitimately hold several links at once (a device reappears under a fresh
    /// handle while its previous connection lives on), and a 1:1 reverse map
    /// would under-report exactly when the rebind guard needs the true answer.
    pub fn links_to(&self, peer: &PeerId) -> BTreeSet<LinkId> {
        self.links
            .iter()
            .filter(|(_, s)| s.peer.as_ref() == Some(peer))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Which roles this peer is reachable on right now.
    pub fn presence(&self, peer: &PeerId) -> LinkPresence {
        let mut presence = LinkPresence::default();
        for state in self.links.values() {
            if state.peer.as_ref() != Some(peer) {
                continue;
            }
            match state.role {
                LinkRole::Central => presence.as_central = true,
                LinkRole::Peripheral => presence.as_peripheral = true,
            }
        }
        presence
    }

    /// Presence for every bound identity — the input
    /// [`PeerRegistry::reconcile_connectivity`] needs.
    pub fn presence_by_peer(&self) -> BTreeMap<PeerId, LinkPresence> {
        let mut out: BTreeMap<PeerId, LinkPresence> = BTreeMap::new();
        for state in self.links.values() {
            let Some(peer) = state.peer.as_ref() else {
                continue;
            };
            let entry = out.entry(peer.clone()).or_default();
            match state.role {
                LinkRole::Central => entry.as_central = true,
                LinkRole::Peripheral => entry.as_peripheral = true,
            }
        }
        out
    }


    /// The whole preferred map, for the fanout selector.
    pub fn preferred_links(&self) -> &BTreeMap<PeerId, LinkId> {
        &self.preferred
    }

    /// Binds an identity to a link that has no owner yet.
    ///
    /// This is the path an **unverified** announce takes, so it must never move a
    /// link that already belongs to someone: the only claim it carries is the
    /// sender ID in the header, which anyone can write. A bound link changes
    /// hands only through [`Self::rebind_after_verified_direct`].
    pub fn bind_if_unbound(&mut self, link: &LinkId, peer: &PeerId) -> BindOutcome {
        let Some(state) = self.links.get(link) else {
            return BindOutcome::UnknownLink;
        };
        match state.peer.clone() {
            Some(existing) if &existing == peer => {
                // Re-affirming keeps the preferred link pointing at the most
                // recently evidenced one, which is what makes collapse stable
                // when a peer holds duplicates.
                self.set_preferred_if_central(link, peer);
                BindOutcome::AlreadyBound
            }
            Some(existing) => BindOutcome::RefusedBoundToOtherPeer(existing),
            None => {
                if let Some(state) = self.links.get_mut(link) {
                    state.peer = Some(peer.clone());
                }
                self.set_preferred_if_central(link, peer);
                BindOutcome::Bound
            }
        }
    }


    /// Moves a bound link to a new identity, applying every containment check.
    ///
    /// The legitimate case: a peer relaunches, rotates its ephemeral peer ID, and
    /// the still-open connection keeps pointing at the retired ID. Until the
    /// binding heals, the peer appears twice and its directed traffic on this
    /// link is dropped as spoofed.
    ///
    /// The attack this is shaped around: directness is unauthenticated, so a
    /// replayed announce with a restored TTL is indistinguishable from a genuine
    /// direct one. A [`VerifiedDirectAnnounce`] is therefore necessary but not
    /// sufficient, and three further checks run in order:
    ///
    /// 1. **The link must already have a different owner.** An unowned link is a
    ///    first bind, and re-claiming your own link is a no-op. Neither is a
    ///    transfer of ownership.
    /// 2. **The claimed identity must not already own a live link**
    ///    ([`RebindRefusal::IdentityOwnsAnotherLink`]).
    /// 3. **This link must not have been rebound inside the cooldown window**
    ///    ([`RebindRefusal::CooldownActive`]).
    ///
    /// On success the link's Noise proof is dropped: that proof belongs to the
    /// *old* binding, and carrying it across an announce-driven rebind would let
    /// a replay inherit an authentication the new owner never performed. The new
    /// owner must handshake again.
    pub fn rebind_after_verified_direct(
        &mut self,
        link: &LinkId,
        announce: &VerifiedDirectAnnounce,
        now_unix_millis: u64,
    ) -> RebindOutcome {
        let peer = announce.peer();

        let Some(state) = self.links.get(link) else {
            return RebindOutcome::Refused(RebindRefusal::UnknownLink);
        };
        let Some(previous) = state.peer.clone() else {
            return RebindOutcome::Refused(RebindRefusal::LinkUnbound);
        };
        if &previous == peer {
            return RebindOutcome::Refused(RebindRefusal::SamePeer);
        }

        // Guard 1: never steal an identity that is already live somewhere else.
        if !self.links_to(peer).is_empty() {
            return RebindOutcome::Refused(RebindRefusal::IdentityOwnsAnotherLink);
        }

        // Guard 2: one rebind per link per window. Expired entries are dropped
        // first so the map cannot grow without bound, exactly as the Swift filter
        // does — and so the boundary is "older than the window is forgotten".
        self.last_rebind_ms
            .retain(|_, at| now_unix_millis.saturating_sub(*at) < LINK_REBIND_COOLDOWN_MS);
        if let Some(at) = self.last_rebind_ms.get(link) {
            let elapsed = now_unix_millis.saturating_sub(*at);
            return RebindOutcome::Refused(RebindRefusal::CooldownActive {
                remaining_ms: LINK_REBIND_COOLDOWN_MS.saturating_sub(elapsed),
            });
        }
        self.last_rebind_ms.insert(link.clone(), now_unix_millis);

        if let Some(state) = self.links.get_mut(link) {
            // The proof belonged to the previous owner's physical binding.
            state.noise_authenticated_owner = None;
            state.peer = Some(peer.clone());
        }
        if self.preferred.get(&previous) == Some(link) {
            self.preferred.remove(&previous);
        }
        self.set_preferred_if_central(link, peer);

        // Outbound links still bound to the retired identity are stale duplicates
        // of the same device. Retire them here so the caller can cancel them;
        // inbound links are the peer's own to drop, and it runs this same policy.
        let retiring: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(id, s)| {
                *id != link && s.role == LinkRole::Central && s.peer.as_ref() == Some(&previous)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &retiring {
            let _ = self.drop_link(id);
        }

        let retire_previous_peer = self.links_to(&previous).is_empty();

        RebindOutcome::Rebound(Rebind {
            link: link.clone(),
            previous_peer: previous,
            new_peer: peer.clone(),
            retired_links: retiring,
            retire_previous_peer,
        })
    }

    /// Records that `peer` completed a Noise handshake on this link.
    ///
    /// Returns false for an unknown link. The proof is per-link on purpose: a
    /// peer-level session survives the connection it was made on, so using one to
    /// authorise traffic on a *different* link would accept exactly the state a
    /// successful rebind attack produces.
    pub fn mark_noise_authenticated(&mut self, link: &LinkId, peer: &PeerId) -> bool {
        match self.links.get_mut(link) {
            Some(state) => {
                state.noise_authenticated_owner = Some(peer.clone());
                true
            }
            None => false,
        }
    }

    pub fn clear_noise_authentication(&mut self, link: &LinkId) {
        if let Some(state) = self.links.get_mut(link) {
            state.noise_authenticated_owner = None;
        }
    }

    pub fn noise_authenticated_owner(&self, link: &LinkId) -> Option<&PeerId> {
        self.links
            .get(link)
            .and_then(|s| s.noise_authenticated_owner.as_ref())
    }

    /// Whether this link both *belongs* to `peer` and carries `peer`'s Noise
    /// proof. Both halves are required: a rebind clears the proof, and an
    /// unbound-but-proved link is not somewhere to send directed traffic.
    pub fn is_noise_authenticated(&self, link: &LinkId, peer: &PeerId) -> bool {
        self.links.get(link).is_some_and(|s| {
            s.peer.as_ref() == Some(peer) && s.noise_authenticated_owner.as_ref() == Some(peer)
        })
    }

    /// Live links that are both bound to `peer` and Noise-authenticated by it.
    pub fn noise_authenticated_links_to(&self, peer: &PeerId) -> BTreeSet<LinkId> {
        self.links
            .iter()
            .filter(|(_, s)| {
                s.peer.as_ref() == Some(peer) && s.noise_authenticated_owner.as_ref() == Some(peer)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Drops every link, e.g. when the adapter powers off. Returns the identities
    /// that lost their last link.
    ///
    /// Rebind cooldowns survive, for the same reason [`Self::link_down`] keeps
    /// them: an adapter bounce must not be a way to reset the guard.
    pub fn clear(&mut self) -> BTreeSet<PeerId> {
        let peers = self
            .links
            .values()
            .filter_map(|s| s.peer.clone())
            .collect::<BTreeSet<_>>();
        self.links.clear();
        self.preferred.clear();
        peers
    }

    fn drop_link(&mut self, link: &LinkId) -> Option<PeerId> {
        let removed = self.links.remove(link)?;
        let peer = removed.peer?;
        self.repair_preferred(&peer, link);
        Some(peer)
    }

    /// Keeps the preferred map pointing at a link that still exists.
    ///
    /// Only repairs when it pointed at the departing link: with duplicate links to
    /// one peer, removing a stale duplicate must not strand the surviving one. A
    /// writable survivor wins, because repairing onto a link that is still
    /// discovering its characteristic strands directed sends until it comes back.
    fn repair_preferred(&mut self, peer: &PeerId, removed: &LinkId) {
        if self.preferred.get(peer) != Some(removed) {
            return;
        }
        let survivors: Vec<(&LinkId, &LinkState)> = self
            .links
            .iter()
            .filter(|(id, s)| {
                *id != removed && s.role == LinkRole::Central && s.peer.as_ref() == Some(peer)
            })
            .collect();
        let chosen = survivors
            .iter()
            .find(|(_, s)| s.writable)
            .or_else(|| survivors.first())
            .map(|(id, _)| (*id).clone());
        match chosen {
            Some(id) => {
                self.preferred.insert(peer.clone(), id);
            }
            None => {
                self.preferred.remove(peer);
            }
        }
    }

    fn set_preferred_if_central(&mut self, link: &LinkId, peer: &PeerId) {
        if self.links.get(link).map(|s| s.role) == Some(LinkRole::Central) {
            self.preferred.insert(peer.clone(), link.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Peer registry
// ---------------------------------------------------------------------------

/// What we know about one peer. Ported from `BLEPeerInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRecord {
    pub peer: PeerId,
    pub nickname: String,
    /// Live on the mesh right now. Distinct from reachable: a peer stays
    /// reachable for a while after its last link goes.
    pub connected: bool,
    pub noise_public_key: Option<Vec<u8>>,
    pub signing_public_key: Option<Vec<u8>>,
    /// The nickname arrived inside an announce whose signature we checked.
    /// Buys the longer reachability retention, and nothing else.
    pub verified_nickname: bool,
    pub last_seen_ms: u64,
    pub capabilities: PeerCapabilities,
    /// Rendezvous cell from the announce, when it advertises
    /// [`PeerCapabilities::BRIDGE`].
    pub bridge_geohash: Option<String>,
}

/// What an announce changed. Ported from `BLEPeerAnnounceUpdate`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnounceUpdate {
    pub is_new_peer: bool,
    /// The peer existed and was marked disconnected — i.e. this announce is a
    /// reconnection, which the UI reports differently from a first sighting.
    pub was_disconnected: bool,
    pub previous_nickname: Option<String>,
    /// This announce established the pin, so the caller has something new to
    /// persist.
    pub pinned_now: bool,
}

/// The keys a peer ID was first seen with.
///
/// Held apart from [`PeerRecord`] because the two have opposite lifetimes. A
/// record is presence: it ages out, and a peer that leaves is forgotten. A pin
/// is identity: if it went the same way, anything that makes us forget a peer
/// — silence, a restart, a `Leave` — would also reopen trust on first use for
/// its ID, and the next announce to claim it would be believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedIdentity {
    pub noise_public_key: Vec<u8>,
    pub signing_public_key: Vec<u8>,
    /// The nickname at first sight. Display only, for naming the peer in a
    /// key-change warning after its record is gone.
    pub nickname: String,
    pub pinned_at_ms: u64,
}

/// How many pins are kept.
///
/// When full, new peers are not pinned and existing pins are untouched. The
/// other choice, evicting the oldest, would let anyone in radio range push a
/// victim's pin out by announcing throwaway identities. The ceiling is a heavy
/// user eventually meeting more distinct peers than this; expiring pins unseen
/// for months is the upgrade, and needs a last-seen write this does not do.
pub const MAX_PINNED_IDENTITIES: usize = 4096;

/// First byte of the persisted pin set.
const PIN_BLOB_VERSION: u8 = 1;

/// The persisted pin set could not be read. Carries nothing: the only thing a
/// caller can do is refuse to start from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedPins;

/// A peer that aged out. Ported from `BLERemovedPeer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedPeer {
    pub peer: PeerId,
    pub nickname: String,
}

/// What one reconciliation pass changed. Ported from `BLEPeerConnectivityChanges`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectivityChanges {
    /// Demoted from connected to merely reachable.
    pub disconnected: Vec<PeerId>,
    /// Aged past retention and dropped entirely.
    pub removed: Vec<RemovedPeer>,
}

impl ConnectivityChanges {
    pub fn is_empty(&self) -> bool {
        self.disconnected.is_empty() && self.removed.is_empty()
    }
}

/// Per-peer records: who exists, when we last heard from them, whether their
/// nickname is verified. Ported from `BLEPeerRegistry`.
///
/// A `BTreeMap` rather than a hash map so eviction and snapshot order are stable
/// — the reconciliation pass emits events, and event order that depends on hash
/// seeding is a class of test flake worth not having.
#[derive(Clone, Debug, Default)]
pub struct PeerRegistry {
    peers: BTreeMap<PeerId, PeerRecord>,
    /// Outlives `peers` on purpose — see [`PinnedIdentity`]. Nothing that
    /// forgets a peer touches it.
    pins: BTreeMap<PeerId, PinnedIdentity>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }


    pub fn records(&self) -> impl Iterator<Item = &PeerRecord> {
        self.peers.values()
    }

    pub fn info(&self, peer: &PeerId) -> Option<&PeerRecord> {
        self.peers.get(peer)
    }

    /// How many peers are live right now. This is the mesh degree, and the relay
    /// policy's fanout decisions are scaled by it.
    pub fn connected_count(&self) -> usize {
        self.peers.values().filter(|p| p.connected).count()
    }

    pub fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.connected)
            .map(|p| p.peer.clone())
            .collect()
    }


    /// Lookups that a caller may reach with a full Noise key go through
    /// [`PeerId::to_short`], matching Swift's `peers[peerID.toShort()]`.
    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.peers
            .get(&peer.to_short())
            .is_some_and(|p| p.connected)
    }


    /// Whether we can still plausibly route to this peer.
    ///
    /// Three tiers, and the middle one matters: a connected peer is reachable; a
    /// remembered peer is reachable only while we are *attached to the mesh at
    /// all*, because with no links there is no path for the memory to be worth
    /// anything; otherwise the memory expires on a retention window that is
    /// longer for verified peers.
    ///
    /// The verified/unverified split is why the retention constants exist
    /// separately: an unverified record is a nickname anyone could have claimed,
    /// so it decays faster.
    pub fn is_reachable(&self, peer: &PeerId, now_unix_millis: u64) -> bool {
        let Some(record) = self.peers.get(&peer.to_short()) else {
            return false;
        };
        if record.connected {
            return true;
        }
        if self.connected_count() == 0 {
            return false;
        }
        let age = now_unix_millis.saturating_sub(record.last_seen_ms);
        age <= reachability_retention_ms(record.verified_nickname)
    }

    pub fn nickname(&self, peer: &PeerId, connected_only: bool) -> Option<&str> {
        let record = self.peers.get(peer)?;
        if connected_only && !record.connected {
            return None;
        }
        Some(record.nickname.as_str())
    }

    pub fn upsert(&mut self, record: PeerRecord) {
        self.peers.insert(record.peer.clone(), record);
    }

    pub fn remove(&mut self, peer: &PeerId) -> Option<PeerRecord> {
        self.peers.remove(peer)
    }

    pub fn remove_all(&mut self) {
        self.peers.clear();
    }

    /// The keys this peer ID was first seen with, whether or not the peer is
    /// still in the registry.
    pub fn pinned(&self, peer: &PeerId) -> Option<&PinnedIdentity> {
        self.pins.get(peer)
    }

    pub fn can_pin(&self, peer: &PeerId) -> bool {
        self.pins.contains_key(peer) || self.pins.len() < MAX_PINNED_IDENTITIES
    }

    /// Forget every pin. For a panic wipe, and nothing else.
    pub fn clear_pins(&mut self) {
        self.pins.clear();
    }

    /// The pin set, for the caller to seal and persist.
    ///
    /// Every field is length-prefixed with one byte, so a nickname is cut at
    /// 255 bytes — it is display text, and the announce TLV cannot carry more.
    pub fn pins_to_bytes(&self) -> Vec<u8> {
        fn field(out: &mut Vec<u8>, bytes: &[u8]) {
            let bytes = &bytes[..bytes.len().min(usize::from(u8::MAX))];
            out.push(bytes.len() as u8);
            out.extend_from_slice(bytes);
        }

        let mut out = vec![PIN_BLOB_VERSION];
        for (peer, pin) in &self.pins {
            field(&mut out, peer.as_str().as_bytes());
            field(&mut out, &pin.noise_public_key);
            field(&mut out, &pin.signing_public_key);
            field(&mut out, pin.nickname.as_bytes());
            out.extend_from_slice(&pin.pinned_at_ms.to_be_bytes());
        }
        out
    }

    /// Load a persisted pin set, keeping any pin already held.
    ///
    /// All or nothing. A blob that does not parse to its last byte changes
    /// nothing, because half a pin set is a set with holes an attacker may
    /// have chosen.
    pub fn restore_pins(&mut self, bytes: &[u8]) -> Result<(), MalformedPins> {
        fn field<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], MalformedPins> {
            let (&len, tail) = rest.split_first().ok_or(MalformedPins)?;
            let (value, tail) = tail
                .split_at_checked(usize::from(len))
                .ok_or(MalformedPins)?;
            *rest = tail;
            Ok(value)
        }

        let Some((&PIN_BLOB_VERSION, mut rest)) = bytes.split_first() else {
            return Err(MalformedPins);
        };
        let mut loaded = Vec::new();
        while !rest.is_empty() {
            let peer = std::str::from_utf8(field(&mut rest)?).map_err(|_| MalformedPins)?;
            let noise_public_key = field(&mut rest)?.to_vec();
            let signing_public_key = field(&mut rest)?.to_vec();
            let nickname = String::from_utf8_lossy(field(&mut rest)?).into_owned();
            let (stamp, tail) = rest.split_first_chunk::<8>().ok_or(MalformedPins)?;
            rest = tail;
            // A pin filed under an ID its own key does not derive to could
            // never have been recorded by `on_announce`, so the blob is not
            // one we wrote.
            if PeerId::from_noise_key(&noise_public_key).as_str() != peer {
                return Err(MalformedPins);
            }
            loaded.push((
                PeerId::from_str(peer),
                PinnedIdentity {
                    noise_public_key,
                    signing_public_key,
                    nickname,
                    pinned_at_ms: u64::from_be_bytes(*stamp),
                },
            ));
        }
        for (peer, pin) in loaded {
            if self.pins.len() >= MAX_PINNED_IDENTITIES {
                break;
            }
            self.pins.entry(peer).or_insert(pin);
        }
        Ok(())
    }


    pub fn update_last_seen(&mut self, peer: &PeerId, now_unix_millis: u64) {
        if let Some(record) = self.peers.get_mut(peer) {
            record.last_seen_ms = now_unix_millis;
        }
    }

    /// Records a peer from an announce whose signature we verified.
    ///
    /// Replaces the record wholesale rather than merging fields, matching Swift:
    /// the announce is the peer's own current statement about itself, and keeping
    /// stale fields around would mean a nickname or capability that no announce
    /// still claims.
    pub fn upsert_verified_announce(
        &mut self,
        peer: &PeerId,
        announcement: &AnnouncementPacket,
        connected: bool,
        now_unix_millis: u64,
    ) -> AnnounceUpdate {
        let pinned_now =
            !self.pins.contains_key(peer) && self.pins.len() < MAX_PINNED_IDENTITIES;
        if pinned_now {
            self.pins.insert(
                peer.clone(),
                PinnedIdentity {
                    noise_public_key: announcement.noise_public_key.clone(),
                    signing_public_key: announcement.signing_public_key.clone(),
                    nickname: announcement.nickname.clone(),
                    pinned_at_ms: now_unix_millis,
                },
            );
        }

        let existing = self.peers.get(peer);
        let update = AnnounceUpdate {
            is_new_peer: existing.is_none(),
            was_disconnected: existing.is_some_and(|e| !e.connected),
            previous_nickname: existing.map(|e| e.nickname.clone()),
            pinned_now,
        };

        self.peers.insert(
            peer.clone(),
            PeerRecord {
                peer: peer.clone(),
                nickname: announcement.nickname.clone(),
                connected,
                noise_public_key: Some(announcement.noise_public_key.clone()),
                signing_public_key: Some(announcement.signing_public_key.clone()),
                verified_nickname: true,
                last_seen_ms: now_unix_millis,
                capabilities: announcement.capabilities.unwrap_or_default(),
                bridge_geohash: announcement.bridge_geohash.clone(),
            },
        );

        update
    }

    /// Ages the registry against the clock and the current link presence.
    ///
    /// Two independent rules:
    ///
    /// - A **connected** peer silent for longer than [`PEER_INACTIVITY_TIMEOUT_MS`]
    ///   is demoted — but only if it has no live link. An idle link is not a dead
    ///   peer, and demoting one would tear down a working path over silence alone.
    /// - A **disconnected** peer silent past its retention window is forgotten.
    ///
    /// Both rules read the peer's state as it was at the top of the pass, so a
    /// peer demoted by the first rule is not also removed by the second in the
    /// same call. That is bitchat's behaviour (its second `if` tests the captured
    /// copy) and it is the right one: it gives a freshly demoted peer a full
    /// retention window as a remembered peer instead of evicting it instantly.
    pub fn reconcile_connectivity(
        &mut self,
        now_unix_millis: u64,
        presence: &BTreeMap<PeerId, LinkPresence>,
    ) -> ConnectivityChanges {
        let mut changes = ConnectivityChanges::default();
        let snapshot: Vec<(PeerId, bool, bool, u64, String)> = self
            .peers
            .values()
            .map(|p| {
                (
                    p.peer.clone(),
                    p.connected,
                    p.verified_nickname,
                    p.last_seen_ms,
                    p.nickname.clone(),
                )
            })
            .collect();

        for (peer, was_connected, verified, last_seen, nickname) in snapshot {
            let age = now_unix_millis.saturating_sub(last_seen);
            let retention = reachability_retention_ms(verified);

            if was_connected && age > PEER_INACTIVITY_TIMEOUT_MS {
                let live = presence.get(&peer).copied().unwrap_or_default();
                if !live.any() {
                    if let Some(record) = self.peers.get_mut(&peer) {
                        record.connected = false;
                    }
                    changes.disconnected.push(peer.clone());
                }
            }

            if !was_connected && age > retention {
                self.peers.remove(&peer);
                changes.removed.push(RemovedPeer { peer, nickname });
            }
        }

        changes
    }
}

// ---------------------------------------------------------------------------
// Ingress memory
// ---------------------------------------------------------------------------

/// Where a packet came in. Ported from `BLEIngressLinkRecord`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressRecord {
    pub link: LinkId,
    /// The identity we attributed the packet to on arrival — the last hop, not
    /// necessarily the sender named in the header.
    pub peer: PeerId,
    pub recorded_at_ms: u64,
}


/// Which link each recently seen packet arrived on. Ported from
/// `BLEIngressLinkRegistry`.
///
/// Two jobs, both short-lived:
///
/// - **Echo suppression.** A flood re-sent down the link it came from is pure
///   waste and, on a two-node segment, a loop.
/// - **Last-hop attribution.** The relay bookkeeping needs to know who handed us
///   a packet, which is not who wrote their ID in the header.
///
/// Records expire after [`INGRESS_RECORD_LIFETIME_MS`] — three seconds. That is
/// not a tuning knob to raise: the window only has to cover the moment between
/// receiving a packet and deciding where to forward it, and a long-lived
/// packet→link memory would be another surface a replay could steer.
#[derive(Debug, Default)]
pub struct IngressRegistry {
    by_message: BTreeMap<String, IngressRecord>,
}

impl IngressRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_message.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_message.is_empty()
    }

    pub fn clear(&mut self) {
        self.by_message.clear();
    }

    /// The key a packet is remembered under: sender, millisecond, type and a
    /// payload digest.
    ///
    /// Shared with [`MessageDeduplicator::dedup_key`] rather than reimplemented —
    /// bitchat computes the identical string in both places, and two copies that
    /// could drift would mean the dedup cache and the ingress memory disagreeing
    /// about what "the same packet" is.
    pub fn message_key(packet: &BitchatPacket) -> String {
        MessageDeduplicator::dedup_key(packet)
    }

    /// Remembers where a packet arrived, unless a live record already exists.
    ///
    /// Returns false for a packet we have already accepted within `lifetime_ms` —
    /// which is the caller's cue to drop it as a duplicate arriving by another
    /// path. An expired record is replaced and reported as new.
    pub fn record_if_new(
        &mut self,
        packet: &BitchatPacket,
        link: LinkId,
        peer: PeerId,
        now_unix_millis: u64,
        lifetime_ms: u64,
    ) -> bool {
        let key = Self::message_key(packet);
        if let Some(existing) = self.by_message.get(&key) {
            if now_unix_millis.saturating_sub(existing.recorded_at_ms) <= lifetime_ms {
                return false;
            }
        }
        self.by_message.insert(
            key,
            IngressRecord {
                link,
                peer,
                recorded_at_ms: now_unix_millis,
            },
        );
        true
    }

    pub fn record(&self, packet: &BitchatPacket) -> Option<&IngressRecord> {
        self.by_message.get(&Self::message_key(packet))
    }

    /// The link a packet arrived on, if still remembered.
    pub fn link_for(&self, packet: &BitchatPacket) -> Option<&LinkId> {
        self.record(packet).map(|r| &r.link)
    }

    /// The last hop a packet arrived from, if still remembered.
    pub fn peer_for(&self, packet: &BitchatPacket) -> Option<&PeerId> {
        self.record(packet).map(|r| &r.peer)
    }

    /// Drops records older than `lifetime_ms`.
    pub fn prune(&mut self, now_unix_millis: u64, lifetime_ms: u64) {
        self.by_message
            .retain(|_, r| now_unix_millis.saturating_sub(r.recorded_at_ms) <= lifetime_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECT_TTL: u8 = 7;

    fn link(id: &str) -> LinkId {
        LinkId::new(id)
    }

    fn peer(hex_byte: u8) -> PeerId {
        PeerId::from_bytes(&[hex_byte; 8])
    }

    fn announcement(nickname: &str) -> AnnouncementPacket {
        AnnouncementPacket::new(
            nickname.to_string(),
            vec![0xAB; 32],
            vec![0xCD; 32],
            None,
            None,
            None,
        )
    }

    fn packet(msg_type: MessageType, sender: u8, ttl: u8, payload: &[u8]) -> BitchatPacket {
        BitchatPacket::new(
            msg_type,
            [sender; 8],
            None,
            1_700_000_000_000,
            payload.to_vec(),
            ttl,
        )
    }

    fn announce_packet(sender: u8) -> BitchatPacket {
        packet(MessageType::Announce, sender, DIRECT_TTL, b"announce")
    }

    fn store_with_bound_link(id: &str, role: LinkRole, owner: &PeerId) -> LinkStateStore {
        let mut store = LinkStateStore::new();
        store.link_up(link(id), role);
        assert_eq!(store.bind_if_unbound(&link(id), owner), BindOutcome::Bound);
        store
    }

    // -- LinkStateStore: binding ------------------------------------------

    #[test]
    fn a_link_starts_unbound_and_binds_once() {
        let mut store = LinkStateStore::new();
        store.link_up(link("l1"), LinkRole::Central);
        assert!(store.contains(&link("l1")));
        assert_eq!(store.peer_for(&link("l1")), None);

        assert_eq!(
            store.bind_if_unbound(&link("l1"), &peer(0xAA)),
            BindOutcome::Bound
        );
        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
        assert_eq!(
            store.bind_if_unbound(&link("l1"), &peer(0xAA)),
            BindOutcome::AlreadyBound
        );
    }

    #[test]
    fn an_unverified_announce_can_never_take_a_bound_link() {
        // The whole point of bind_if_unbound: the pre-verification path sees only
        // a sender ID in a header, which anyone can write.
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        assert_eq!(
            store.bind_if_unbound(&link("l1"), &peer(0xBB)),
            BindOutcome::RefusedBoundToOtherPeer(peer(0xAA))
        );
        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
    }

    #[test]
    fn binding_an_unknown_link_is_refused() {
        let mut store = LinkStateStore::new();
        assert_eq!(
            store.bind_if_unbound(&link("ghost"), &peer(0xAA)),
            BindOutcome::UnknownLink
        );
    }

    #[test]
    fn links_to_finds_every_link_one_identity_owns() {
        let mut store = LinkStateStore::new();
        store.link_up(link("l1"), LinkRole::Central);
        store.link_up(link("l2"), LinkRole::Peripheral);
        store.link_up(link("l3"), LinkRole::Central);
        store.bind_if_unbound(&link("l1"), &peer(0xAA));
        store.bind_if_unbound(&link("l2"), &peer(0xAA));
        store.bind_if_unbound(&link("l3"), &peer(0xBB));

        assert_eq!(
            store.links_to(&peer(0xAA)),
            BTreeSet::from([link("l1"), link("l2")])
        );
        assert_eq!(store.links_to(&peer(0xCC)), BTreeSet::new());
    }

    #[test]
    fn presence_is_named_for_our_own_role() {
        let mut store = LinkStateStore::new();
        store.link_up(link("out"), LinkRole::Central);
        store.bind_if_unbound(&link("out"), &peer(0xAA));
        let presence = store.presence(&peer(0xAA));
        assert!(presence.as_central);
        assert!(!presence.as_peripheral);
        assert!(presence.any());

        store.link_up(link("in"), LinkRole::Peripheral);
        store.bind_if_unbound(&link("in"), &peer(0xAA));
        assert_eq!(
            store.presence(&peer(0xAA)),
            LinkPresence {
                as_central: true,
                as_peripheral: true
            }
        );
        assert!(!store.presence(&peer(0xBB)).any());
    }

    #[test]
    fn presence_by_peer_covers_only_bound_links() {
        let mut store = LinkStateStore::new();
        store.link_up(link("l1"), LinkRole::Central);
        store.link_up(link("l2"), LinkRole::Peripheral);
        store.bind_if_unbound(&link("l1"), &peer(0xAA));
        let map = store.presence_by_peer();
        assert_eq!(map.len(), 1);
        assert!(map.get(&peer(0xAA)).is_some_and(|p| p.as_central));
    }

    #[test]
    fn preferred_link_tracks_the_latest_outbound_binding_only() {
        let mut store = LinkStateStore::new();
        store.link_up(link("out1"), LinkRole::Central);
        store.link_up(link("out2"), LinkRole::Central);
        store.link_up(link("in"), LinkRole::Peripheral);

        store.bind_if_unbound(&link("out1"), &peer(0xAA));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("out1")));
        store.bind_if_unbound(&link("out2"), &peer(0xAA));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("out2")));

        // Inbound links are the peer's to consolidate; they never become the
        // preferred outbound target.
        store.bind_if_unbound(&link("in"), &peer(0xBB));
        assert_eq!(store.preferred_links().get(&peer(0xBB)), None);
    }

    #[test]
    fn dropping_the_preferred_link_repairs_onto_a_writable_survivor() {
        let mut store = LinkStateStore::new();
        for id in ["s1", "s2", "preferred"] {
            store.link_up(link(id), LinkRole::Central);
            store.bind_if_unbound(&link(id), &peer(0xAA));
        }
        store.set_writable(&link("s2"), true);
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("preferred")));

        assert_eq!(store.link_down(&link("preferred")), Some(peer(0xAA)));
        // s2 is writable, s1 is not: repairing onto s1 would strand directed
        // sends until its characteristic came back.
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("s2")));
    }

    #[test]
    fn dropping_a_non_preferred_duplicate_leaves_the_preferred_link_alone() {
        let mut store = LinkStateStore::new();
        store.link_up(link("dup"), LinkRole::Central);
        store.bind_if_unbound(&link("dup"), &peer(0xAA));
        store.link_up(link("main"), LinkRole::Central);
        store.bind_if_unbound(&link("main"), &peer(0xAA));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("main")));

        let _ = store.link_down(&link("dup"));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), Some(&link("main")));
    }

    #[test]
    fn dropping_the_last_link_clears_the_preference() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        let _ = store.link_down(&link("l1"));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
        assert!(store.links_to(&peer(0xAA)).is_empty());
    }

    #[test]
    fn link_up_is_idempotent_and_keeps_the_binding() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        store.set_writable(&link("l1"), true);
        store.link_up(link("l1"), LinkRole::Central);
        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xAA)));
        assert!(store.is_writable(&link("l1")));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn role_counts_and_clear() {
        let mut store = LinkStateStore::new();
        store.link_up(link("a"), LinkRole::Central);
        store.link_up(link("b"), LinkRole::Central);
        store.link_up(link("c"), LinkRole::Peripheral);
        store.bind_if_unbound(&link("a"), &peer(0xAA));
        assert_eq!(store.count_role(LinkRole::Central), 2);
        assert_eq!(store.count_role(LinkRole::Peripheral), 1);

        assert_eq!(store.clear(), BTreeSet::from([peer(0xAA)]));
        assert!(store.is_empty());
        assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
    }


    // -- LinkStateStore: the rebind guards --------------------------------

    #[test]
    fn an_unverified_announce_yields_no_rebind_witness_at_all() {
        // There is no way to call the rebind path without one: the guard is
        // structural, not a bool a caller can pass wrong.
        assert!(VerifiedDirectAnnounce::new(peer(0xBB), false, true).is_none());
        assert!(VerifiedDirectAnnounce::new(peer(0xBB), true, false).is_none());
        assert!(VerifiedDirectAnnounce::new(peer(0xBB), false, false).is_none());
        assert!(VerifiedDirectAnnounce::new(peer(0xBB), true, true).is_some());
    }

    #[test]
    fn witness_from_packet_requires_the_origination_ttl() {
        let direct = announce_packet(0xBB);
        let mut relayed = announce_packet(0xBB);
        relayed.ttl = DIRECT_TTL - 1;
        let not_announce = packet(MessageType::Message, 0xBB, DIRECT_TTL, b"hi");

        assert!(VerifiedDirectAnnounce::from_packet(peer(0xBB), &direct, true, DIRECT_TTL).is_some());
        assert!(
            VerifiedDirectAnnounce::from_packet(peer(0xBB), &relayed, true, DIRECT_TTL).is_none()
        );
        assert!(
            VerifiedDirectAnnounce::from_packet(peer(0xBB), &not_announce, true, DIRECT_TTL)
                .is_none()
        );
        assert!(
            VerifiedDirectAnnounce::from_packet(peer(0xBB), &direct, false, DIRECT_TTL).is_none()
        );
    }

    #[test]
    fn a_verified_direct_announce_rebinds_a_rotated_peer_id() {
        // The legitimate case: the peer relaunched, rotated its ID, and the open
        // connection still points at the retired one.
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();

        let outcome = store.rebind_after_verified_direct(&link("l1"), &witness, 1_000);
        let rebind = outcome.rebound().expect("rebind allowed");
        assert_eq!(rebind.previous_peer, peer(0xAA));
        assert_eq!(rebind.new_peer, peer(0xBB));
        assert!(rebind.retired_links.is_empty());
        assert!(rebind.retire_previous_peer);

        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xBB)));
        assert!(store.links_to(&peer(0xAA)).is_empty());
        assert_eq!(store.preferred_links().get(&peer(0xBB)), Some(&link("l1")));
        assert_eq!(store.preferred_links().get(&peer(0xAA)), None);
    }

    #[test]
    fn rebind_is_refused_when_the_identity_already_owns_a_live_link() {
        // The containment that matters most: a replayed announce cannot displace
        // an identity that is genuinely connected elsewhere.
        let mut store = store_with_bound_link("victim", LinkRole::Central, &peer(0xBB));
        store.link_up(link("attacker"), LinkRole::Peripheral);
        store.bind_if_unbound(&link("attacker"), &peer(0xAA));

        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        let outcome = store.rebind_after_verified_direct(&link("attacker"), &witness, 1_000);
        assert_eq!(
            outcome.refusal(),
            Some(&RebindRefusal::IdentityOwnsAnotherLink)
        );

        // Nothing moved, and no cooldown was spent on the refusal.
        assert_eq!(store.peer_for(&link("attacker")), Some(&peer(0xAA)));
        assert_eq!(store.peer_for(&link("victim")), Some(&peer(0xBB)));
        assert_eq!(store.links_to(&peer(0xBB)), BTreeSet::from([link("victim")]));
    }

    #[test]
    fn rebind_is_refused_inside_the_cooldown_and_allowed_after_it() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));

        let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        assert!(
            store
                .rebind_after_verified_direct(&link("l1"), &first, 1_000)
                .rebound()
                .is_some()
        );

        // A third identity replays immediately: refused, with the remaining
        // window reported.
        let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
        assert_eq!(
            store.rebind_after_verified_direct(&link("l1"), &second, 1_500),
            RebindOutcome::Refused(RebindRefusal::CooldownActive {
                remaining_ms: LINK_REBIND_COOLDOWN_MS - 500
            })
        );
        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xBB)));

        // One millisecond before the window closes: still refused.
        assert_eq!(
            store.rebind_after_verified_direct(&link("l1"), &second, 1_000 + LINK_REBIND_COOLDOWN_MS - 1),
            RebindOutcome::Refused(RebindRefusal::CooldownActive { remaining_ms: 1 })
        );

        // Exactly at the window: the entry is forgotten, so the rebind is allowed.
        // (bitchat keeps entries with age strictly < the window.)
        let outcome =
            store.rebind_after_verified_direct(&link("l1"), &second, 1_000 + LINK_REBIND_COOLDOWN_MS);
        assert!(outcome.rebound().is_some());
        assert_eq!(store.peer_for(&link("l1")), Some(&peer(0xCC)));
    }

    #[test]
    fn the_cooldown_is_per_link_not_global() {
        let mut store = LinkStateStore::new();
        for id in ["l1", "l2"] {
            store.link_up(link(id), LinkRole::Central);
        }
        store.bind_if_unbound(&link("l1"), &peer(0xAA));
        store.bind_if_unbound(&link("l2"), &peer(0xBB));

        let to_cc = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
        assert!(
            store
                .rebind_after_verified_direct(&link("l1"), &to_cc, 1_000)
                .rebound()
                .is_some()
        );
        let to_dd = VerifiedDirectAnnounce::new(peer(0xDD), true, true).unwrap();
        assert!(
            store
                .rebind_after_verified_direct(&link("l2"), &to_dd, 1_100)
                .rebound()
                .is_some()
        );
    }

    #[test]
    fn a_clock_that_steps_backwards_keeps_the_cooldown_closed() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        store.rebind_after_verified_direct(&link("l1"), &first, 10_000);

        let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
        assert_eq!(
            store.rebind_after_verified_direct(&link("l1"), &second, 5_000),
            RebindOutcome::Refused(RebindRefusal::CooldownActive {
                remaining_ms: LINK_REBIND_COOLDOWN_MS
            })
        );
    }

    #[test]
    fn rebinding_an_unbound_or_unknown_link_is_not_a_rebind() {
        let mut store = LinkStateStore::new();
        store.link_up(link("l1"), LinkRole::Central);
        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();

        assert_eq!(
            store.rebind_after_verified_direct(&link("l1"), &witness, 1_000),
            RebindOutcome::Refused(RebindRefusal::LinkUnbound)
        );
        assert_eq!(
            store.rebind_after_verified_direct(&link("ghost"), &witness, 1_000),
            RebindOutcome::Refused(RebindRefusal::UnknownLink)
        );
        // A refused rebind must not have spent the link's cooldown.
        store.bind_if_unbound(&link("l1"), &peer(0xAA));
        assert!(
            store
                .rebind_after_verified_direct(&link("l1"), &witness, 1_001)
                .rebound()
                .is_some()
        );
    }

    #[test]
    fn reclaiming_your_own_link_is_a_no_op_that_does_not_spend_the_cooldown() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        let same = VerifiedDirectAnnounce::new(peer(0xAA), true, true).unwrap();
        assert_eq!(
            store.rebind_after_verified_direct(&link("l1"), &same, 1_000),
            RebindOutcome::Refused(RebindRefusal::SamePeer)
        );

        let other = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        assert!(
            store
                .rebind_after_verified_direct(&link("l1"), &other, 1_000)
                .rebound()
                .is_some()
        );
    }

    #[test]
    fn rebind_retires_the_rotated_ids_other_outbound_links() {
        // A relaunch leaves the same device holding several outbound links under
        // the retired ID; they duplicate every packet until they are cancelled.
        let mut store = LinkStateStore::new();
        for id in ["stale1", "stale2", "ingress"] {
            store.link_up(link(id), LinkRole::Central);
            store.bind_if_unbound(&link(id), &peer(0xAA));
        }
        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        let outcome = store.rebind_after_verified_direct(&link("ingress"), &witness, 1_000);
        let rebind = outcome.rebound().expect("rebind allowed");

        assert_eq!(rebind.retired_links, vec![link("stale1"), link("stale2")]);
        assert!(rebind.retire_previous_peer);
        assert!(!store.contains(&link("stale1")));
        assert!(!store.contains(&link("stale2")));
        assert!(store.contains(&link("ingress")));
    }

    #[test]
    fn rebind_leaves_inbound_links_of_the_rotated_id_alone() {
        // We cannot cancel a connection the peer opened to us; it retires its own
        // side by running this same policy. So the old identity survives.
        let mut store = LinkStateStore::new();
        store.link_up(link("ingress"), LinkRole::Central);
        store.link_up(link("theirs"), LinkRole::Peripheral);
        store.bind_if_unbound(&link("ingress"), &peer(0xAA));
        store.bind_if_unbound(&link("theirs"), &peer(0xAA));

        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        let outcome = store.rebind_after_verified_direct(&link("ingress"), &witness, 1_000);
        let rebind = outcome.rebound().expect("rebind allowed");
        assert!(rebind.retired_links.is_empty());
        assert!(!rebind.retire_previous_peer);
        assert_eq!(store.links_to(&peer(0xAA)), BTreeSet::from([link("theirs")]));
    }

    #[test]
    fn rebind_drops_the_links_noise_proof() {
        // The proof belongs to the previous physical binding. Carrying it across
        // would hand a replayer an authentication it never performed.
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        assert!(store.mark_noise_authenticated(&link("l1"), &peer(0xAA)));
        assert!(store.is_noise_authenticated(&link("l1"), &peer(0xAA)));

        let witness = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        assert!(
            store
                .rebind_after_verified_direct(&link("l1"), &witness, 1_000)
                .rebound()
                .is_some()
        );
        assert_eq!(store.noise_authenticated_owner(&link("l1")), None);
        assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xBB)));
        assert!(store.noise_authenticated_links_to(&peer(0xBB)).is_empty());
    }

    #[test]
    fn noise_proof_requires_the_binding_to_agree() {
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        store.mark_noise_authenticated(&link("l1"), &peer(0xAA));
        assert_eq!(
            store.noise_authenticated_links_to(&peer(0xAA)),
            BTreeSet::from([link("l1")])
        );

        let _ = store.link_down(&link("l1"));
        assert!(!store.is_noise_authenticated(&link("l1"), &peer(0xAA)));
        assert!(!store.mark_noise_authenticated(&link("ghost"), &peer(0xAA)));
    }

    #[test]
    fn the_cooldown_survives_a_reconnect_of_the_same_link() {
        // Link IDs are stable per device, so forgetting the cooldown on
        // disconnect would let a replayer reset the guard by forcing a bounce.
        let mut store = store_with_bound_link("l1", LinkRole::Central, &peer(0xAA));
        let first = VerifiedDirectAnnounce::new(peer(0xBB), true, true).unwrap();
        store.rebind_after_verified_direct(&link("l1"), &first, 1_000);

        let _ = store.link_down(&link("l1"));
        store.link_up(link("l1"), LinkRole::Central);
        store.bind_if_unbound(&link("l1"), &peer(0xAA));

        let second = VerifiedDirectAnnounce::new(peer(0xCC), true, true).unwrap();
        assert!(matches!(
            store.rebind_after_verified_direct(&link("l1"), &second, 2_000),
            RebindOutcome::Refused(RebindRefusal::CooldownActive { .. })
        ));
    }

    // -- PeerRegistry -----------------------------------------------------

    fn registry_with(peer_id: PeerId, verified: bool, connected: bool, last_seen: u64)
    -> PeerRegistry {
        let mut registry = PeerRegistry::new();
        registry.upsert(PeerRecord {
            peer: peer_id,
            nickname: "nick".into(),
            connected,
            noise_public_key: None,
            signing_public_key: None,
            verified_nickname: verified,
            last_seen_ms: last_seen,
            capabilities: PeerCapabilities::default(),
            bridge_geohash: None,
        });
        registry
    }

    #[test]
    fn a_verified_announce_records_the_peer_and_reports_what_changed() {
        let mut registry = PeerRegistry::new();
        let first = registry.upsert_verified_announce(&peer(0xAA), &announcement("alice"), false, 10);
        assert!(first.is_new_peer);
        assert!(!first.was_disconnected);
        assert_eq!(first.previous_nickname, None);

        let second =
            registry.upsert_verified_announce(&peer(0xAA), &announcement("alice2"), true, 20);
        assert!(!second.is_new_peer);
        assert!(second.was_disconnected);
        assert_eq!(second.previous_nickname, Some("alice".to_string()));

        let record = registry.info(&peer(0xAA)).unwrap();
        assert_eq!(record.nickname, "alice2");
        assert!(record.verified_nickname);
        assert_eq!(record.last_seen_ms, 20);
        assert_eq!(record.noise_public_key.as_deref(), Some(&[0xAB; 32][..]));
        assert_eq!(registry.connected_count(), 1);
    }


    #[test]
    fn a_connected_peer_is_reachable_regardless_of_age() {
        let registry = registry_with(peer(0xAA), false, true, 0);
        assert!(registry.is_reachable(&peer(0xAA), 10_000_000));
    }

    #[test]
    fn a_remembered_peer_is_unreachable_when_we_have_no_mesh_at_all() {
        // Nothing is connected, so there is no path a memory could be used over.
        let registry = registry_with(peer(0xAA), true, false, 0);
        assert!(!registry.is_reachable(&peer(0xAA), 1_000));
    }

    #[test]
    fn reachability_retention_differs_for_verified_and_unverified_at_the_boundary() {
        let mut registry = registry_with(peer(0xAA), true, false, 0);
        registry.upsert(PeerRecord {
            peer: peer(0xBB),
            nickname: "unverified".into(),
            connected: false,
            noise_public_key: None,
            signing_public_key: None,
            verified_nickname: false,
            last_seen_ms: 0,
            capabilities: PeerCapabilities::default(),
            bridge_geohash: None,
        });
        // Something must be attached for remembered peers to count at all.
        registry.upsert(PeerRecord {
            peer: peer(0xCC),
            nickname: "anchor".into(),
            connected: true,
            noise_public_key: None,
            signing_public_key: None,
            verified_nickname: true,
            last_seen_ms: 0,
            capabilities: PeerCapabilities::default(),
            bridge_geohash: None,
        });

        // Unverified: 45 s exactly is still reachable, 45 s + 1 ms is not.
        assert!(registry.is_reachable(&peer(0xBB), REACHABILITY_RETENTION_UNVERIFIED_MS));
        assert!(!registry.is_reachable(&peer(0xBB), REACHABILITY_RETENTION_UNVERIFIED_MS + 1));
        // Verified is still reachable at that same instant — the whole point of
        // the split — and out to 60 s exactly.
        assert!(registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_UNVERIFIED_MS + 1));
        assert!(registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_VERIFIED_MS));
        assert!(!registry.is_reachable(&peer(0xAA), REACHABILITY_RETENTION_VERIFIED_MS + 1));
    }

    #[test]
    fn reachability_lookups_reduce_a_full_noise_key_to_its_short_form() {
        let key = vec![0xAB; 32];
        let short = PeerId::from_noise_key(&key);
        let registry = registry_with(short.clone(), true, true, 0);
        let long = PeerId::from_str(&"ab".repeat(32));
        assert_eq!(long.to_short(), short);
        assert!(registry.is_connected(&long));
        assert!(registry.is_reachable(&long, 0));
    }

    #[test]
    fn an_idle_connected_peer_with_a_live_link_is_left_connected() {
        let mut registry = registry_with(peer(0xAA), true, true, 0);
        let mut presence = BTreeMap::new();
        presence.insert(
            peer(0xAA),
            LinkPresence {
                as_central: true,
                as_peripheral: false,
            },
        );
        let changes =
            registry.reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS + 1_000, &presence);
        assert!(changes.is_empty());
        assert!(registry.info(&peer(0xAA)).unwrap().connected);
    }

    #[test]
    fn an_idle_connected_peer_with_no_link_is_demoted_at_the_boundary() {
        let presence = BTreeMap::new();
        let mut registry = registry_with(peer(0xAA), true, true, 0);
        // Exactly the timeout is not yet over it (Swift uses a strict >).
        assert!(
            registry
                .reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS, &presence)
                .is_empty()
        );
        let changes = registry.reconcile_connectivity(PEER_INACTIVITY_TIMEOUT_MS + 1, &presence);
        assert_eq!(changes.disconnected, vec![peer(0xAA)]);
        assert!(changes.removed.is_empty());
        assert!(!registry.info(&peer(0xAA)).unwrap().connected);
    }

    #[test]
    fn a_peer_demoted_this_pass_is_not_also_removed_in_it() {
        // It gets a full retention window as a remembered peer instead.
        let presence = BTreeMap::new();
        let mut registry = registry_with(peer(0xAA), false, true, 0);
        let changes = registry.reconcile_connectivity(1_000_000, &presence);
        assert_eq!(changes.disconnected, vec![peer(0xAA)]);
        assert!(changes.removed.is_empty());
        assert!(registry.info(&peer(0xAA)).is_some());

        let changes = registry.reconcile_connectivity(1_000_000, &presence);
        assert_eq!(
            changes.removed,
            vec![RemovedPeer {
                peer: peer(0xAA),
                nickname: "nick".into()
            }]
        );
        assert!(registry.info(&peer(0xAA)).is_none());
    }

    #[test]
    fn disconnected_peers_are_evicted_on_their_own_retention_window() {
        let presence = BTreeMap::new();
        let mut verified = registry_with(peer(0xAA), true, false, 0);
        let mut unverified = registry_with(peer(0xAA), false, false, 0);

        assert!(
            verified
                .reconcile_connectivity(REACHABILITY_RETENTION_VERIFIED_MS, &presence)
                .is_empty()
        );
        assert!(!verified.is_empty());
        assert_eq!(
            verified
                .reconcile_connectivity(REACHABILITY_RETENTION_VERIFIED_MS + 1, &presence)
                .removed
                .len(),
            1
        );

        // The unverified peer is gone while the verified one is still held.
        assert_eq!(
            unverified
                .reconcile_connectivity(REACHABILITY_RETENTION_UNVERIFIED_MS + 1, &presence)
                .removed
                .len(),
            1
        );
    }

    #[test]
    fn capabilities_and_bridge_geohash_come_from_the_announce() {
        let mut registry = PeerRegistry::new();
        let mut announcement = announcement("bridge");
        announcement.capabilities = Some(PeerCapabilities::from_bits(
            PeerCapabilities::BRIDGE | PeerCapabilities::PREKEYS,
        ));
        announcement.bridge_geohash = Some("u4pruy".into());
        registry.upsert_verified_announce(&peer(0xAA), &announcement, true, 0);

        let record = registry.info(&peer(0xAA)).unwrap();
        assert!(record.capabilities.contains(PeerCapabilities::BRIDGE));
        assert_eq!(record.bridge_geohash.as_deref(), Some("u4pruy"));
    }

    /// An announce whose peer ID really is derived from its Noise key, which
    /// is what a persisted pin has to be.
    fn derived(noise_byte: u8, signing_byte: u8, nickname: &str) -> (PeerId, AnnouncementPacket) {
        let noise = vec![noise_byte; 32];
        let announcement = AnnouncementPacket::new(
            nickname.to_string(),
            noise.clone(),
            vec![signing_byte; 32],
            None,
            None,
            None,
        );
        (PeerId::from_noise_key(&noise), announcement)
    }

    #[test]
    fn the_first_announce_pins_and_a_later_one_does_not_move_the_pin() {
        let mut registry = PeerRegistry::new();
        let (alice, first) = derived(1, 2, "alice");

        assert!(registry.upsert_verified_announce(&alice, &first, true, 10).pinned_now);

        let (_, renamed) = derived(1, 9, "alice-renamed");
        let update = registry.upsert_verified_announce(&alice, &renamed, true, 20);

        assert!(!update.pinned_now);
        let pin = registry.pinned(&alice).expect("pinned");
        assert_eq!(pin.signing_public_key, vec![2; 32]);
        assert_eq!(pin.nickname, "alice");
        assert_eq!(pin.pinned_at_ms, 10);
    }

    #[test]
    fn forgetting_a_peer_does_not_forget_its_pin() {
        let mut registry = PeerRegistry::new();
        let (alice, announcement) = derived(1, 2, "alice");
        registry.upsert_verified_announce(&alice, &announcement, false, 0);

        // Aged out as a disconnected peer, then removed outright, then the
        // whole registry cleared: every way a record can go.
        let removed = registry.reconcile_connectivity(
            REACHABILITY_RETENTION_VERIFIED_MS + 1,
            &BTreeMap::new(),
        );
        assert_eq!(removed.removed.len(), 1);
        registry.remove(&alice);
        registry.remove_all();

        assert!(registry.info(&alice).is_none());
        assert_eq!(
            registry.pinned(&alice).map(|pin| pin.signing_public_key.clone()),
            Some(vec![2; 32])
        );
    }

    #[test]
    fn pins_survive_a_round_trip_through_storage() {
        let mut registry = PeerRegistry::new();
        let (alice, a) = derived(1, 2, "alice");
        let (bob, b) = derived(3, 4, "bob");
        registry.upsert_verified_announce(&alice, &a, true, 10);
        registry.upsert_verified_announce(&bob, &b, true, 20);

        let mut restarted = PeerRegistry::new();
        restarted
            .restore_pins(&registry.pins_to_bytes())
            .expect("restore");

        assert_eq!(restarted.pinned(&alice), registry.pinned(&alice));
        assert_eq!(restarted.pinned(&bob), registry.pinned(&bob));
        assert!(restarted.is_empty(), "a pin is not presence");
    }

    #[test]
    fn a_restored_pin_does_not_replace_one_already_held() {
        let (alice, stored) = derived(1, 2, "alice");
        let mut old = PeerRegistry::new();
        old.upsert_verified_announce(&alice, &stored, true, 10);

        let mut registry = PeerRegistry::new();
        let (_, live) = derived(1, 7, "alice");
        registry.upsert_verified_announce(&alice, &live, true, 99);
        registry.restore_pins(&old.pins_to_bytes()).expect("restore");

        assert_eq!(
            registry.pinned(&alice).map(|pin| pin.pinned_at_ms),
            Some(99)
        );
    }

    #[test]
    fn a_damaged_pin_blob_is_refused_whole() {
        let mut registry = PeerRegistry::new();
        let (alice, a) = derived(1, 2, "alice");
        let (bob, b) = derived(3, 4, "bob");
        registry.upsert_verified_announce(&alice, &a, true, 10);
        registry.upsert_verified_announce(&bob, &b, true, 20);
        let good = registry.pins_to_bytes();

        let mut wrong_version = good.clone();
        wrong_version[0] = PIN_BLOB_VERSION + 1;

        // Same length, but the first pin's Noise key no longer derives to the
        // ID it is filed under.
        let mut swapped_key = good.clone();
        let key_start = 1 + 1 + alice.as_str().len().max(bob.as_str().len()) + 1;
        swapped_key[key_start] ^= 0xFF;

        for blob in [&good[..good.len() - 1], &[][..], &wrong_version, &swapped_key] {
            let mut target = PeerRegistry::new();
            assert_eq!(target.restore_pins(blob), Err(MalformedPins));
            assert!(target.pinned(&alice).is_none() && target.pinned(&bob).is_none());
        }
    }

    #[test]
    fn a_full_pin_set_refuses_newcomers_and_keeps_what_it_has() {
        let mut registry = PeerRegistry::new();
        let (alice, announcement) = derived(1, 2, "alice");
        registry.upsert_verified_announce(&alice, &announcement, true, 0);
        for n in 0..MAX_PINNED_IDENTITIES as u64 {
            let noise = n.to_be_bytes().repeat(4);
            let flood = AnnouncementPacket::new(
                "flood".to_string(),
                noise.clone(),
                vec![0; 32],
                None,
                None,
                None,
            );
            registry.upsert_verified_announce(&PeerId::from_noise_key(&noise), &flood, true, 1);
        }

        let (late, announcement) = derived(0xEE, 0xEF, "late");
        let update = registry.upsert_verified_announce(&late, &announcement, true, 2);

        assert!(!update.pinned_now);
        assert!(registry.pinned(&late).is_none());
        assert!(registry.info(&late).is_some(), "still a peer for this session");
        assert!(registry.pinned(&alice).is_some());
    }

    #[test]
    fn nickname_can_be_restricted_to_connected_peers() {
        let registry = registry_with(peer(0xAA), true, false, 0);
        assert_eq!(registry.nickname(&peer(0xAA), false), Some("nick"));
        assert_eq!(registry.nickname(&peer(0xAA), true), None);
        let connected = registry_with(peer(0xAA), true, true, 0);
        assert_eq!(connected.nickname(&peer(0xAA), true), Some("nick"));
        assert_eq!(registry.nickname(&peer(0xBB), false), None);
    }


    #[test]
    fn update_last_seen_and_remove() {
        let mut registry = registry_with(peer(0xAA), true, true, 0);
        registry.update_last_seen(&peer(0xAA), 5_000);
        assert_eq!(registry.info(&peer(0xAA)).unwrap().last_seen_ms, 5_000);
        registry.update_last_seen(&peer(0xBB), 5_000); // unknown: no-op
        assert!(registry.remove(&peer(0xAA)).is_some());
        assert!(registry.remove(&peer(0xAA)).is_none());
        assert!(registry.is_empty());
    }

    // -- IngressRegistry --------------------------------------------------

    #[test]
    fn an_ingress_record_remembers_the_link_and_last_hop() {
        let mut ingress = IngressRegistry::new();
        let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
        assert!(ingress.record_if_new(
            &pkt,
            link("l1"),
            peer(0xBB),
            1_000,
            INGRESS_RECORD_LIFETIME_MS
        ));
        assert_eq!(ingress.link_for(&pkt), Some(&link("l1")));
        // The last hop is not the claimed sender: 0xAA wrote the packet, 0xBB
        // handed it to us.
        assert_eq!(ingress.peer_for(&pkt), Some(&peer(0xBB)));
        assert_eq!(ingress.len(), 1);
    }

    #[test]
    fn a_second_copy_inside_the_lifetime_is_reported_as_not_new() {
        let mut ingress = IngressRegistry::new();
        let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
        assert!(ingress.record_if_new(
            &pkt,
            link("l1"),
            peer(0xAA),
            1_000,
            INGRESS_RECORD_LIFETIME_MS
        ));
        // Same packet arriving by another path within the window: suppressed, and
        // the original link record is kept.
        assert!(!ingress.record_if_new(
            &pkt,
            link("l2"),
            peer(0xBB),
            1_000 + INGRESS_RECORD_LIFETIME_MS,
            INGRESS_RECORD_LIFETIME_MS
        ));
        assert_eq!(ingress.link_for(&pkt), Some(&link("l1")));
    }

    #[test]
    fn a_record_expires_after_its_lifetime_and_is_replaced() {
        let mut ingress = IngressRegistry::new();
        let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
        ingress.record_if_new(&pkt, link("l1"), peer(0xAA), 1_000, INGRESS_RECORD_LIFETIME_MS);
        assert!(ingress.record_if_new(
            &pkt,
            link("l2"),
            peer(0xBB),
            1_000 + INGRESS_RECORD_LIFETIME_MS + 1,
            INGRESS_RECORD_LIFETIME_MS
        ));
        assert_eq!(ingress.link_for(&pkt), Some(&link("l2")));
    }

    #[test]
    fn prune_drops_records_past_the_lifetime_only() {
        let mut ingress = IngressRegistry::new();
        let old = packet(MessageType::Message, 0xAA, 5, b"old");
        let fresh = packet(MessageType::Message, 0xAA, 5, b"fresh");
        ingress.record_if_new(&old, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS);
        ingress.record_if_new(&fresh, link("l1"), peer(0xAA), 2_000, INGRESS_RECORD_LIFETIME_MS);

        ingress.prune(INGRESS_RECORD_LIFETIME_MS, INGRESS_RECORD_LIFETIME_MS);
        assert_eq!(ingress.len(), 2); // the older one is exactly at the boundary

        ingress.prune(INGRESS_RECORD_LIFETIME_MS + 1, INGRESS_RECORD_LIFETIME_MS);
        assert_eq!(ingress.len(), 1);
        assert!(ingress.record(&old).is_none());
        assert!(ingress.record(&fresh).is_some());

        ingress.prune(10_000, INGRESS_RECORD_LIFETIME_MS);
        assert!(ingress.is_empty());
    }

    #[test]
    fn distinct_packets_get_distinct_records() {
        let mut ingress = IngressRegistry::new();
        let a = packet(MessageType::Message, 0xAA, 5, b"a");
        let b = packet(MessageType::Message, 0xAA, 5, b"b");
        assert!(ingress.record_if_new(&a, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS));
        assert!(ingress.record_if_new(&b, link("l2"), peer(0xBB), 0, INGRESS_RECORD_LIFETIME_MS));
        assert_eq!(ingress.link_for(&a), Some(&link("l1")));
        assert_eq!(ingress.link_for(&b), Some(&link("l2")));

        // The relay decrements the TTL; the key must not follow it, or a relayed
        // copy would look like a different packet.
        let mut relayed = a.clone();
        relayed.ttl = 4;
        assert_eq!(ingress.link_for(&relayed), Some(&link("l1")));
    }


    #[test]
    fn clear_empties_the_ingress_memory() {
        let mut ingress = IngressRegistry::new();
        let pkt = packet(MessageType::Message, 0xAA, 5, b"hello");
        ingress.record_if_new(&pkt, link("l1"), peer(0xAA), 0, INGRESS_RECORD_LIFETIME_MS);
        ingress.clear();
        assert!(ingress.is_empty());
        assert!(ingress.record(&pkt).is_none());
    }


    #[test]
    fn refusal_and_rejection_render_for_logs() {
        assert!(
            RebindRefusal::IdentityOwnsAnotherLink
                .to_string()
                .contains("already owns")
        );
        assert!(
            RebindRefusal::CooldownActive { remaining_ms: 5 }
                .to_string()
                .contains('5')
        );
    }

    #[test]
    fn constants_match_transport_config() {
        assert_eq!(REACHABILITY_RETENTION_VERIFIED_MS, 60_000);
        assert_eq!(REACHABILITY_RETENTION_UNVERIFIED_MS, 45_000);
        assert_eq!(PEER_INACTIVITY_TIMEOUT_MS, 8_000);
        assert_eq!(INGRESS_RECORD_LIFETIME_MS, 3_000);
        assert_eq!(LINK_REBIND_COOLDOWN_MS, 60_000);
        assert_eq!(reachability_retention_ms(true), 60_000);
        assert_eq!(reachability_retention_ms(false), 45_000);
    }
}
