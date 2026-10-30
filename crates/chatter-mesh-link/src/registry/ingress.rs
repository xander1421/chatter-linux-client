use std::collections::BTreeMap;

use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::BitchatPacket;

use crate::link::LinkId;

/// `TransportConfig.bleIngressRecordLifetimeSeconds` (3 s).
///
/// Ingress records exist to suppress the echo of a packet back down the link it
/// arrived on, which happens within milliseconds of receiving it. They are
/// deliberately short-lived: this is not a routing table, and a long-lived
/// link↔packet memory would be another surface for a replay to steer.
pub const INGRESS_RECORD_LIFETIME_MS: u64 = 3_000;

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
