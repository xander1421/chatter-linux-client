//! Outbound link planning: what to do with a packet that is about to leave.
//!
//! Ported from `Services/BLE/BLEOutboundLinkPlanner.swift`,
//! `BLEOutboundPacketPolicy.swift`, and `BLEDirectedRelaySpool.swift`.
//!
//! [`plan`] answers four questions at once, because they are not independent:
//!
//! - **Who is this for?** A recipient in the header, or an explicit override.
//! - **Which links does it go on?** Delegated to [`crate::fanout`].
//! - **Does it need fragmenting?** Only against the links actually selected — see
//!   the note on [`OutboundPlan::fragment_chunk_size`].
//! - **If it is directed and went nowhere, should we hold it?** [`DirectedRelaySpool`].
//!
//! Everything here is pure. Time enters only as a parameter to the spool.

use std::collections::BTreeMap;

use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BROADCAST_RECIPIENT, BitchatPacket, MessageType, SENDER_ID_LEN};
use chatter_mesh::relay::MESSAGE_TTL_DEFAULT;

use crate::fanout::{FanoutRequest, FanoutSelection, LinkCandidate, select_links};
use crate::link::LinkId;

/// Bytes a fragment costs on top of its chunk: the 13-byte fragment header, the
/// 8-byte sender and 8-byte recipient, and the packet header. A source route
/// adds one count byte and eight bytes per hop; `plan` accounts for those from
/// the packet because they vary per send.
///
/// Swift's `BLEOutboundPacketPolicy.fragmentFrameOverhead` counts a 13-byte
/// header, but the encoded header is 14 bytes in v1 and 16 in v2
/// (`chatter_mesh::codec`), so a full chunk overran the link limit by 1-3
/// bytes and the radio refused it. Budget for the larger v2 header.
pub const FRAGMENT_FRAME_OVERHEAD: usize = chatter_mesh::fragment::FRAGMENT_HEADER_LEN
    + SENDER_ID_LEN * 2
    + chatter_mesh::codec::V2_HEADER_SIZE;

/// Smallest chunk a fragment may carry, `BLEOutboundFragmentPlanner.minimumChunkSize`.
///
/// Re-exported from [`chatter_mesh::fragment`] rather than redeclared, so the
/// planner and the encoder can never disagree about the floor. A link whose MTU is
/// below the frame overhead would otherwise ask for a zero-byte chunk and never
/// finish.
pub use chatter_mesh::fragment::MIN_FRAGMENT_CHUNK;

/// `TransportConfig.bleDirectedSpoolWindowSeconds`.
///
/// A minute is deliberately generous: it lets an ordinary link gap — walking
/// between rooms, a reconnect — heal itself instead of dropping a directed packet
/// that has nowhere to go for two seconds.
pub const DIRECTED_SPOOL_WINDOW_SECS: u64 = 60;

/// What to do with one outbound packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundPlan {
    /// The peer this packet is addressed to, if any. `None` means broadcast.
    pub directed_peer: Option<PeerId>,

    /// When `Some`, the packet is too large for at least one selected link and
    /// must be re-sent as fragments of this size instead.
    ///
    /// The subtlety worth keeping: this is the minimum over the **selected** links
    /// only. Measuring every connected link instead lets an unrelated peer's small
    /// MTU declare a directed send oversized — and then every fragment it produces
    /// selects zero links, so the message silently never leaves.
    pub fragment_chunk_size: Option<usize>,

    /// The links to write to.
    pub selected: FanoutSelection,

    /// The packet is directed, has no link to go down, and is worth holding.
    /// See [`DirectedRelaySpool`].
    pub should_spool_directed: bool,
}

/// Inputs to [`plan`].
///
/// Built with [`OutboundPlanRequest::new`] plus `with_*` setters. Link state
/// arrives as a plain slice of [`LinkCandidate`] rather than a registry handle,
/// which is what keeps the whole module pure.
#[derive(Clone, Debug)]
pub struct OutboundPlanRequest<'a> {
    pub packet: &'a BitchatPacket,
    /// Length of the encoded, padded frame — what will actually hit the wire.
    /// Compare against link MTUs, not `packet.payload.len()`.
    pub encoded_len: usize,
    /// Every live, writable link. A link whose characteristic has not been
    /// discovered yet must be left out: offering it here wastes a fanout slot, and
    /// a peer whose single collapsed copy lands on it loses the packet outright.
    pub links: &'a [LinkCandidate],
    /// The link this packet arrived on, for split horizon.
    pub ingress_link: Option<&'a LinkId>,
    /// Further links to leave out.
    pub excluded_links: &'a [LinkId],
    /// Force a recipient regardless of the packet header.
    pub directed_only_peer: Option<&'a PeerId>,
    /// Refuse to flood when the recipient has no link.
    pub require_direct_peer_link: bool,
    /// The TTL that marks an announce as *direct* (first hop, not relayed).
    /// `TransportConfig.messageTTLDefault` unless the caller overrides it.
    pub direct_announce_ttl: u8,
}

