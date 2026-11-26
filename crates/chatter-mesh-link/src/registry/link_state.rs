use std::collections::{BTreeMap, BTreeSet};

use chatter_mesh::ids::PeerId;

use crate::link::{LinkId, LinkRole};

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
    /// A direct announce hints who to try for public sync and Noise opening.
    discovery_peer: Option<PeerId>,
    /// Whether the link can currently carry a write: the characteristic has been
    /// discovered (central) or a subscription is active (peripheral). A link can
    /// be up and not writable, and offering it to the planner in that state
    /// silently drops whatever gets routed to it.
    pub writable: bool,
    /// The identity that completed a Noise handshake *on this physical link*.
    ///
    /// Peer-level Noise sessions outlive the links they were made on, so a
    /// session alone does not prove the device currently on this link is that
    /// peer. The proof expires with the physical link.
    pub noise_authenticated_owner: Option<PeerId>,
}

impl LinkState {
    fn new(role: LinkRole) -> Self {
        Self {
            role,
            peer: None,
            discovery_peer: None,
            writable: false,
            noise_authenticated_owner: None,
        }
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
    /// The link already belongs to a different peer.
    RefusedBoundToOtherPeer(PeerId),
    /// No such live link.
    UnknownLink,
}

/// Link↔peer bindings in both directions.
///
/// Only **live** links are held: one is added when the radio reports it
/// connected and removed when it reports it gone. That makes "does this identity
/// already own a live link?" a plain lookup.
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

    /// A link-local discovery hint. It never sets preferred routing or presence.
    pub fn note_discovery_peer(&mut self, link: &LinkId, peer: &PeerId) -> bool {
        let Some(state) = self.links.get_mut(link) else {
            return false;
        };
        if state.peer.is_some() || state.discovery_peer.as_ref() == Some(peer) {
            return false;
        }
        state.discovery_peer = Some(peer.clone());
        true
    }

    pub fn discovery_peer_for(&self, link: &LinkId) -> Option<&PeerId> {
        self.links.get(link)?.discovery_peer.as_ref()
    }

    pub fn discovery_links_to(&self, peer: &PeerId) -> impl Iterator<Item = &LinkId> {
        self.links.iter().filter_map(move |(link, state)| {
            (state.writable && state.discovery_peer.as_ref() == Some(peer)).then_some(link)
        })
    }

    /// Every live link bound to an identity.
    ///
    /// Scans rather than consulting the reverse map, matching Swift: one peer can
    /// legitimately hold several links at once (a device reappears under a fresh
    /// handle while its previous connection lives on).
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
    /// Production callers promote a discovery hint after Noise proof. This
    /// operation keeps any existing binding in place.
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
                    state.discovery_peer = None;
                }
                self.set_preferred_if_central(link, peer);
                BindOutcome::Bound
            }
        }
    }

    /// Records that `peer` completed a Noise handshake on this link.
    ///
    /// The existing binding or discovery hint must agree with the proved peer.
    /// A peer-level session can outlive the physical link that established it.
    pub fn mark_noise_authenticated(&mut self, link: &LinkId, peer: &PeerId) -> bool {
        if self.discovery_peer_for(link) == Some(peer) {
            self.bind_if_unbound(link, peer);
        }
        let Some(state) = self.links.get_mut(link) else {
            return false;
        };
        if state.peer.as_ref() != Some(peer) {
            return false;
        }
        state.noise_authenticated_owner = Some(peer.clone());
        true
    }

    pub fn clear_noise_authentication(&mut self, link: &LinkId) {
        if let Some(state) = self.links.get_mut(link) {
            state.noise_authenticated_owner = None;
        }
    }

    /// Whether this link both *belongs* to `peer` and carries `peer`'s Noise
    /// proof. Both halves are required for authenticated directed traffic.
    pub fn is_noise_authenticated(&self, link: &LinkId, peer: &PeerId) -> bool {
        self.links.get(link).is_some_and(|s| {
            s.peer.as_ref() == Some(peer) && s.noise_authenticated_owner.as_ref() == Some(peer)
        })
    }

    /// Drops every link, e.g. when the adapter powers off. Returns the identities
    /// that lost their last link.
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
