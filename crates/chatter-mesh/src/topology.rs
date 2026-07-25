//! Mesh topology tracking and v2 source routing.
//!
//! Ported from `Services/MeshTopologyTracker.swift`,
//! `Services/BLE/BLESourceRouteOriginationPolicy.swift`,
//! `Services/BLE/BLERouteForwardingPolicy.swift`, and `docs/SOURCE_ROUTING.md`
//! §7 (relay logic) and §8 (origination policy).
//!
//! Peers gossip their direct neighbours in a TLV on every announce. Those claims
//! are *directed*: A saying it sees B is not evidence B sees A. Only a
//! bidirectionally confirmed edge — both endpoints naming each other, both
//! claims fresh — is usable for routing, because a one-sided claim is either a
//! link that has already died in one direction or a fabrication.
//!
//! A route is a bet that one specific path survives the next few hundred
//! milliseconds; a flood heals around a broken hop and a route does not. That is
//! why origination is gated so tightly, and why every gate failing simply means
//! "flood instead" rather than "drop".

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::packet::{BROADCAST_RECIPIENT, BitchatPacket, SENDER_ID_LEN};

/// `MeshTopologyTracker.routeFreshnessThreshold`. Claims older than this are
/// neither routed over nor reported as edges: a route computed from stale
/// topology fails where a flood would have got through.
pub const NEIGHBOR_FRESHNESS_SECS: u64 = 60;

/// `TransportConfig.bleSourceRouteMaxIntermediateHops`. Deliberately small:
/// every hop must be fresh, confirmed, and v2-capable, and a long path fails
/// more often than a flood.
pub const MAX_INTERMEDIATE_HOPS: usize = 4;

/// The protocol version a node must have been observed speaking before we route
/// through it. A v1-only node drops a v2 frame on decode, so routing through one
/// loses the packet silently.
pub const ROUTE_REQUIRED_VERSION: u8 = 2;

type PeerId = [u8; SENDER_ID_LEN];

/// The observed mesh graph: who claims to see whom, when we last heard it, and
/// which nodes have been seen speaking v2.
#[derive(Clone, Debug, Default)]
pub struct MeshTopology {
    /// Directed claims: the key claims to see every peer in the value.
    claims: BTreeMap<PeerId, BTreeSet<PeerId>>,
    /// When each peer's claim set was last refreshed.
    last_seen: BTreeMap<PeerId, u64>,
    /// Highest protocol version observed from each peer. Absent means v1-only.
    observed_versions: BTreeMap<PeerId, u8>,
}

impl MeshTopology {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces a peer's self-reported neighbour list.
    ///
    /// Self-loops are dropped and duplicates collapse, as in
    /// `MeshTopologyTracker.updateNeighbors`. The list replaces rather than
    /// merges: an announce is a complete statement of what that peer can see
    /// right now, so a neighbour it stops naming must stop being an edge.
    pub fn update_neighbors(&mut self, peer: PeerId, neighbors: Vec<PeerId>, now_unix_secs: u64) {
        let set: BTreeSet<PeerId> = neighbors
            .into_iter()
            .filter(|neighbor| *neighbor != peer)
            .collect();
        self.claims.insert(peer, set);
        self.last_seen.insert(peer, now_unix_secs);
    }

    /// Records the protocol version seen on a decoded packet from `peer`.
    ///
    /// Only versions above the v1 baseline are stored, and the highest wins —
    /// a peer that has spoken v2 once can decode v2.
    ///
    /// Deviation, forced by the frozen signature: the Swift stamps each entry
    /// with a `seenAt` and expires it independently. This function is handed no
    /// clock, so [`evict_stale`](Self::evict_stale) instead drops version
    /// entries for peers that have no fresh claim entry. That is strictly
    /// conservative for routing — every hop on a route must be a fresh,
    /// confirmed node anyway — and it keeps the map bounded by the claim map
    /// rather than by the set of sender IDs a stranger can invent.
    pub fn record_observed_version(&mut self, peer: PeerId, version: u8) {
        if version <= 1 {
            return;
        }
        let entry = self.observed_versions.entry(peer).or_insert(version);
        *entry = (*entry).max(version);
    }