impl<'a> OutboundPlanRequest<'a> {
    pub fn new(packet: &'a BitchatPacket, encoded_len: usize, links: &'a [LinkCandidate]) -> Self {
        Self {
            packet,
            encoded_len,
            links,
            ingress_link: None,
            excluded_links: &[],
            directed_only_peer: None,
            require_direct_peer_link: false,
            direct_announce_ttl: MESSAGE_TTL_DEFAULT,
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
    pub fn with_directed_only_peer(mut self, peer: Option<&'a PeerId>) -> Self {
        self.directed_only_peer = peer;
        self
    }

    #[must_use]
    pub fn with_require_direct_peer_link(mut self, require: bool) -> Self {
        self.require_direct_peer_link = require;
        self
    }

    #[must_use]
    pub fn with_direct_announce_ttl(mut self, ttl: u8) -> Self {
        self.direct_announce_ttl = ttl;
        self
    }
}

/// Plans one outbound packet.
pub fn plan(req: &OutboundPlanRequest<'_>) -> OutboundPlan {
    let packet = req.packet;
    let directed_peer = directed_peer_hint(packet, req.directed_only_peer);
    let message_id = message_id(packet);

    // A direct announce is the packet that *creates* a link↔peer binding, so it
    // must reach every live link including duplicates to the same peer. Collapse
    // it and a duplicate link never receives an announce, never becomes bound,
    // and is then treated as pre-announce by every later broadcast — which is
    // exactly the spray the collapse exists to prevent.
    let is_direct_announce =
        packet.msg_type == MessageType::Announce.as_u8() && packet.ttl == req.direct_announce_ttl;

    let selected = select_links(
        &FanoutRequest::new(req.links, packet.msg_type, &message_id)
            .with_ingress_link(req.ingress_link)
            .with_excluded_links(req.excluded_links)
            .with_collapse_duplicate_peer_links(!is_direct_announce)
            .with_directed_peer(directed_peer.as_ref())
            .with_require_direct_peer_link(req.require_direct_peer_link),
    );

    let min_limit = req
        .links
        .iter()
        .filter(|link| selected.contains(&link.id))
        .map(|link| link.max_write)
        .min();

    // Fragments are never re-fragmented: a fragment that does not fit is a
    // chunk-size bug upstream, and splitting it again would produce a stream no
    // peer can reassemble. `None` means nothing was selected, so there is no MTU
    // to measure against and no decision to make.
    let over_mtu = min_limit.filter(|limit| {
        packet.msg_type != MessageType::Fragment.as_u8() && req.encoded_len > *limit
    });
    if let Some(limit) = over_mtu {
        return OutboundPlan {
            directed_peer,
            fragment_chunk_size: Some(
                fragment_chunk_size(limit)
                    .saturating_sub(
                        packet
                            .route
                            .as_ref()
                            .filter(|route| !route.is_empty())
                            .map_or(0, |route| 1 + route.len() * SENDER_ID_LEN),
                    )
                    .max(MIN_FRAGMENT_CHUNK),
            ),
            selected,
            should_spool_directed: false,
        };
    }

    let should_spool_directed =
        should_spool_directed(directed_peer.as_ref(), &selected, packet.msg_type);

    OutboundPlan {
        directed_peer,
        fragment_chunk_size: None,
        selected,
        should_spool_directed,
    }
}

/// Who this packet is for.
///
/// An explicit override wins; otherwise the recipient field of the header.
///
/// **Deviation from Swift, deliberate.** bitchat builds the hint from
/// `recipientID.hexEncodedString()` with no broadcast check, so a packet carrying
/// the explicit all-`0xFF` recipient would be treated as directed to the peer
/// `"ffffffffffffffff"`, match no link, and then flood *without subsetting*.
/// bitchat never emits that form — its own broadcasts leave the field `nil` — but
/// it does accept it on the wire (`BLEPacketFreshnessPolicy.isBroadcastRecipient`
/// tests for exactly those bytes), so an older or third-party client could send
/// one. We treat it as what it is: a broadcast.
pub fn directed_peer_hint(packet: &BitchatPacket, explicit: Option<&PeerId>) -> Option<PeerId> {
    if let Some(peer) = explicit {
        return Some(peer.clone());
    }
    let recipient = packet.recipient_id?;
    if recipient == BROADCAST_RECIPIENT {
        return None;
    }
    // Swift also rejects an empty recipient here; ours is a fixed `[u8; 8]`, so
    // the hex form is always 16 characters and the check cannot fire.
    Some(PeerId::from_bytes(&recipient))
}

/// Whether a directed packet that selected no links is worth holding.
///
/// Only Noise traffic. A handshake or an encrypted message has a session behind it
/// and no upstream copy to retry from, so losing it strands the conversation.
/// Everything else either floods (and will arrive another way) or has a durable
/// sender-side copy.
pub fn should_spool_directed(
    directed_peer: Option<&PeerId>,
    selected: &FanoutSelection,
    packet_type: u8,
) -> bool {
    directed_peer.is_some()
        && selected.is_empty()
        && (packet_type == MessageType::NoiseEncrypted.as_u8()
            || packet_type == MessageType::NoiseHandshake.as_u8())
}

/// The key this packet is identified by — the fanout hash seed and the spool key.
///
/// Identical to `BLEIngressLinkRegistry.messageID(for:)`, which is already ported
/// as the deduplicator's key: `sender-timestamp-type-sha256(payload)[..4]`. Using
/// the same string is not incidental — it is what makes a packet select the same
/// links on a retry that it selected the first time.
pub fn message_id(packet: &BitchatPacket) -> String {
    MessageDeduplicator::dedup_key(packet)
}

/// Which packet types are padded to a block size before going on the wire.
///
/// `BLEOutboundPacketPolicy.padsBLEFrame(for:)`. Only Noise traffic, whose length
/// would otherwise leak message size. Voice frames are deliberately excluded: a
/// ~490-byte signed voice packet padded to the 512 block clears the MTU and drops
/// into the fragment path, turning one write into several.
///
/// Re-exported from [`chatter_mesh::fragment`] rather than reimplemented. Padding
/// is wire-visible, so two copies of the rule would eventually disagree about what
/// goes on the air — and the encode path and the send path disagreeing about
/// padding is exactly the kind of divergence that only shows up against a real
/// peer.
pub use chatter_mesh::fragment::pads_ble_frame;

/// The chunk size to fragment at, given the smallest MTU among the target links.
///
/// `BLEOutboundPacketPolicy.fragmentChunkSize(forLinkLimit:)`. Floored at
/// [`MIN_FRAGMENT_CHUNK`], which also makes an MTU smaller than the frame overhead
/// harmless instead of a zero-length chunk loop.
pub fn fragment_chunk_size(link_limit: usize) -> usize {
    MIN_FRAGMENT_CHUNK.max(link_limit.saturating_sub(FRAGMENT_FRAME_OVERHEAD))
}

/// Where a packet sits in the outbound write queue.
///
/// `BLEOutboundWritePriority`. Ordered by `(level, suborder)` ascending — lower is
/// sent first — which the derived `Ord` gives directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutboundPriority {
    pub level: u8,
    pub suborder: u32,
}

impl OutboundPriority {
    /// Everything interactive: messages, announces, handshakes, sync.
    pub const HIGH: Self = Self {
        level: 0,
        suborder: 0,
    };

