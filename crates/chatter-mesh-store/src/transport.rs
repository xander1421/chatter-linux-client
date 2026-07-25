//! The narrow slice of a transport the router actually needs.
//!
//! bitchat's `Transport` protocol has some sixty members; the router touches a
//! handful. Porting the whole thing here would couple store-and-forward to file
//! transfer, voice, verification, and group state for no benefit, and would make
//! this crate untestable without standing all of that up.

use chatter_mesh::ids::PeerId;

/// What we know about a peer right now, from one transport's point of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerSnapshot {
    pub peer: PeerId,
    pub is_connected: bool,
    /// Their Noise static key, when we know it. Required to seal courier mail —
    /// a peer whose key we do not have cannot be addressed offline at all.
    pub noise_public_key: Option<[u8; 32]>,
    /// Whether their last announce was signature-verified. This is the courier
    /// trust-tier gate: unverified peers may not carry mail.
    pub is_verified: bool,
}

/// A transport, as the router sees it.
///
/// # The four predicates are not redundant
///
/// They look similar and mean genuinely different things. The router's entire
/// decision tree is built on the differences, so collapsing any two of them changes
/// delivery behaviour rather than simplifying code:
///
/// - [`is_peer_connected`](RouterTransport::is_peer_connected) — a live link binding
///   exists. **Forgeable**: link bindings heal on signature-verified "direct"
///   announces, but directness rides on the unsigned TTL, so a replayed announce can
///   bind an absent peer's ID to the replayer's link.
/// - [`is_peer_reachable`](RouterTransport::is_peer_reachable) — a freshness
///   heuristic. The mesh keeps a peer "reachable" for up to a minute after its radio
///   goes quiet, so a send here can silently go nowhere.
/// - [`can_deliver_promptly`](RouterTransport::can_deliver_promptly) — the send will
///   actually leave the device. Nostr claims any favourite with a known npub as
///   reachable even with no relay connection, where a send only joins a queue
///   waiting for internet that may never arrive.
/// - [`can_deliver_securely`](RouterTransport::can_deliver_securely) — an encrypted
///   delivery can complete *now*, e.g. an established Noise session. This is the only
///   one strong enough to send against without also retaining a copy.
pub trait RouterTransport {
    fn is_peer_connected(&self, peer: &PeerId) -> bool;
    fn is_peer_reachable(&self, peer: &PeerId) -> bool;
    fn can_deliver_promptly(&self, peer: &PeerId) -> bool;
    fn can_deliver_securely(&self, peer: &PeerId) -> bool;

    /// Everyone this transport currently knows about, for courier selection.
    fn peer_snapshots(&self) -> Vec<PeerSnapshot>;

    /// Returns whether the transport accepted the message for delivery. Acceptance
    /// is not delivery — only an ack clears the outbox.
    fn send_private(&self, content: &str, to: &PeerId, message_id: &str) -> bool;

    /// Hand a sealed copy to peers who may physically encounter the recipient.
    ///
    /// Returns **which couriers actually accepted**, which may be a subset of `via`
    /// or empty. bitchat returns a single bool for the whole batch and then records
    /// every candidate as carrying the message; a transport that accepts for one of
    /// three couriers therefore burns three of the message's courier slots, and the
    /// mail rides with fewer carriers than the router believes while no further
    /// deposits are attempted. Returning the accepted subset costs nothing — this is
    /// internal API with no wire representation — and keeps the slot accounting
    /// honest.
    ///
    /// An empty result means this transport cannot courier at all, or had no
    /// eligible courier connected.
    fn send_courier(
        &self,
        content: &str,
        message_id: &str,
        recipient_key: &[u8; 32],
        via: &[PeerId],
    ) -> Vec<PeerId>;
}

/// Trust tier for a peer offering to carry mail.
///
/// The tiers exist so that strangers cannot crowd out favourites: they draw from
/// separate quotas against a shared pool, and overflow evicts the lower tier first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CourierTier {
    /// Signature-verified, but not a mutual favourite. Smaller quota, evicted first.
    Verified,
    /// Mutual favourite. Larger quota, never evicted to make room for `Verified`.
    Favorite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorites_outrank_verified_for_eviction() {
        // Ord is what the eviction sort relies on: the lowest tier goes first.
        assert!(CourierTier::Verified < CourierTier::Favorite);
        let mut tiers = [CourierTier::Favorite, CourierTier::Verified];
        tiers.sort();
        assert_eq!(tiers[0], CourierTier::Verified);
    }

    #[test]
    fn a_snapshot_without_a_noise_key_cannot_be_addressed_offline() {
        // Sealing needs the static key. This is why the field is Option: a peer we
        // have seen but never learned a key for is reachable online and invisible
        // to the courier path.
        let snapshot = PeerSnapshot {
            peer: PeerId::from_str("abcdef0123456789"),
            is_connected: true,
            noise_public_key: None,
            is_verified: true,
        };
        assert!(snapshot.noise_public_key.is_none());
    }
}
