use chatter_mesh::packet::BitchatPacket;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// The per-type packet stores
// ---------------------------------------------------------------------------

/// Bounded insertion-ordered packet store, keyed by gossip-sync packet ID.
///
/// FIFO by *first* insertion: re-seeing a packet replaces the stored copy (relays
/// differ in ttl) without moving it up the queue, so a packet endlessly re-relayed
/// cannot pin itself in the store and starve newer traffic.
#[derive(Clone, Debug, Default)]
pub(super) struct PacketStore {
    pub(super) packets: BTreeMap<[u8; 16], BitchatPacket>,
    order: Vec<[u8; 16]>,
}

impl PacketStore {
    pub(super) fn insert(&mut self, id: [u8; 16], packet: BitchatPacket, capacity: usize) {
        if capacity == 0 {
            return;
        }
        if let std::collections::btree_map::Entry::Occupied(mut e) = self.packets.entry(id) {
            e.insert(packet);
            return;
        }
        self.packets.insert(id, packet);
        self.order.push(id);
        while self.order.len() > capacity {
            if self.order.is_empty() {
                break;
            }
            let victim = self.order.remove(0);
            self.packets.remove(&victim);
        }
    }

    pub(super) fn all_packets(
        &self,
        is_fresh: impl Fn(&BitchatPacket) -> bool,
    ) -> Vec<&BitchatPacket> {
        self.order
            .iter()
            .filter_map(|id| self.packets.get(id))
            .filter(|packet| is_fresh(packet))
            .collect()
    }

    pub(super) fn remove_where(&mut self, should_remove: impl Fn(&BitchatPacket) -> bool) {
        let previous = core::mem::take(&mut self.order);
        let mut next = Vec::with_capacity(previous.len());
        for id in previous {
            let Some(packet) = self.packets.get(&id) else {
                // Key with no packet: drop it from the order, as Swift does.
                continue;
            };
            if should_remove(packet) {
                self.packets.remove(&id);
            } else {
                next.push(id);
            }
        }
        self.order = next;
    }

    pub(super) fn remove_expired(&mut self, is_fresh: impl Fn(&BitchatPacket) -> bool) {
        self.remove_where(|packet| !is_fresh(packet));
    }

    pub(super) fn len(&self) -> usize {
        self.packets.len()
    }
}