    /// Bulk transfers, behind fragments so a file cannot stall a message stream.
    pub const FILE_TRANSFER: Self = Self {
        level: 2,
        suborder: u32::MAX - 1,
    };

    /// Packet types we do not recognise. Still relayed — just last.
    pub const LOW: Self = Self {
        level: 2,
        suborder: u32::MAX,
    };

    /// Fragments, ordered by how many pieces the whole message has.
    ///
    /// Shorter streams go first, which is shortest-job-first: a 3-fragment image
    /// finishes while a 400-fragment file is still going, instead of both
    /// finishing late.
    pub fn fragment(total_fragments: usize) -> Self {
        Self {
            level: 1,
            suborder: total_fragments.clamp(1, u16::MAX as usize) as u32,
        }
    }
}

/// The outbound queue priority for a packet.
///
/// `BLEOutboundPacketPolicy.priority(for:data:)`.
pub fn priority(packet: &BitchatPacket) -> OutboundPriority {
    match MessageType::from_u8(packet.msg_type) {
        None => OutboundPriority::LOW,
        Some(MessageType::Fragment) => {
            OutboundPriority::fragment(fragment_total_count(&packet.payload))
        }
        // Bulk payload, both of them: the single-packet file and one chunk of a
        // streamed one. `FileChunk` has to be named explicitly — before it was a
        // known type it fell to `LOW` as an unknown, and adding it to
        // `MessageType` without this arm would silently promote a 512-chunk
        // transfer to `HIGH` and let it starve conversation, which is precisely
        // what this level exists to prevent.
        Some(MessageType::FileTransfer | MessageType::FileChunk) => OutboundPriority::FILE_TRANSFER,
        // `FileManifest` and `FileChunkRequest` stay HIGH deliberately. Both are
        // a few hundred bytes of control plane, and both are on the critical path
        // of a transfer that is otherwise stalled: queueing a retransmit request
        // behind the chunks it is trying to repair would be self-defeating.
        Some(_) => OutboundPriority::HIGH,
    }
}

/// Reads the `total` field out of a fragment payload.
///
/// A payload too short to hold a header yields `u16::MAX`, i.e. the back of the
/// fragment queue — an unreadable fragment should not be able to jump the line.
fn fragment_total_count(payload: &[u8]) -> usize {
    // Swift checks `count >= 12` and reads bytes 10 and 11.
    let (Some(high), Some(low)) = (payload.get(10), payload.get(11)) else {
        return u16::MAX as usize;
    };
    let total = (usize::from(*high) << 8) | usize::from(*low);
    total.max(1)
}

/// One packet waiting for a link to its recipient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpooledPacket {
    pub recipient: PeerId,
    pub packet: BitchatPacket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredPacket {
    packet: BitchatPacket,
    enqueued_at_secs: u64,
}

/// Short-window store-and-forward for directed packets with nowhere to go.
///
/// `BLEDirectedRelaySpool`. When a directed Noise packet selects no links — the
/// recipient's link just dropped, or we are relaying toward someone we have not
/// met yet — holding it for a minute costs nothing and often turns a lost message
/// into a delivered one.
///
/// It is explicitly **not** delivery. `BLEService` returns "not accepted" for a
/// spooled packet precisely so a caller with a durable upstream copy keeps
/// retrying; treating the spool as success is how a message gets dropped while
/// everything reports fine.
///
/// Keyed by recipient then message ID, so a packet re-offered while it is still
/// spooled does not accumulate copies. Time is a parameter: nothing here reads a
/// clock.
#[derive(Clone, Debug, Default)]
pub struct DirectedRelaySpool {
    by_recipient: BTreeMap<PeerId, BTreeMap<String, StoredPacket>>,
}

impl DirectedRelaySpool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_recipient.values().all(|packets| packets.is_empty())
    }

    /// Total packets held, across all recipients.
    pub fn len(&self) -> usize {
        self.by_recipient
            .values()
            .map(|packets| packets.len())
            .sum()
    }

    /// Holds a packet. Returns `false` if this recipient already has one under
    /// the same message ID, in which case nothing changes — including the
    /// original's enqueue time, so a repeatedly retried packet still expires on
    /// schedule rather than living forever.
    pub fn enqueue(
        &mut self,
        packet: BitchatPacket,
        recipient: PeerId,
        message_id: String,
        enqueued_at_secs: u64,
    ) -> bool {
        let packets = self.by_recipient.entry(recipient).or_default();
        if packets.contains_key(&message_id) {
            return false;
        }
        packets.insert(
            message_id,
            StoredPacket {
                packet,
                enqueued_at_secs,
            },
        );
        true
    }

    /// Takes every unexpired packet out and empties the spool completely.
    ///
    /// Emptying rather than keeping the survivors is deliberate, and inherited:
    /// the caller re-broadcasts what it gets back, and anything that still has no
    /// link is spooled again with a *fresh* timestamp. Draining is therefore the
    /// only thing that resets the window; a packet nobody drains ages out.
    ///
    /// Ordered by recipient then message ID. Swift iterates a dictionary and gets
    /// whatever order it gets; ours is stable, which costs nothing and makes the
    /// flush reproducible.
    pub fn drain_unexpired(&mut self, now_secs: u64, window_secs: u64) -> Vec<SpooledPacket> {
        let drained = core::mem::take(&mut self.by_recipient);
        let mut entries = Vec::new();
        for (recipient, packets) in drained {
            for stored in packets.into_values() {
                if is_fresh(&stored, now_secs, window_secs) {
                    entries.push(SpooledPacket {
                        recipient: recipient.clone(),
                        packet: stored.packet,
                    });
                }
            }
        }
        entries
    }

    /// Drops expired packets, keeping the rest. For a maintenance tick that runs
    /// when there is still no link to flush toward.
    pub fn prune_expired(&mut self, now_secs: u64, window_secs: u64) {
        self.by_recipient.retain(|_, packets| {
            packets.retain(|_, stored| is_fresh(stored, now_secs, window_secs));
            !packets.is_empty()
        });
    }

    pub fn clear(&mut self) {
        self.by_recipient.clear();
    }
}

