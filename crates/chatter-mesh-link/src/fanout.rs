//! Which links a packet is written to.
//!
//! Ported from `Services/BLE/BLEFanoutSelector.swift`.
//!
//! A flooded mesh only works because every node re-sends what it receives. It only
//! *keeps* working because no node re-sends to everything. This module is that
//! second half: a broadcast is re-sent to roughly `log2(degree) + 1` of the links
//! we hold, chosen by hashing the message ID against each link's identifier.
//!
//! # Why deterministic and not random
//!
//! The subset is a pure function of `(message_id, link_ids)`. That is the whole
//! design, not an implementation convenience:
//!
//! - A packet that arrives twice — same message, different path — selects the same
//!   links both times, so the second copy adds no new airtime.
//! - Coverage is reproducible. A random subset would make "did this reach the far
//!   side of the room?" a probabilistic question that no test can pin down.
//!
//! Note what determinism does *not* mean here: two different nodes never compute
//! the same subset, because [`LinkId`]s are backend-local opaque strings (a D-Bus
//! object path here, a `CBPeripheral` UUID on iOS). Determinism is per-node and
//! per-message.
//!
//! # The role naming trap
//!
//! bitchat's Swift talks in CoreBluetooth's vocabulary, where `peripheralIDs` are
//! the peripherals *we as central* connected out to, and `centralIDs` are the
//! centrals subscribed to *our* peripheral. Our [`LinkRole`] names the side **we**
//! are on, so the mapping is inverted:
//!
//! | Swift | Ours |
//! |---|---|
//! | `peripheralIDs` (we write to them) | [`LinkRole::Central`] |
//! | `centralIDs` (we notify them) | [`LinkRole::Peripheral`] |
//!
//! Every comment below that mentions a Swift identifier uses the Swift sense; the
//! code uses ours. Getting this backwards inverts the collapse preference and the
//! per-role subset split, both silently.

use std::collections::{BTreeSet, btree_set};

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::MessageType;
use sha2::{Digest, Sha256};

use crate::link::{LinkId, LinkRole};

/// One physical link the fanout is allowed to consider.
///
/// Deliberately a plain value rather than a handle into the link registry: the
/// selection is pure, so everything it needs to decide has to be in front of it.
/// The caller snapshots its live links into these and reads the answer back by
/// [`LinkId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkCandidate {
    pub id: LinkId,
    /// Which side of this link we are. See the module note on the naming trap.
    pub role: LinkRole,
    /// The peer this link is bound to, or `None` while it is still pre-announce.
    ///
    /// A link is bound only once its peer proved an identity with a signed
    /// announce. Unbound links are never collapsed away — they are exactly the
    /// links still waiting for the announce that would bind them.
    pub peer: Option<PeerId>,
    /// Largest single write this link accepts. Feeds fragment sizing in
    /// [`crate::planner`]; the selection itself ignores it.
    pub max_write: usize,
    /// This is the peer's *preferred* link — `preferredPeripheralPerPeer` in Swift,
    /// meaning the most recently bound one.
    ///
    /// Only meaningful with [`LinkRole::Central`] and a bound `peer`. When a peer
    /// somehow holds several links, collapsing onto an arbitrary one could route
    /// its single copy down a stale connection; this names the live one.
    pub preferred_for_peer: bool,
}

impl LinkCandidate {
    pub fn new(id: LinkId, role: LinkRole, max_write: usize) -> Self {
        Self {
            id,
            role,
            peer: None,
            max_write,
            preferred_for_peer: false,
        }
    }

    /// Binds this candidate to a peer.
    #[must_use]
    pub fn bound_to(mut self, peer: PeerId) -> Self {
        self.peer = Some(peer);
        self
    }

    /// Marks this candidate as its peer's preferred link.
    #[must_use]
    pub fn preferred(mut self) -> Self {
        self.preferred_for_peer = true;
        self
    }
}

/// The links one outbound packet goes on.
///
/// One flat set rather than Swift's `(peripheralIDs, centralIDs)` pair: the role
/// split matters while choosing, not after. Callers iterate their own candidates
/// and test membership, which is what `BLEService.sendOnAllLinks` does too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FanoutSelection {
    links: BTreeSet<LinkId>,
}

impl FanoutSelection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &LinkId) -> bool {
        self.links.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn iter(&self) -> btree_set::Iter<'_, LinkId> {
        self.links.iter()
    }
}