    /// Forgets a peer entirely.
    pub fn remove_peer(&mut self, peer: &PeerId) {
        self.claims.remove(peer);
        self.last_seen.remove(peer);
        self.observed_versions.remove(peer);
    }

    /// Drops claims not refreshed within [`NEIGHBOR_FRESHNESS_SECS`], and the
    /// version entries of peers left with no claims.
    pub fn evict_stale(&mut self, now_unix_secs: u64) {
        let last_seen = &mut self.last_seen;
        let claims = &mut self.claims;
        last_seen.retain(|peer, seen| {
            // Swift prunes on `lastSeen < now - age`, so a claim exactly at the
            // boundary survives the sweep even though the BFS below already
            // treats it as stale. Ported as written.
            let fresh = now_unix_secs.saturating_sub(*seen) <= NEIGHBOR_FRESHNESS_SECS;
            if !fresh {
                claims.remove(peer);
            }
            fresh
        });
        self.observed_versions
            .retain(|peer, _| last_seen.contains_key(peer));
    }

    /// Whether `peer` has been observed speaking v2 or better.
    pub fn is_v2_capable(&self, peer: &PeerId) -> bool {
        self.observed_versions
            .get(peer)
            .is_some_and(|version| *version >= ROUTE_REQUIRED_VERSION)
    }

    /// Every bidirectionally confirmed, fresh edge, each reported once with the
    /// lower peer ID first.
    ///
    /// One-sided claims are excluded — that is the whole point of the two-way
    /// handshake in `SOURCE_ROUTING.md` §4.
    pub fn confirmed_edges(&self, now_unix_secs: u64) -> Vec<(PeerId, PeerId)> {
        let mut edges = Vec::new();
        for (peer, neighbors) in &self.claims {
            if !self.is_fresh(peer, now_unix_secs) {
                continue;
            }
            for neighbor in neighbors {
                // Emit each edge once.
                if neighbor <= peer {
                    continue;
                }
                if !self.is_fresh(neighbor, now_unix_secs) {
                    continue;
                }
                if self.claims_edge(neighbor, peer) {
                    edges.push((*peer, *neighbor));
                }
            }
        }
        edges
    }

    /// Shortest path over confirmed, fresh edges, as intermediate hops only —
    /// neither endpoint is included, matching the wire format's "intermediate
    /// hops only" rule (`SOURCE_ROUTING.md` §3).
    ///
    /// `from == to` yields an empty path: a direct connection needs no hops.
    /// `max_hops` bounds the number of intermediate hops.
    pub fn path_to(
        &self,
        from: PeerId,
        to: PeerId,
        max_hops: usize,
        now_unix_secs: u64,
    ) -> Option<Vec<PeerId>> {
        self.bfs(from, to, max_hops, now_unix_secs, false)
    }

    /// [`path_to`](Self::path_to), additionally requiring every intermediate hop
    /// *and* the destination to have been observed speaking v2.
    ///
    /// This is a separate entry point rather than a flag on `path_to` because
    /// the gate has to be applied inside the search: filtering a finished path
    /// would reject a route that a version-aware search would have found by
    /// going around the v1 node. It corresponds to the Swift's
    /// `computeRoute(..., requiringVersion: 2)`, which is what both production
    /// call sites use.
    pub fn path_to_v2(
        &self,
        from: PeerId,
        to: PeerId,
        max_hops: usize,
        now_unix_secs: u64,
    ) -> Option<Vec<PeerId>> {
        self.bfs(from, to, max_hops, now_unix_secs, true)
    }