/// Age is inclusive of the window, matching Swift's `<=`.
///
/// A saturating subtraction also makes a backwards clock jump keep the packet,
/// which is the safe direction: the alternative silently drops live traffic.
fn is_fresh(stored: &StoredPacket, now_secs: u64, window_secs: u64) -> bool {
    now_secs.saturating_sub(stored.enqueued_at_secs) <= window_secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkRole;
    use chatter_mesh::packet::SENDER_ID_LEN;

    fn peer(name: &str) -> PeerId {
        PeerId::from_str(name)
    }

    fn central(id: &str, max_write: usize) -> LinkCandidate {
        LinkCandidate::new(LinkId::new(id), LinkRole::Central, max_write)
    }

    fn peripheral(id: &str, max_write: usize) -> LinkCandidate {
        LinkCandidate::new(LinkId::new(id), LinkRole::Peripheral, max_write)
    }

    fn packet(msg_type: MessageType) -> BitchatPacket {
        BitchatPacket::new(
            msg_type,
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
            None,
            1_700_000_000_000,
            b"payload".to_vec(),
            MESSAGE_TTL_DEFAULT,
        )
    }

    // Locals are never named `plan`: that would shadow the function under test in
    // the value namespace and make the next call in the same scope fail to
    // resolve.
    fn ids(out: &OutboundPlan) -> Vec<String> {
        out.selected
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect()
    }

    // ---- directed peer hint --------------------------------------------

    #[test]
    fn a_packet_with_no_recipient_is_a_broadcast() {
        assert_eq!(
            directed_peer_hint(&packet(MessageType::Message), None),
            None
        );
    }

    #[test]
    fn the_recipient_field_becomes_the_directed_hint() {
        let mut p = packet(MessageType::NoiseEncrypted);
        p.recipient_id = Some([0xAA; SENDER_ID_LEN]);
        assert_eq!(
            directed_peer_hint(&p, None),
            Some(PeerId::from_str("aaaaaaaaaaaaaaaa"))
        );
    }

    #[test]
    fn the_explicit_peer_overrides_the_header() {
        let mut p = packet(MessageType::NoiseEncrypted);
        p.recipient_id = Some([0xAA; SENDER_ID_LEN]);
        let bob = peer("bbbb");
        assert_eq!(directed_peer_hint(&p, Some(&bob)), Some(bob));
    }

    #[test]
    fn the_explicit_broadcast_recipient_is_not_a_directed_hint() {
        // The deviation documented on `directed_peer_hint`. Reading this as a
        // peer would make the packet skip subsetting and flood every link.
        let mut p = packet(MessageType::Message);
        p.recipient_id = Some(BROADCAST_RECIPIENT);
        assert_eq!(directed_peer_hint(&p, None), None);

        let links: Vec<LinkCandidate> = (0..8).map(|i| central(&format!("L{i}"), 512)).collect();
        let out = plan(&OutboundPlanRequest::new(&p, 100, &links));
        assert_eq!(out.directed_peer, None);
        assert_eq!(out.selected.len(), 4, "broadcast must still be subsetted");
    }

    // ---- plan: fanout wiring -------------------------------------------

    #[test]
    fn a_broadcast_is_subsetted_and_a_directed_packet_is_not() {
        let bob = peer("bbbb");
        let mut links: Vec<LinkCandidate> =
            (0..8).map(|i| central(&format!("L{i}"), 512)).collect();
        links.push(central("to-bob", 512).bound_to(bob.clone()));

        let broadcast = packet(MessageType::Message);
        let out = plan(&OutboundPlanRequest::new(&broadcast, 100, &links));
        assert_eq!(out.selected.len(), 5); // subset_size(9)

        let directed = packet(MessageType::NoiseEncrypted);
        let out = plan(
            &OutboundPlanRequest::new(&directed, 100, &links).with_directed_only_peer(Some(&bob)),
        );
        assert_eq!(ids(&out), vec!["to-bob"]);
    }

    #[test]
    fn the_ingress_link_is_excluded_from_the_plan() {
        let links: Vec<LinkCandidate> = (0..8).map(|i| central(&format!("L{i}"), 512)).collect();
        let p = packet(MessageType::Announce);
        let ingress = LinkId::new("L2");
        let out =
            plan(&OutboundPlanRequest::new(&p, 100, &links).with_ingress_link(Some(&ingress)));
        assert_eq!(out.selected.len(), 7);
        assert!(!out.selected.contains(&ingress));
    }

    #[test]
    fn a_direct_announce_reaches_both_links_of_a_dual_role_pair() {
        // The announce is what binds a link to its peer. Collapsing it leaves the
        // notify link permanently unbound.
        let bob = peer("bbbb");
        let links = vec![
            central("bob-write", 512).bound_to(bob.clone()),
            peripheral("bob-notify", 512).bound_to(bob.clone()),
        ];

        let mut direct = packet(MessageType::Announce);
        direct.ttl = MESSAGE_TTL_DEFAULT;
        let out = plan(&OutboundPlanRequest::new(&direct, 100, &links));
        assert_eq!(ids(&out), vec!["bob-notify", "bob-write"]);
    }

    #[test]
    fn a_relayed_announce_is_still_collapsed_per_peer() {
        let bob = peer("bbbb");
        let links = vec![
            central("bob-write", 512).bound_to(bob.clone()),
            peripheral("bob-notify", 512).bound_to(bob.clone()),
        ];

        let mut relayed = packet(MessageType::Announce);
        relayed.ttl = MESSAGE_TTL_DEFAULT - 1;
        let out = plan(&OutboundPlanRequest::new(&relayed, 100, &links));
        assert_eq!(ids(&out), vec!["bob-write"]);
    }

    #[test]
    fn require_direct_peer_link_is_forwarded_to_the_fanout() {
        let bob = peer("bbbb");
        let links: Vec<LinkCandidate> = (0..4).map(|i| central(&format!("L{i}"), 512)).collect();
        let p = packet(MessageType::NoiseEncrypted);

        let flooding =
            plan(&OutboundPlanRequest::new(&p, 100, &links).with_directed_only_peer(Some(&bob)));
        assert_eq!(flooding.selected.len(), 4);

        let strict = plan(
            &OutboundPlanRequest::new(&p, 100, &links)
                .with_directed_only_peer(Some(&bob))
                .with_require_direct_peer_link(true),
        );
        assert!(strict.selected.is_empty());
    }

    // ---- plan: fragmentation -------------------------------------------

    #[test]
    fn an_oversized_packet_asks_for_fragments() {
        let links = vec![central("L0", 244)];
        let p = packet(MessageType::Message);
        let out = plan(&OutboundPlanRequest::new(&p, 900, &links));
        assert_eq!(out.fragment_chunk_size, Some(244 - FRAGMENT_FRAME_OVERHEAD));
        assert!(!out.should_spool_directed);
    }

    #[test]
    fn a_packet_that_fits_is_not_fragmented() {
        let links = vec![central("L0", 244)];
        let p = packet(MessageType::Message);
        assert_eq!(
            plan(&OutboundPlanRequest::new(&p, 244, &links)).fragment_chunk_size,
            None,
            "exactly the limit still fits"
        );
        assert_eq!(
            plan(&OutboundPlanRequest::new(&p, 245, &links)).fragment_chunk_size,
            Some(244 - FRAGMENT_FRAME_OVERHEAD)
        );
    }

    #[test]
    fn fragments_are_never_re_fragmented() {
        let links = vec![central("L0", 100)];
        let mut p = packet(MessageType::Fragment);
        p.payload = vec![0u8; 40];
        assert_eq!(
            plan(&OutboundPlanRequest::new(&p, 900, &links)).fragment_chunk_size,
            None
        );
    }

    #[test]
    fn only_the_selected_links_constrain_the_chunk_size() {
        // The trap this guards: a directed send to Bob must not be sized by
        // Carol's tiny MTU. If it were, the resulting fragments would each select
        // only Bob's link and be far smaller than needed — or, with the roles
        // reversed, the send would look routable and then select nothing.
        let bob = peer("bbbb");
        let links = vec![
            central("to-bob", 512).bound_to(bob.clone()),
            central("to-carol", 80).bound_to(peer("cccc")),
        ];
        let p = packet(MessageType::NoiseEncrypted);

        let directed =
            plan(&OutboundPlanRequest::new(&p, 400, &links).with_directed_only_peer(Some(&bob)));
        assert_eq!(ids(&directed), vec!["to-bob"]);
        assert_eq!(directed.fragment_chunk_size, None);

        // Broadcast reaches both, so Carol's MTU does govern.
        let broadcast = plan(&OutboundPlanRequest::new(&p, 400, &links));
        assert_eq!(broadcast.selected.len(), 2);
        assert_eq!(broadcast.fragment_chunk_size, Some(MIN_FRAGMENT_CHUNK));
    }

    #[test]
    fn no_selected_links_means_no_fragmentation_decision() {
        let bob = peer("bbbb");
        let p = packet(MessageType::NoiseEncrypted);
        let out =
            plan(&OutboundPlanRequest::new(&p, 9_000, &[]).with_directed_only_peer(Some(&bob)));
        assert_eq!(out.fragment_chunk_size, None);
        assert!(out.selected.is_empty());
    }

    #[test]
    fn fragment_chunk_size_floors_at_the_minimum() {
        assert_eq!(fragment_chunk_size(0), MIN_FRAGMENT_CHUNK);
        assert_eq!(
            fragment_chunk_size(FRAGMENT_FRAME_OVERHEAD),
            MIN_FRAGMENT_CHUNK
        );
        assert_eq!(fragment_chunk_size(100), MIN_FRAGMENT_CHUNK);
        assert_eq!(fragment_chunk_size(512), 512 - FRAGMENT_FRAME_OVERHEAD);
        assert_eq!(FRAGMENT_FRAME_OVERHEAD, 42);
    }

    // ---- plan: spooling ------------------------------------------------

    #[test]
    fn a_directed_noise_packet_with_nowhere_to_go_is_spooled() {
        let bob = peer("bbbb");
        for msg_type in [MessageType::NoiseEncrypted, MessageType::NoiseHandshake] {
            let p = packet(msg_type);
            let out =
                plan(&OutboundPlanRequest::new(&p, 100, &[]).with_directed_only_peer(Some(&bob)));
            assert!(out.should_spool_directed, "{msg_type:?}");
        }
    }

    #[test]
    fn nothing_else_is_spooled() {
        let bob = peer("bbbb");

        // Not a Noise type.
        let p = packet(MessageType::Message);
        assert!(
            !plan(&OutboundPlanRequest::new(&p, 100, &[]).with_directed_only_peer(Some(&bob)))
                .should_spool_directed
        );

        // Not directed.
        let p = packet(MessageType::NoiseEncrypted);
        assert!(!plan(&OutboundPlanRequest::new(&p, 100, &[])).should_spool_directed);

        // Directed, Noise, but it did go somewhere.
        let links = vec![central("to-bob", 512).bound_to(bob.clone())];
        assert!(
            !plan(&OutboundPlanRequest::new(&p, 100, &links).with_directed_only_peer(Some(&bob)))
                .should_spool_directed
        );
    }

    #[test]
    fn an_oversized_directed_packet_fragments_rather_than_spooling() {
        // Fragmenting takes precedence: the fragments each get their own plan,
        // and spooling the un-fragmented original would hold a packet no link
        // could ever carry.
        let bob = peer("bbbb");
        let links = vec![central("to-bob", 100).bound_to(bob.clone())];
        let p = packet(MessageType::NoiseEncrypted);
        let out =
            plan(&OutboundPlanRequest::new(&p, 900, &links).with_directed_only_peer(Some(&bob)));
        assert!(out.fragment_chunk_size.is_some());
        assert!(!out.should_spool_directed);
    }

    // ---- packet policy -------------------------------------------------

    #[test]
    fn only_noise_frames_are_padded() {
        assert!(pads_ble_frame(MessageType::NoiseEncrypted.as_u8()));
        assert!(pads_ble_frame(MessageType::NoiseHandshake.as_u8()));
        for msg_type in [
            MessageType::Announce,
            MessageType::Message,
            MessageType::Leave,
            MessageType::CourierEnvelope,
            MessageType::Fragment,
            MessageType::RequestSync,
            MessageType::FileTransfer,
            MessageType::BoardPost,
            MessageType::PrekeyBundle,
            MessageType::GroupMessage,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::NostrCarrier,
            // The deliberate one: padding a ~490-byte voice packet to 512 pushes
            // it over the MTU and into the fragment path.
            MessageType::VoiceFrame,
        ] {
            assert!(!pads_ble_frame(msg_type.as_u8()), "{msg_type:?}");
        }
        assert!(!pads_ble_frame(0xEE), "unknown types are not padded");
    }

    #[test]
    fn priority_orders_interactive_before_fragments_before_bulk() {
        let mut p = packet(MessageType::Message);
        assert_eq!(priority(&p), OutboundPriority::HIGH);

        p.msg_type = MessageType::FileTransfer.as_u8();
        assert_eq!(priority(&p), OutboundPriority::FILE_TRANSFER);

        // A streamed chunk is bulk too. If this ever reads HIGH, a large
        // transfer outranks every message in the room for its whole duration.
        p.msg_type = MessageType::FileChunk.as_u8();
        assert_eq!(priority(&p), OutboundPriority::FILE_TRANSFER);

        // The control plane of the same transfer is not bulk: a manifest nobody
        // has yet, and a retransmit request for a stalled transfer, are both
        // small and both blocking.
        for control in [MessageType::FileManifest, MessageType::FileChunkRequest] {
            p.msg_type = control.as_u8();
            assert_eq!(priority(&p), OutboundPriority::HIGH, "{control:?}");
        }

        p.msg_type = 0x7F;
        assert_eq!(priority(&p), OutboundPriority::LOW);

        assert!(OutboundPriority::HIGH < OutboundPriority::fragment(1));
        assert!(OutboundPriority::fragment(9_000) < OutboundPriority::FILE_TRANSFER);
        assert!(OutboundPriority::FILE_TRANSFER < OutboundPriority::LOW);
    }

    #[test]
    fn shorter_fragment_streams_are_sent_first() {
        let mut short = packet(MessageType::Fragment);
        short.payload = {
            let mut header = vec![0u8; 13];
            header[10] = 0;
            header[11] = 3;
            header
        };
        let mut long = short.clone();
        long.payload[10] = 1;
        long.payload[11] = 0x90; // 400

        assert_eq!(priority(&short), OutboundPriority::fragment(3));
        assert_eq!(priority(&long), OutboundPriority::fragment(400));
        assert!(priority(&short) < priority(&long));
    }

    #[test]
    fn an_unreadable_fragment_goes_to_the_back_of_its_class() {
        let mut truncated = packet(MessageType::Fragment);
        truncated.payload = vec![0u8; 11];
        assert_eq!(
            priority(&truncated),
            OutboundPriority::fragment(u16::MAX as usize)
        );

        // A zero `total` is clamped up rather than treated as urgent.
        let mut zeroed = packet(MessageType::Fragment);
        zeroed.payload = vec![0u8; 13];
        assert_eq!(priority(&zeroed), OutboundPriority::fragment(1));
    }

    #[test]
    fn the_message_id_is_the_dedup_key() {
        let p = packet(MessageType::Message);
        assert_eq!(message_id(&p), MessageDeduplicator::dedup_key(&p));
        assert_eq!(message_id(&p), "0011223344556677-1700000000000-2-239f59ed");
    }

    #[test]
    fn the_message_id_seeds_the_fanout_so_relays_agree_with_themselves() {
        // A relayed copy differs in ttl and route, which the message ID ignores —
        // so it lands on the same links as the first copy did.
        let links: Vec<LinkCandidate> = (0..8).map(|i| central(&format!("L{i}"), 512)).collect();
        let original = packet(MessageType::Message);
        let relayed = BitchatPacket {
            ttl: 2,
            ..original.clone()
        };
        assert_eq!(
            plan(&OutboundPlanRequest::new(&original, 100, &links)).selected,
            plan(&OutboundPlanRequest::new(&relayed, 100, &links)).selected
        );
    }

    // ---- directed relay spool ------------------------------------------

    fn spool_packet(tag: u8) -> BitchatPacket {
        let mut p = packet(MessageType::NoiseEncrypted);
        p.payload = vec![tag; 8];
        p
    }

    #[test]
    fn spool_holds_and_drains_a_packet() {
        let mut spool = DirectedRelaySpool::new();
        assert!(spool.is_empty());

        let bob = peer("bbbb");
        let p = spool_packet(1);
        assert!(spool.enqueue(p.clone(), bob.clone(), message_id(&p), 1_000));
        assert_eq!(spool.len(), 1);
        assert!(!spool.is_empty());

        let drained = spool.drain_unexpired(1_010, DIRECTED_SPOOL_WINDOW_SECS);
        assert_eq!(
            drained,
            vec![SpooledPacket {
                recipient: bob,
                packet: p
            }]
        );
        assert!(spool.is_empty());
    }

    #[test]
    fn the_same_packet_is_never_spooled_twice() {
        let mut spool = DirectedRelaySpool::new();
        let bob = peer("bbbb");
        let p = spool_packet(1);
        let id = message_id(&p);

        assert!(spool.enqueue(p.clone(), bob.clone(), id.clone(), 1_000));
        assert!(!spool.enqueue(p.clone(), bob.clone(), id.clone(), 1_050));
        assert_eq!(spool.len(), 1);

        // The re-offer did not refresh the original's timestamp, so it still
        // expires on the first enqueue's schedule.
        assert!(spool.drain_unexpired(1_061, 60).is_empty());
    }

    #[test]
    fn the_same_message_to_two_recipients_is_held_separately() {
        let mut spool = DirectedRelaySpool::new();
        let p = spool_packet(1);
        let id = message_id(&p);
        assert!(spool.enqueue(p.clone(), peer("aaaa"), id.clone(), 0));
        assert!(spool.enqueue(p.clone(), peer("bbbb"), id, 0));
        assert_eq!(spool.len(), 2);
    }

    #[test]
    fn expired_packets_are_not_drained() {
        let mut spool = DirectedRelaySpool::new();
        let bob = peer("bbbb");
        let fresh = spool_packet(1);
        let stale = spool_packet(2);
        assert!(spool.enqueue(stale.clone(), bob.clone(), message_id(&stale), 0));
        assert!(spool.enqueue(fresh.clone(), bob.clone(), message_id(&fresh), 100));

        // Window is inclusive, matching Swift's `<=`.
        let drained = spool.drain_unexpired(160, 60);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].packet, fresh);
        // Draining empties the spool regardless of what it returned.
        assert!(spool.is_empty());
    }

    #[test]
    fn the_expiry_boundary_is_inclusive() {
        let bob = peer("bbbb");
        let p = spool_packet(1);

        let mut spool = DirectedRelaySpool::new();
        assert!(spool.enqueue(p.clone(), bob.clone(), message_id(&p), 0));
        assert_eq!(spool.drain_unexpired(60, 60).len(), 1);

        let mut spool = DirectedRelaySpool::new();
        assert!(spool.enqueue(p.clone(), bob.clone(), message_id(&p), 0));
        assert!(spool.drain_unexpired(61, 60).is_empty());
    }

    #[test]
    fn prune_drops_the_expired_and_keeps_the_rest() {
        let mut spool = DirectedRelaySpool::new();
        let bob = peer("bbbb");
        let stale = spool_packet(1);
        let fresh = spool_packet(2);
        assert!(spool.enqueue(stale.clone(), bob.clone(), message_id(&stale), 0));
        assert!(spool.enqueue(fresh.clone(), bob.clone(), message_id(&fresh), 100));
        assert!(spool.enqueue(stale.clone(), peer("aaaa"), message_id(&stale), 0));

        // At t=150 the stale pair (enqueued at 0) is 150s old and gone; the fresh
        // one is 50s old and stays. `aaaa` loses its only packet and the empty
        // recipient bucket goes with it.
        spool.prune_expired(150, 60);
        assert_eq!(spool.len(), 1);
        let drained = spool.drain_unexpired(150, 60);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].recipient, bob);
        assert_eq!(drained[0].packet, fresh);
    }

    #[test]
    fn prune_on_an_empty_spool_is_harmless() {
        let mut spool = DirectedRelaySpool::new();
        spool.prune_expired(1_000, 60);
        assert!(spool.is_empty());
        assert_eq!(spool.len(), 0);
        assert!(spool.drain_unexpired(1_000, 60).is_empty());
    }

    #[test]
    fn a_backwards_clock_keeps_packets_rather_than_dropping_them() {
        let mut spool = DirectedRelaySpool::new();
        let bob = peer("bbbb");
        let p = spool_packet(1);
        assert!(spool.enqueue(p.clone(), bob, message_id(&p), 5_000));
        spool.prune_expired(1_000, 60);
        assert_eq!(spool.len(), 1);
    }

    #[test]
    fn clear_empties_the_spool() {
        let mut spool = DirectedRelaySpool::new();
        let bob = peer("bbbb");
        let p = spool_packet(1);
        assert!(spool.enqueue(p.clone(), bob, message_id(&p), 0));
        spool.clear();
        assert!(spool.is_empty());
        assert_eq!(spool.len(), 0);
    }

    #[test]
    fn drain_order_is_stable() {
        let mut spool = DirectedRelaySpool::new();
        for (recipient, tag) in [("cccc", 3u8), ("aaaa", 1), ("bbbb", 2)] {
            let p = spool_packet(tag);
            assert!(spool.enqueue(p.clone(), peer(recipient), message_id(&p), 0));
        }
        let drained = spool.drain_unexpired(10, 60);
        let recipients: Vec<&str> = drained.iter().map(|e| e.recipient.as_str()).collect();
        assert_eq!(recipients, vec!["aaaa", "bbbb", "cccc"]);
    }
}