impl FromIterator<LinkId> for FanoutSelection {
    fn from_iter<I: IntoIterator<Item = LinkId>>(iter: I) -> Self {
        Self {
            links: iter.into_iter().collect(),
        }
    }
}

impl<'a> IntoIterator for &'a FanoutSelection {
    type Item = &'a LinkId;
    type IntoIter = btree_set::Iter<'a, LinkId>;

    fn into_iter(self) -> Self::IntoIter {
        self.links.iter()
    }
}

/// Everything [`select_links`] needs.
///
/// Built with [`FanoutRequest::new`] plus the `with_*` setters; the defaults are
/// the ones a plain broadcast wants.
#[derive(Clone, Debug)]
pub struct FanoutRequest<'a> {
    /// Every link currently live and writable, in a stable order.
    pub links: &'a [LinkCandidate],
    /// The link this packet arrived on, if we are relaying it.
    ///
    /// Split horizon: never send a packet back where it came from. Excluded before
    /// anything else, including the directed path.
    pub ingress_link: Option<&'a LinkId>,
    /// Additional links to leave out — e.g. every other link to the ingress peer,
    /// or the unauthenticated links to a Noise recipient.
    pub excluded_links: &'a [LinkId],
    /// Whether to keep only one link per bound peer.
    ///
    /// True for everything except a *direct announce*, which is the packet that
    /// binds a link to its peer in the first place. Collapsing those starves a
    /// duplicate link of the announce it needs to become bound; it then looks
    /// pre-announce forever and every later broadcast sprays down all of them.
    pub collapse_duplicate_peer_links: bool,
    /// The peer this packet is addressed to, if any. Directed packets are never
    /// subsetted — they go to that peer's link and nowhere else.
    pub directed_peer: Option<&'a PeerId>,
    /// When set, a directed packet with no usable link to its peer selects nothing
    /// rather than falling back to a flood.
    pub require_direct_peer_link: bool,
    /// Raw packet type tag. Announce, fragment, and request-sync bypass subsetting.
    pub packet_type: u8,
    /// The dedup key of the packet, used as the hash seed. See
    /// [`crate::planner::message_id`].
    pub message_id: &'a str,
}

impl<'a> FanoutRequest<'a> {
    pub fn new(links: &'a [LinkCandidate], packet_type: u8, message_id: &'a str) -> Self {
        Self {
            links,
            ingress_link: None,
            excluded_links: &[],
            collapse_duplicate_peer_links: true,
            directed_peer: None,
            require_direct_peer_link: false,
            packet_type,
            message_id,
        }
    }

    #[must_use]
    pub fn with_ingress_link(mut self, link: Option<&'a LinkId>) -> Self {
        self.ingress_link = link;
        self
    }

    #[must_use]
    pub fn with_excluded_links(mut self, links: &'a [LinkId]) -> Self {
        self.excluded_links = links;
        self
    }

    #[must_use]
    pub fn with_collapse_duplicate_peer_links(mut self, collapse: bool) -> Self {
        self.collapse_duplicate_peer_links = collapse;
        self
    }

    #[must_use]
    pub fn with_directed_peer(mut self, peer: Option<&'a PeerId>) -> Self {
        self.directed_peer = peer;
        self
    }

    #[must_use]
    pub fn with_require_direct_peer_link(mut self, require: bool) -> Self {
        self.require_direct_peer_link = require;
        self
    }
}