    /// Breadth-first search over confirmed edges, ported from
    /// `MeshTopologyTracker.computeRoute`.
    ///
    /// Neighbours are visited in ID order, which the Swift's `Set` iteration
    /// does not guarantee. Among equal-length paths the choice is arbitrary
    /// either way; ordering it makes the route we originate reproducible.
    fn bfs(
        &self,
        from: PeerId,
        to: PeerId,
        max_hops: usize,
        now_unix_secs: u64,
        require_v2: bool,
    ) -> Option<Vec<PeerId>> {
        if from == to {
            return Some(Vec::new());
        }

        let mut visited: BTreeSet<PeerId> = BTreeSet::new();
        visited.insert(from);

        let mut frontier: VecDeque<Vec<PeerId>> = VecDeque::new();
        frontier.push_back(vec![from]);

        while let Some(path) = frontier.pop_front() {
            // `path` holds the source plus the hops so far, so this bounds the
            // intermediate hops at `max_hops`.
            if path.len() > max_hops + 1 {
                continue;
            }
            let Some(last) = path.last().copied() else {
                continue;
            };
            let Some(neighbors) = self.claims.get(&last) else {
                continue;
            };
            if !self.is_fresh(&last, now_unix_secs) {
                continue;
            }

            for neighbor in neighbors {
                if visited.contains(neighbor) {
                    continue;
                }
                if require_v2 && !self.is_v2_capable(neighbor) {
                    continue;
                }
                // Confirmed edge: `last` claims `neighbor` (we are iterating
                // its claims) and `neighbor` must claim `last` back.
                if !self.claims_edge(neighbor, &last) {
                    continue;
                }
                if !self.is_fresh(neighbor, now_unix_secs) {
                    continue;
                }

                if *neighbor == to {
                    // Drop the source; the destination was never pushed.
                    return Some(path[1..].to_vec());
                }

                visited.insert(*neighbor);
                let mut next = path.clone();
                next.push(*neighbor);
                frontier.push_back(next);
            }
        }

        None
    }

    fn is_fresh(&self, peer: &PeerId, now_unix_secs: u64) -> bool {
        self.last_seen
            .get(peer)
            .is_some_and(|seen| now_unix_secs.saturating_sub(*seen) < NEIGHBOR_FRESHNESS_SECS)
    }

    fn claims_edge(&self, peer: &PeerId, neighbor: &PeerId) -> bool {
        self.claims
            .get(peer)
            .is_some_and(|claimed| claimed.contains(neighbor))
    }
}

/// Inputs to the origination decision for one outbound packet.
pub struct RouteOriginationInput<'a> {
    pub packet: &'a BitchatPacket,
    /// Whether we authored this packet. Relays never attach a route: doing so
    /// would mean re-signing someone else's packet under our key.
    pub sender_is_self: bool,
    /// Whether the recipient is a direct BLE neighbour. A direct write already
    /// delivers in one hop, so a route would only add bytes.
    pub recipient_directly_connected: bool,
    /// Whether a recent routed send to this recipient went unconfirmed
    /// (`BLESourceRouteFailureCache`: no inbound packet from them within 10 s,
    /// suppressing routing for 60 s).
    pub recently_failed: bool,
}

/// The intermediate-hop route to attach to an outbound packet, or `None` to keep
/// the plain flood/direct-write path.
///
/// Every gate in `SOURCE_ROUTING.md` §8 must pass, in the same order as
/// `BLESourceRouteOriginationPolicy.route`. The BFS source is the packet's own
/// sender ID, which `sender_is_self` has just asserted is us — so the caller
/// must have recorded our own neighbour list via
/// [`MeshTopology::update_neighbors`], exactly as `refreshLocalTopology` does
/// before every route computation.
pub fn should_originate_route(
    input: &RouteOriginationInput,
    topology: &MeshTopology,
    now_unix_secs: u64,
) -> Option<Vec<PeerId>> {
    if !input.sender_is_self {
        return None;
    }
    let recipient = input.packet.recipient_id?;
    if recipient == BROADCAST_RECIPIENT {
        return None;
    }
    if input.packet.ttl <= 1 {
        return None;
    }
    if input.recipient_directly_connected {
        return None;
    }
    if input.recently_failed {
        return None;
    }

    let route = topology.path_to_v2(
        input.packet.sender_id,
        recipient,
        MAX_INTERMEDIATE_HOPS,
        now_unix_secs,
    )?;
    // An empty route means the topology says "direct"; there is nothing to
    // attach, and the direct-write path already covers it.
    if route.is_empty() {
        return None;
    }
    Some(route)
}

