use std::collections::{BTreeMap, BTreeSet};

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BitchatPacket, MessageType};

use crate::link::{LinkId, LinkRole};

/// `TransportConfig.bleLinkRebindCooldownSeconds` (60 s).
///
/// At most one identity rotation per link per window. Without it, two identities
/// replaying each other's announces could flip a single link back and forth
/// indefinitely, and every flip is an opportunity to receive traffic addressed
/// to whoever currently owns it.
pub const LINK_REBIND_COOLDOWN_MS: u64 = 60_000;

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
            Self::IdentityOwnsAnotherLink => f.write_str("identity already owns another live link"),
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
    pub(super) fn rebound(&self) -> Option<&Rebind> {
        match self {
            Self::Rebound(r) => Some(r),
            Self::Refused(_) => None,
        }
    }

    pub(super) fn refusal(&self) -> Option<&RebindRefusal> {
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
        self.links
            .entry(link)
            .or_insert_with(|| LinkState::new(role));
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

    /// Whether this link both *belongs* to `peer` and carries `peer`'s Noise
    /// proof. Both halves are required: a rebind clears the proof, and an
    /// unbound-but-proved link is not somewhere to send directed traffic.
    pub fn is_noise_authenticated(&self, link: &LinkId, peer: &PeerId) -> bool {
        self.links.get(link).is_some_and(|s| {
            s.peer.as_ref() == Some(peer) && s.noise_authenticated_owner.as_ref() == Some(peer)
        })
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