/// Chooses the links for one outbound packet.
///
/// The order of the decisions is load-bearing and matches
/// `BLEFanoutSelector.selectLinks`:
///
/// 1. Drop the ingress link and anything explicitly excluded.
/// 2. If the packet is directed and we hold an allowed link to that peer, use it
///    alone.
/// 3. If it is directed and `require_direct_peer_link` is set, send nothing.
/// 4. If it is directed and a link to that peer exists but was *excluded*, send
///    nothing — falling through to a flood here would route around the very
///    exclusion the caller asked for.
/// 5. Otherwise collapse duplicate per-peer links, then subset unless this type is
///    exempt.
pub fn select_links(req: &FanoutRequest<'_>) -> FanoutSelection {
    let raw_allowed: Vec<&LinkCandidate> = req
        .links
        .iter()
        .filter(|link| !is_blocked(link, req))
        .collect();

    if let Some(peer) = req.directed_peer {
        let bound: Vec<&LinkCandidate> = raw_allowed
            .iter()
            .copied()
            .filter(|link| link.peer.as_ref() == Some(peer))
            .collect();
        let direct = collapse_duplicate_links_per_peer(&bound);
        if !direct.is_empty() {
            return selection(&direct);
        }

        if req.require_direct_peer_link {
            return FanoutSelection::new();
        }

        // A link to this peer exists, it was just filtered out above. Flooding
        // now would deliver the packet to that peer over some other path, which
        // is precisely what the exclusion (or split horizon) forbade.
        if req
            .links
            .iter()
            .any(|link| link.peer.as_ref() == Some(peer))
        {
            return FanoutSelection::new();
        }
    }

    let allowed = if req.collapse_duplicate_peer_links {
        collapse_duplicate_links_per_peer(&raw_allowed)
    } else {
        raw_allowed
    };

    if !should_subset(req.packet_type, req.directed_peer) {
        return selection(&allowed);
    }

    // The two roles are subsetted independently, exactly as Swift subsets its two
    // arrays. A node holding 4 of each therefore sends 3 + 3, not the 4 that a
    // single pool of 8 would give: the write path and the notify path have
    // separate flow control, and starving one to balance the other is not a
    // trade the mesh wants.
    let mut selected = BTreeSet::new();
    for role in [LinkRole::Central, LinkRole::Peripheral] {
        let ids: Vec<LinkId> = allowed
            .iter()
            .filter(|link| link.role == role)
            .map(|link| link.id.clone())
            .collect();
        selected.extend(deterministic_subset(
            &ids,
            subset_size(ids.len()),
            req.message_id,
        ));
    }

    FanoutSelection { links: selected }
}

/// Whether this packet is eligible for subsetting at all.
///
/// Three types always go to *every* allowed link, and each exemption is load-
/// bearing:
///
/// - **Announce** — subsetting discovery means some neighbours never learn we
///   exist, and an unbound link never becomes bound. Peer discovery degrades
///   silently, which is the worst way for it to degrade.
/// - **Fragment** — a message is useless without all of its pieces. Sending
///   different fragments down different subsets guarantees nobody reassembles it.
/// - **RequestSync** — link-local by construction (TTL 0, never relayed), so there
///   is no flood to thin out.
///
/// Directed packets are excluded here too, but they never reach this point in
/// practice: [`select_links`] returns the bound link before asking.
pub fn should_subset(packet_type: u8, directed_peer: Option<&PeerId>) -> bool {
    directed_peer.is_none()
        && packet_type != MessageType::Fragment.as_u8()
        && packet_type != MessageType::Announce.as_u8()
        && packet_type != MessageType::RequestSync.as_u8()
}

/// How many of `count` links a broadcast goes to.
///
/// `floor(log2(count - 1)) + 2`, clamped to `count`. Small degrees are exempt:
/// 1 or 2 links get all of them, because thinning a two-link node just partitions
/// it.
///
/// | degree | 1 | 2 | 3 | 4 | 5 | 8 | 9 | 16 | 17 | 32 |
/// |---|---|---|---|---|---|---|---|---|---|---|
/// | picked | 1 | 2 | 3 | 3 | 4 | 4 | 5 | 5 | 6 | 6 |
pub fn subset_size(count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    if count <= 2 {
        return count;
    }
    // `count - 1` is at least 2 here, so this is at least 2 and `+ 1` can never
    // land on 0 — the `max(1, …)` in Swift is unreachable and omitted.
    let bits = (usize::BITS - (count - 1).leading_zeros()) as usize;
    count.min(bits + 1)
}

/// Picks `k` of `ids` by hashing each against `seed`.
///
/// The score is `SHA-256(seed ‖ "::" ‖ id)` compared as raw bytes, ascending, with
/// the ID string as tiebreak. That exact preimage is the port-sensitive part:
/// changing the separator, the order, or the digest changes which links a node
/// picks, and the only symptom is a mesh that covers slightly worse than it
/// should.
///
/// `ids.len() <= k` returns everything, as does `k == 0` — matching Swift's guard,
/// where the second case is unreachable because [`subset_size`] only returns 0 for
/// an empty input.
///
/// (Swift breaks ties with `String <`, which is Unicode-collation ordering; ours is
/// byte ordering. Link IDs are ASCII on every backend, and a tie needs two
/// identical SHA-256 digests regardless.)
pub fn deterministic_subset(ids: &[LinkId], k: usize, seed: &str) -> BTreeSet<LinkId> {
    if k == 0 || ids.len() <= k {
        return ids.iter().cloned().collect();
    }

    let mut scored: Vec<([u8; 32], &LinkId)> = ids
        .iter()
        .map(|id| {
            let mut hasher = Sha256::new();
            hasher.update(seed.as_bytes());
            hasher.update(b"::");
            hasher.update(id.as_str().as_bytes());
            (hasher.finalize().into(), id)
        })
        .collect();

    scored.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0).then_with(|| lhs.1.cmp(rhs.1)));
    scored
        .into_iter()
        .take(k)
        .map(|(_, id)| id.clone())
        .collect()
}