/// Where a routed packet goes next, or `None` to fall back to flooding.
///
/// Per `SOURCE_ROUTING.md` §7: find our own ID in the route at index `i`; the
/// next hop is `i + 1`, or the recipient when `i` is the last index.
///
/// **Deviation from the design brief, matching `BLERouteForwardingPolicy`:**
/// when our ID is *not* in the route, the Swift forwards to `route[0]` rather
/// than giving up — an off-route node that overhears a routed packet hands it to
/// the head of the route instead of flooding. Since the caller floods whenever
/// the returned hop is not a connected neighbour, this only changes behaviour in
/// the case where `route[0]` *is* our neighbour, where a unicast is strictly
/// better than a flood. The source wins over the brief here.
///
/// Suppression rules — REQUEST_SYNC, `ttl <= 1`, packets addressed to us — are
/// not repeated here: [`crate::relay::decide`] already answers "no relay" for all
/// three, and this function only answers "where would the route send it".
pub fn next_hop(packet: &BitchatPacket, local_id: &PeerId) -> Option<PeerId> {
    let route = packet.route.as_deref()?;
    match route.iter().position(|hop| hop == local_id) {
        None => route.first().copied(),
        Some(index) if index + 1 == route.len() => packet.recipient_id,
        Some(index) => route.get(index + 1).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::MessageType;

    const A: PeerId = [0xA1; SENDER_ID_LEN];
    const B: PeerId = [0xB2; SENDER_ID_LEN];
    const C: PeerId = [0xC3; SENDER_ID_LEN];
    const D: PeerId = [0xD4; SENDER_ID_LEN];
    const E: PeerId = [0xE5; SENDER_ID_LEN];

    /// A chain A-B-C-D with every claim mutual, every node v2, all seen at `t`.
    fn chain(t: u64) -> MeshTopology {
        let mut topology = MeshTopology::new();
        topology.update_neighbors(A, vec![B], t);
        topology.update_neighbors(B, vec![A, C], t);
        topology.update_neighbors(C, vec![B, D], t);
        topology.update_neighbors(D, vec![C], t);
        for peer in [A, B, C, D] {
            topology.record_observed_version(peer, 2);
        }
        topology
    }

    fn directed_packet(sender: PeerId, recipient: Option<PeerId>, ttl: u8) -> BitchatPacket {
        BitchatPacket::new(
            MessageType::NoiseEncrypted,
            sender,
            recipient,
            1_700_000_000_000,
            vec![0x01, 0x02],
            ttl,
        )
    }

    // --- claims and freshness -----------------------------------------------

    #[test]
    fn self_loops_and_duplicates_are_dropped() {
        let mut topology = MeshTopology::new();
        topology.update_neighbors(A, vec![B, A, B, C], 100);
        assert_eq!(topology.claims.get(&A), Some(&BTreeSet::from([B, C])));
    }

    #[test]
    fn a_neighbour_list_replaces_rather_than_merges() {
        let mut topology = MeshTopology::new();
        topology.update_neighbors(A, vec![B, C], 100);
        topology.update_neighbors(B, vec![A], 100);
        topology.update_neighbors(C, vec![A], 100);
        assert_eq!(topology.confirmed_edges(100).len(), 2);

        // A stops seeing C.
        topology.update_neighbors(A, vec![B], 101);
        assert_eq!(topology.confirmed_edges(101), vec![(A, B)]);
    }

    #[test]
    fn one_sided_claims_are_not_edges_and_are_not_routed_over() {
        let mut topology = MeshTopology::new();
        topology.update_neighbors(A, vec![B], 100);
        topology.update_neighbors(B, vec![A, C], 100);
        // C never claims B back.
        topology.update_neighbors(C, vec![], 100);
        for peer in [A, B, C] {
            topology.record_observed_version(peer, 2);
        }

        assert_eq!(topology.confirmed_edges(100), vec![(A, B)]);
        assert_eq!(topology.path_to(A, C, 4, 100), None);

        // Once C names B, the edge is confirmed and the route appears.
        topology.update_neighbors(C, vec![B], 100);
        assert_eq!(topology.confirmed_edges(100), vec![(A, B), (B, C)]);
        assert_eq!(topology.path_to(A, C, 4, 100), Some(vec![B]));
    }

    #[test]
    fn a_peer_that_has_gone_quiet_is_not_routed_over() {
        let topology = chain(0);
        assert_eq!(topology.path_to(A, D, 4, 59), Some(vec![B, C]));
        // Exactly at the freshness threshold the claims are already stale.
        assert_eq!(topology.path_to(A, D, 4, NEIGHBOR_FRESHNESS_SECS), None);
        assert!(topology.confirmed_edges(NEIGHBOR_FRESHNESS_SECS).is_empty());
    }

    #[test]
    fn one_stale_node_removes_only_the_paths_through_it() {
        let mut topology = chain(0);
        // A, B and D refresh; C goes quiet.
        topology.update_neighbors(A, vec![B], 100);
        topology.update_neighbors(B, vec![A, C], 100);
        topology.update_neighbors(D, vec![C], 100);

        assert_eq!(topology.path_to(A, B, 4, 100), Some(vec![]));
        assert_eq!(topology.path_to(A, D, 4, 100), None);
        assert_eq!(topology.confirmed_edges(100), vec![(A, B)]);
    }

    #[test]
    fn eviction_drops_stale_claims_and_their_version_entries() {
        let mut topology = chain(0);
        topology.update_neighbors(A, vec![B], 100);

        topology.evict_stale(100);
        assert!(topology.claims.contains_key(&A));
        assert!(topology.is_v2_capable(&A));
        // B, C and D were last seen at 0.
        assert!(!topology.claims.contains_key(&B));
        assert!(!topology.is_v2_capable(&B));
        assert!(!topology.last_seen.contains_key(&C));
    }

    #[test]
    fn eviction_keeps_a_claim_sitting_exactly_on_the_boundary() {
        let mut topology = chain(0);
        topology.evict_stale(NEIGHBOR_FRESHNESS_SECS);
        assert!(
            topology.claims.contains_key(&A),
            "prune uses <= where the BFS uses <"
        );
        topology.evict_stale(NEIGHBOR_FRESHNESS_SECS + 1);
        assert!(topology.claims.is_empty());
    }

    #[test]
    fn removing_a_peer_forgets_everything_about_it() {
        let mut topology = chain(10);
        topology.remove_peer(&C);
        assert!(!topology.is_v2_capable(&C));
        assert_eq!(topology.path_to(A, D, 4, 10), None);
    }

    // --- version gate --------------------------------------------------------

    #[test]
    fn only_versions_above_one_are_recorded_and_the_highest_wins() {
        let mut topology = MeshTopology::new();
        assert!(!topology.is_v2_capable(&A));

        topology.record_observed_version(A, 1);
        assert!(!topology.is_v2_capable(&A), "v1 is the baseline, not evidence");

        topology.record_observed_version(A, 2);
        assert!(topology.is_v2_capable(&A));

        topology.record_observed_version(A, 3);
        topology.record_observed_version(A, 2);
        assert!(topology.is_v2_capable(&A), "a downgrade never lowers the record");
    }

    #[test]
    fn a_v1_hop_blocks_the_version_gated_search_but_not_the_plain_one() {
        let mut topology = chain(10);
        topology.observed_versions.remove(&C);

        assert_eq!(topology.path_to(A, D, 4, 10), Some(vec![B, C]));
        assert_eq!(topology.path_to_v2(A, D, 4, 10), None);
    }

    #[test]
    fn the_gated_search_routes_around_a_v1_node() {
        // A-B-D and A-C-D, with B v1-only.
        let mut topology = MeshTopology::new();
        topology.update_neighbors(A, vec![B, C], 10);
        topology.update_neighbors(B, vec![A, D], 10);
        topology.update_neighbors(C, vec![A, D], 10);
        topology.update_neighbors(D, vec![B, C], 10);
        for peer in [A, C, D] {
            topology.record_observed_version(peer, 2);
        }

        // B sorts before C, so the ungated search picks it.
        assert_eq!(topology.path_to(A, D, 4, 10), Some(vec![B]));
        // The gate has to be applied inside the search to find the C path.
        assert_eq!(topology.path_to_v2(A, D, 4, 10), Some(vec![C]));
    }

    #[test]
    fn a_v1_destination_is_never_routed_to() {
        let mut topology = chain(10);
        topology.observed_versions.remove(&D);
        assert_eq!(topology.path_to_v2(A, D, 4, 10), None);
        assert_eq!(topology.path_to_v2(A, C, 4, 10), Some(vec![B]));
    }

    // --- BFS shape -----------------------------------------------------------

    #[test]
    fn a_direct_edge_yields_an_empty_hop_list() {
        let topology = chain(10);
        assert_eq!(topology.path_to(A, B, 4, 10), Some(vec![]));
        assert_eq!(topology.path_to(A, A, 4, 10), Some(vec![]));
    }

    #[test]
    fn max_hops_bounds_the_intermediate_hops_exactly() {
        let topology = chain(10);
        // A -> D needs two intermediate hops.
        assert_eq!(topology.path_to(A, D, 2, 10), Some(vec![B, C]));
        assert_eq!(topology.path_to(A, D, 1, 10), None);
        // A -> B needs none, so even a zero budget finds it.
        assert_eq!(topology.path_to(A, B, 0, 10), Some(vec![]));
        assert_eq!(topology.path_to(A, C, 0, 10), None);
    }

    #[test]
    fn an_unknown_peer_is_unreachable() {
        let topology = chain(10);
        assert_eq!(topology.path_to(A, E, 4, 10), None);
        assert_eq!(topology.path_to(E, A, 4, 10), None);
    }

    #[test]
    fn the_shortest_path_wins() {
        let mut topology = chain(10);
        // Add the shortcut A-D.
        topology.update_neighbors(A, vec![B, D], 10);
        topology.update_neighbors(D, vec![C, A], 10);
        assert_eq!(topology.path_to(A, D, 4, 10), Some(vec![]));
    }

    // --- origination ---------------------------------------------------------

    fn origination(packet: &BitchatPacket) -> RouteOriginationInput<'_> {
        RouteOriginationInput {
            packet,
            sender_is_self: true,
            recipient_directly_connected: false,
            recently_failed: false,
        }
    }

    #[test]
    fn a_route_is_originated_when_every_gate_passes() {
        let topology = chain(10);
        let packet = directed_packet(A, Some(D), 7);
        assert_eq!(
            should_originate_route(&origination(&packet), &topology, 10),
            Some(vec![B, C])
        );
    }

    #[test]
    fn each_gate_alone_falls_back_to_flooding() {
        let topology = chain(10);
        let packet = directed_packet(A, Some(D), 7);

        let relayed = RouteOriginationInput {
            sender_is_self: false,
            ..origination(&packet)
        };
        assert_eq!(should_originate_route(&relayed, &topology, 10), None);

        let connected = RouteOriginationInput {
            recipient_directly_connected: true,
            ..origination(&packet)
        };
        assert_eq!(should_originate_route(&connected, &topology, 10), None);

        let failed = RouteOriginationInput {
            recently_failed: true,
            ..origination(&packet)
        };
        assert_eq!(should_originate_route(&failed, &topology, 10), None);

        let broadcast = directed_packet(A, Some(BROADCAST_RECIPIENT), 7);
        assert_eq!(
            should_originate_route(&origination(&broadcast), &topology, 10),
            None
        );

        let unaddressed = directed_packet(A, None, 7);
        assert_eq!(
            should_originate_route(&origination(&unaddressed), &topology, 10),
            None
        );

        for ttl in [0u8, 1] {
            let link_local = directed_packet(A, Some(D), ttl);
            assert_eq!(
                should_originate_route(&origination(&link_local), &topology, 10),
                None,
                "ttl {ttl}"
            );
        }
        // ttl 2 is the first value with headroom.
        let just_enough = directed_packet(A, Some(D), 2);
        assert!(should_originate_route(&origination(&just_enough), &topology, 10).is_some());

        // Stale topology.
        assert_eq!(
            should_originate_route(&origination(&packet), &topology, 10_000),
            None
        );
    }

    #[test]
    fn a_directly_reachable_recipient_yields_no_route_even_when_unflagged() {
        let topology = chain(10);
        // A and B are adjacent: the BFS returns an empty hop list, which is not
        // a route to attach.
        let packet = directed_packet(A, Some(B), 7);
        assert_eq!(
            should_originate_route(&origination(&packet), &topology, 10),
            None
        );
    }

    #[test]
    fn origination_respects_the_four_hop_ceiling() {
        // A-B-C-D-E: four intermediate hops to E is one too many... build it out.
        let mut topology = MeshTopology::new();
        let f: PeerId = [0xF6; SENDER_ID_LEN];
        topology.update_neighbors(A, vec![B], 10);
        topology.update_neighbors(B, vec![A, C], 10);
        topology.update_neighbors(C, vec![B, D], 10);
        topology.update_neighbors(D, vec![C, E], 10);
        topology.update_neighbors(E, vec![D, f], 10);
        topology.update_neighbors(f, vec![E], 10);
        for peer in [A, B, C, D, E, f] {
            topology.record_observed_version(peer, 2);
        }

        // E is four hops away: B, C, D are intermediates.
        let to_e = directed_packet(A, Some(E), 7);
        assert_eq!(
            should_originate_route(&origination(&to_e), &topology, 10),
            Some(vec![B, C, D])
        );

        // F needs four intermediates — exactly at MAX_INTERMEDIATE_HOPS.
        let to_f = directed_packet(A, Some(f), 7);
        assert_eq!(
            should_originate_route(&origination(&to_f), &topology, 10),
            Some(vec![B, C, D, E])
        );

        // One more node puts it out of reach.
        let g: PeerId = [0x67; SENDER_ID_LEN];
        topology.update_neighbors(f, vec![E, g], 10);
        topology.update_neighbors(g, vec![f], 10);
        topology.record_observed_version(g, 2);
        let to_g = directed_packet(A, Some(g), 7);
        assert_eq!(
            should_originate_route(&origination(&to_g), &topology, 10),
            None
        );
    }

    // --- forwarding ----------------------------------------------------------

    fn routed(route: Vec<PeerId>, recipient: PeerId) -> BitchatPacket {
        let mut packet = directed_packet(A, Some(recipient), 7);
        packet.version = 2;
        packet.route = Some(route);
        packet
    }

    #[test]
    fn the_next_hop_is_the_following_entry() {
        let packet = routed(vec![B, C], D);
        assert_eq!(next_hop(&packet, &B), Some(C));
    }

    #[test]
    fn the_last_hop_forwards_to_the_recipient() {
        let packet = routed(vec![B, C], D);
        assert_eq!(next_hop(&packet, &C), Some(D));
    }

    #[test]
    fn a_single_hop_route_forwards_straight_to_the_recipient() {
        let packet = routed(vec![B], D);
        assert_eq!(next_hop(&packet, &B), Some(D));
    }

    #[test]
    fn an_off_route_node_hands_the_packet_to_the_head_of_the_route() {
        // Ported from BLERouteForwardingPolicy, which forwards to route[0]
        // rather than flooding when the local ID is absent.
        let packet = routed(vec![B, C], D);
        assert_eq!(next_hop(&packet, &E), Some(B));
    }

    #[test]
    fn an_unrouted_packet_floods() {
        let mut packet = directed_packet(A, Some(D), 7);
        assert_eq!(next_hop(&packet, &B), None);

        packet.route = Some(Vec::new());
        assert_eq!(next_hop(&packet, &B), None);
    }

    #[test]
    fn a_last_hop_with_no_recipient_floods() {
        let mut packet = routed(vec![B], D);
        packet.recipient_id = None;
        assert_eq!(next_hop(&packet, &B), None);
    }

    #[test]
    fn a_repeated_hop_resolves_to_its_first_occurrence() {
        let packet = routed(vec![B, C, B], D);
        assert_eq!(next_hop(&packet, &B), Some(C));
    }
}