fn is_blocked(link: &LinkCandidate, req: &FanoutRequest<'_>) -> bool {
    if req.ingress_link == Some(&link.id) {
        return true;
    }
    req.excluded_links.contains(&link.id)
}

fn selection(links: &[&LinkCandidate]) -> FanoutSelection {
    links.iter().map(|link| link.id.clone()).collect()
}

/// Keeps one link per bound peer.
///
/// A dual-role pair holds two live links at once — we-as-central writing to their
/// peripheral, and they-as-central subscribed to ours. Sending the same bytes down
/// both doubles airtime for nothing: the receiver's deduplicator discards the
/// second copy.
///
/// The surviving link is chosen in three passes, and the pass order is the policy:
///
/// 1. [`LinkRole::Central`] links flagged [`LinkCandidate::preferred_for_peer`] —
///    the peer's most recent binding.
/// 2. The remaining [`LinkRole::Central`] links. This side wins ties because it has
///    per-link flow control (`canSendWriteWithoutResponse`), where notifications
///    share one queue across every subscriber.
/// 3. [`LinkRole::Peripheral`] links, sharing the same `seen` set so a peer already
///    covered by a write link is not notified as well.
///
/// Known limitation, inherited: pass 3 has no recency signal, so a peer reachable
/// only by inbound subscriptions rides its oldest one until the remote side
/// consolidates.
///
/// Unbound links pass through untouched — they are pre-announce, and collapsing
/// them is what would keep them that way.
fn collapse_duplicate_links_per_peer<'a>(links: &[&'a LinkCandidate]) -> Vec<&'a LinkCandidate> {
    let mut seen: BTreeSet<&'a PeerId> = BTreeSet::new();
    let mut kept: Vec<&'a LinkCandidate> = Vec::with_capacity(links.len());

    for link in links.iter().copied() {
        if link.role != LinkRole::Central || !link.preferred_for_peer {
            continue;
        }
        let Some(peer) = link.peer.as_ref() else {
            continue;
        };
        if seen.insert(peer) {
            kept.push(link);
        }
    }

    for link in links.iter().copied() {
        if link.role != LinkRole::Central {
            continue;
        }
        if let Some(peer) = link.peer.as_ref() {
            // Already taken by pass 1; there is at most one preferred link per
            // peer, so this only ever skips a duplicate append.
            if link.preferred_for_peer {
                continue;
            }
            if !seen.insert(peer) {
                continue;
            }
        }
        kept.push(link);
    }

    for link in links.iter().copied() {
        if link.role != LinkRole::Peripheral {
            continue;
        }
        if let Some(peer) = link.peer.as_ref() {
            if !seen.insert(peer) {
                continue;
            }
        }
        kept.push(link);
    }

    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: &str) -> PeerId {
        PeerId::from_str(n)
    }

    fn central(id: &str) -> LinkCandidate {
        LinkCandidate::new(LinkId::new(id), LinkRole::Central, 512)
    }

    fn peripheral(id: &str) -> LinkCandidate {
        LinkCandidate::new(LinkId::new(id), LinkRole::Peripheral, 512)
    }

    fn ids(selection: &FanoutSelection) -> Vec<String> {
        selection.iter().map(|id| id.as_str().to_owned()).collect()
    }

    /// Eight central-role links, named so the golden vectors below stay readable.
    fn eight_links() -> Vec<LinkCandidate> {
        (0..8).map(|i| central(&format!("L{i}"))).collect()
    }

    const MESSAGE: u8 = MessageType::Message as u8;
    const ANNOUNCE: u8 = MessageType::Announce as u8;
    const FRAGMENT: u8 = MessageType::Fragment as u8;
    const REQUEST_SYNC: u8 = MessageType::RequestSync as u8;
    const NOISE: u8 = MessageType::NoiseEncrypted as u8;

    // ---- subset sizing -------------------------------------------------

    #[test]
    fn subset_size_scales_with_degree() {
        // floor(log2(n-1)) + 2, with 0..=2 exempt.
        let expected = [
            (0, 0),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 3),
            (5, 4),
            (6, 4),
            (7, 4),
            (8, 4),
            (9, 5),
            (16, 5),
            (17, 6),
            (32, 6),
            (33, 7),
            (64, 7),
        ];
        for (degree, picked) in expected {
            assert_eq!(subset_size(degree), picked, "degree {degree}");
        }
    }

    #[test]
    fn subset_size_grows_logarithmically_and_never_exceeds_degree() {
        let mut previous = 0;
        for degree in 1..4096usize {
            let picked = subset_size(degree);
            assert!(picked <= degree, "degree {degree} picked {picked}");
            assert!(picked >= previous, "degree {degree} went backwards");
            assert!(picked >= 1);
            previous = picked;
        }
        // A 4095-link node is absurd, but the point is the shape: 13 links, not
        // 4095. Anything super-logarithmic here is a flood.
        assert_eq!(subset_size(4095), 13);
    }

    // ---- determinism ---------------------------------------------------

    #[test]
    fn subset_is_identical_across_repeated_calls() {
        let links = eight_links();
        let req = FanoutRequest::new(&links, MESSAGE, "msg-1");
        let first = select_links(&req);
        for _ in 0..16 {
            assert_eq!(select_links(&req), first);
        }
    }

    #[test]
    fn subset_is_identical_across_independent_inputs_and_orderings() {
        // "Across selector instances" in a module of free functions means: no
        // hidden state, and no dependence on how the caller happened to order or
        // allocate its snapshot.
        let forward = eight_links();
        let mut reversed = eight_links();
        reversed.reverse();

        let a = select_links(&FanoutRequest::new(&forward, MESSAGE, "msg-1"));
        let b = select_links(&FanoutRequest::new(&reversed, MESSAGE, "msg-1"));
        assert_eq!(a, b);
    }

    #[test]
    fn subset_matches_the_golden_vector() {
        // Computed independently: sha256("msg-1::L{i}") for i in 0..8, sorted
        // ascending, first subset_size(8) = 4 taken.
        //
        //   0881a4a5… L1
        //   164e2f46… L6
        //   6ce795af… L5
        //   8467a641… L7
        //   a04a03f1… L3   <- cut
        //   c3f8e686… L0
        //   e1f1127e… L2
        //   e7b78ae8… L4
        //
        // If this test fails, the hash preimage changed. Check the separator and
        // the seed/id order before touching anything else.
        let links = eight_links();
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert_eq!(ids(&selected), vec!["L1", "L5", "L6", "L7"]);
    }

    #[test]
    fn subset_matches_the_golden_vector_at_a_different_degree() {
        // Five links, subset_size(5) = 4. Same digests, restricted to L0..L4:
        // L1, L3, L0, L2 survive; L4 (e7b78ae8…) is cut.
        let links: Vec<LinkCandidate> = (0..5).map(|i| central(&format!("L{i}"))).collect();
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert_eq!(ids(&selected), vec!["L0", "L1", "L2", "L3"]);
    }

    #[test]
    fn a_different_message_selects_a_different_subset() {
        // sha256("msg-2::L{i}") sorts to L0, L1, L6, L5 — overlapping but not
        // equal. If every message picked the same links, the unpicked ones would
        // never carry anything.
        let links = eight_links();
        let one = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        let two = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-2"));
        assert_eq!(ids(&two), vec!["L0", "L1", "L5", "L6"]);
        assert_ne!(one, two);
    }

    #[test]
    fn every_link_is_reachable_by_some_message() {
        // Over many messages the subsets must cover the whole link set, or a
        // neighbour is permanently deaf to us.
        let links = eight_links();
        let mut covered = BTreeSet::new();
        for n in 0..64 {
            let seed = format!("m{n}");
            let selected = select_links(&FanoutRequest::new(&links, MESSAGE, &seed));
            for id in &selected {
                covered.insert(id.clone());
            }
        }
        assert_eq!(covered.len(), links.len());
    }

    #[test]
    fn subset_size_is_respected_per_role() {
        // 4 central + 4 peripheral: 3 from each, not 3 from a pool of 8.
        let mut links: Vec<LinkCandidate> = (0..4).map(|i| central(&format!("C{i}"))).collect();
        links.extend((0..4).map(|i| peripheral(&format!("P{i}"))));

        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert_eq!(selected.len(), 6);
        let picked = ids(&selected);
        assert_eq!(picked.iter().filter(|id| id.starts_with('C')).count(), 3);
        assert_eq!(picked.iter().filter(|id| id.starts_with('P')).count(), 3);
    }

    #[test]
    fn small_degrees_are_never_thinned() {
        for degree in 1..=2usize {
            let links: Vec<LinkCandidate> =
                (0..degree).map(|i| central(&format!("L{i}"))).collect();
            let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
            assert_eq!(selected.len(), degree);
        }
    }

    #[test]
    fn no_links_selects_nothing() {
        let selected = select_links(&FanoutRequest::new(&[], MESSAGE, "msg-1"));
        assert!(selected.is_empty());
    }

    // ---- full-fanout exemptions ----------------------------------------

    #[test]
    fn announces_fragments_and_sync_bypass_subsetting() {
        let links = eight_links();
        for packet_type in [ANNOUNCE, FRAGMENT, REQUEST_SYNC] {
            let selected = select_links(&FanoutRequest::new(&links, packet_type, "msg-1"));
            assert_eq!(
                selected.len(),
                8,
                "type {packet_type:#04x} must reach every link"
            );
        }
    }

    #[test]
    fn ordinary_broadcasts_do_not_bypass_subsetting() {
        let links = eight_links();
        for packet_type in [MESSAGE, NOISE, MessageType::VoiceFrame as u8, 0x7F] {
            let selected = select_links(&FanoutRequest::new(&links, packet_type, "msg-1"));
            assert_eq!(
                selected.len(),
                4,
                "type {packet_type:#04x} must be subsetted"
            );
        }
    }

    #[test]
    fn exempt_types_still_honour_split_horizon() {
        let links = eight_links();
        let ingress = LinkId::new("L3");
        let selected = select_links(
            &FanoutRequest::new(&links, ANNOUNCE, "msg-1").with_ingress_link(Some(&ingress)),
        );
        assert_eq!(selected.len(), 7);
        assert!(!selected.contains(&ingress));
    }

    // ---- split horizon and exclusions ----------------------------------

    #[test]
    fn the_ingress_link_is_always_excluded() {
        let links = eight_links();
        for packet_type in [MESSAGE, ANNOUNCE, FRAGMENT, REQUEST_SYNC, NOISE] {
            for i in 0..8 {
                let ingress = LinkId::new(format!("L{i}"));
                let selected = select_links(
                    &FanoutRequest::new(&links, packet_type, "msg-1")
                        .with_ingress_link(Some(&ingress)),
                );
                assert!(
                    !selected.contains(&ingress),
                    "type {packet_type:#04x} echoed back down its ingress link"
                );
            }
        }
    }

    #[test]
    fn explicitly_excluded_links_are_dropped() {
        let links = eight_links();
        let excluded = [LinkId::new("L0"), LinkId::new("L4")];
        let selected = select_links(
            &FanoutRequest::new(&links, ANNOUNCE, "msg-1").with_excluded_links(&excluded),
        );
        assert_eq!(selected.len(), 6);
        for id in &excluded {
            assert!(!selected.contains(id));
        }
    }

    #[test]
    fn exclusion_shrinks_the_subset_size_too() {
        // Three links minus the ingress leaves two, and two is exempt from
        // thinning — so the subset is computed after the exclusion, not before.
        let links: Vec<LinkCandidate> = (0..3).map(|i| central(&format!("L{i}"))).collect();
        let ingress = LinkId::new("L1");
        let selected = select_links(
            &FanoutRequest::new(&links, MESSAGE, "msg-1").with_ingress_link(Some(&ingress)),
        );
        assert_eq!(ids(&selected), vec!["L0", "L2"]);
    }

    // ---- directed packets ----------------------------------------------

    #[test]
    fn directed_packets_go_only_to_the_bound_link() {
        let alice = peer("aaaa");
        let bob = peer("bbbb");
        let links = vec![
            central("to-alice").bound_to(alice.clone()),
            central("to-bob").bound_to(bob.clone()),
            central("unbound-0"),
            central("unbound-1"),
            peripheral("from-carol").bound_to(peer("cccc")),
        ];

        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1").with_directed_peer(Some(&bob)),
        );
        assert_eq!(ids(&selected), vec!["to-bob"]);
    }

    #[test]
    fn a_directed_packet_never_gets_subsetted_away() {
        // Degree 8, so a broadcast would pick 4 — the directed path must ignore
        // that entirely and still pick exactly its one link.
        let bob = peer("bbbb");
        let mut links = eight_links();
        links.push(central("to-bob").bound_to(bob.clone()));

        let selected = select_links(
            &FanoutRequest::new(&links, MESSAGE, "msg-1").with_directed_peer(Some(&bob)),
        );
        assert_eq!(ids(&selected), vec!["to-bob"]);
    }

    #[test]
    fn a_directed_packet_prefers_the_write_side_of_a_dual_role_pair() {
        let bob = peer("bbbb");
        let links = vec![
            peripheral("bob-notify").bound_to(bob.clone()),
            central("bob-write").bound_to(bob.clone()),
        ];
        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1").with_directed_peer(Some(&bob)),
        );
        assert_eq!(ids(&selected), vec!["bob-write"]);
    }

    #[test]
    fn a_directed_packet_uses_the_notify_side_when_that_is_all_there_is() {
        let bob = peer("bbbb");
        let links = vec![
            peripheral("bob-notify-old").bound_to(bob.clone()),
            peripheral("bob-notify-new").bound_to(bob.clone()),
        ];
        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1").with_directed_peer(Some(&bob)),
        );
        // Oldest-first, the inherited limitation: there is no recency signal on
        // this side.
        assert_eq!(ids(&selected), vec!["bob-notify-old"]);
    }

    #[test]
    fn a_directed_packet_takes_the_peers_preferred_link() {
        let bob = peer("bbbb");
        let links = vec![
            central("bob-stale").bound_to(bob.clone()),
            central("bob-fresh").bound_to(bob.clone()).preferred(),
        ];
        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1").with_directed_peer(Some(&bob)),
        );
        assert_eq!(ids(&selected), vec!["bob-fresh"]);
    }

    #[test]
    fn a_directed_packet_with_no_bound_link_floods_every_allowed_link() {
        // We have no link to Bob, so the packet is relayed toward him. Note the
        // absence of subsetting: a directed packet is never thinned.
        let bob = peer("bbbb");
        let links = eight_links();
        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1").with_directed_peer(Some(&bob)),
        );
        assert_eq!(selected.len(), 8);
    }

    #[test]
    fn a_directed_packet_sends_nothing_when_its_only_link_was_excluded() {
        // The split horizon must not be routed around: Bob's link is the ingress,
        // so the packet stops here rather than flooding to reach him anyway.
        let bob = peer("bbbb");
        let ingress = LinkId::new("to-bob");
        let mut links = eight_links();
        links.push(central("to-bob").bound_to(bob.clone()));

        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1")
                .with_directed_peer(Some(&bob))
                .with_ingress_link(Some(&ingress)),
        );
        assert!(selected.is_empty());
    }

    #[test]
    fn require_direct_peer_link_refuses_to_flood() {
        let bob = peer("bbbb");
        let links = eight_links();
        let selected = select_links(
            &FanoutRequest::new(&links, NOISE, "msg-1")
                .with_directed_peer(Some(&bob))
                .with_require_direct_peer_link(true),
        );
        assert!(selected.is_empty());

        // …but it still uses the link when there is one.
        let mut with_bob = links;
        with_bob.push(central("to-bob").bound_to(bob.clone()));
        let selected = select_links(
            &FanoutRequest::new(&with_bob, NOISE, "msg-1")
                .with_directed_peer(Some(&bob))
                .with_require_direct_peer_link(true),
        );
        assert_eq!(ids(&selected), vec!["to-bob"]);
    }

    // ---- duplicate-link collapse ---------------------------------------

    #[test]
    fn a_dual_role_pair_receives_one_copy() {
        let bob = peer("bbbb");
        let links = vec![
            central("bob-write").bound_to(bob.clone()),
            peripheral("bob-notify").bound_to(bob.clone()),
        ];
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert_eq!(ids(&selected), vec!["bob-write"]);
    }

    #[test]
    fn collapse_prefers_the_flagged_link_over_input_order() {
        let bob = peer("bbbb");
        let links = vec![
            central("bob-stale").bound_to(bob.clone()),
            central("bob-fresh").bound_to(bob.clone()).preferred(),
            peripheral("bob-notify").bound_to(bob.clone()),
        ];
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert_eq!(ids(&selected), vec!["bob-fresh"]);
    }

    #[test]
    fn unbound_links_survive_the_collapse() {
        // Pre-announce links are exactly the ones still waiting for the announce
        // that would bind them. Collapsing them keeps them unbound forever.
        let bob = peer("bbbb");
        let links = vec![
            central("bob-write").bound_to(bob.clone()),
            peripheral("bob-notify").bound_to(bob.clone()),
            central("pre-announce-0"),
            peripheral("pre-announce-1"),
        ];
        let selected = select_links(&FanoutRequest::new(&links, ANNOUNCE, "msg-1"));
        assert_eq!(
            ids(&selected),
            vec!["bob-write", "pre-announce-0", "pre-announce-1"]
        );
    }

    #[test]
    fn collapse_can_be_disabled_so_every_link_gets_the_announce() {
        let bob = peer("bbbb");
        let links = vec![
            central("bob-write").bound_to(bob.clone()),
            peripheral("bob-notify").bound_to(bob.clone()),
        ];
        let selected = select_links(
            &FanoutRequest::new(&links, ANNOUNCE, "msg-1")
                .with_collapse_duplicate_peer_links(false),
        );
        assert_eq!(ids(&selected), vec!["bob-notify", "bob-write"]);
    }

    #[test]
    fn collapse_keeps_distinct_peers_apart() {
        let links = vec![
            central("a").bound_to(peer("aaaa")),
            central("b").bound_to(peer("bbbb")),
            peripheral("c").bound_to(peer("cccc")),
        ];
        let selected = select_links(&FanoutRequest::new(&links, ANNOUNCE, "msg-1"));
        assert_eq!(ids(&selected), vec!["a", "b", "c"]);
    }

    #[test]
    fn collapse_runs_before_subsetting() {
        // Six links but only three peers: the subset is drawn from the three
        // survivors (2 central + 1 peripheral), not from all six.
        let mut links = Vec::new();
        for name in ["aaaa", "bbbb", "cccc"] {
            let p = peer(name);
            links.push(central(&format!("{name}-write")).bound_to(p.clone()));
            links.push(peripheral(&format!("{name}-notify")).bound_to(p));
        }
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        // 3 central survivors -> subset_size(3) = 3; 0 peripheral survivors.
        assert_eq!(
            ids(&selected),
            vec!["aaaa-write", "bbbb-write", "cccc-write"]
        );
    }

    // ---- deterministic_subset directly ---------------------------------

    #[test]
    fn deterministic_subset_returns_everything_when_k_covers_the_input() {
        let ids: Vec<LinkId> = ["a", "b", "c"].iter().map(|s| LinkId::new(*s)).collect();
        assert_eq!(deterministic_subset(&ids, 3, "seed").len(), 3);
        assert_eq!(deterministic_subset(&ids, 9, "seed").len(), 3);
        // k == 0 is Swift's unreachable branch: it returns everything rather
        // than nothing.
        assert_eq!(deterministic_subset(&ids, 0, "seed").len(), 3);
        assert!(deterministic_subset(&[], 4, "seed").is_empty());
    }

    #[test]
    fn deterministic_subset_depends_on_the_seed_and_the_id() {
        let ids: Vec<LinkId> = (0..8).map(|i| LinkId::new(format!("L{i}"))).collect();
        let a = deterministic_subset(&ids, 4, "seed-a");
        let b = deterministic_subset(&ids, 4, "seed-b");
        assert_eq!(a.len(), 4);
        assert_eq!(b.len(), 4);
        assert_eq!(deterministic_subset(&ids, 4, "seed-a"), a);
        assert_ne!(a, b);
    }

    #[test]
    fn selection_iteration_and_membership() {
        let links = eight_links();
        let selected = select_links(&FanoutRequest::new(&links, MESSAGE, "msg-1"));
        assert!(!selected.is_empty());
        assert_eq!(selected.iter().count(), selected.len());
        for id in &selected {
            assert!(selected.contains(id));
        }
        assert!(!selected.contains(&LinkId::new("nope")));
        assert!(FanoutSelection::new().is_empty());
    }
}
