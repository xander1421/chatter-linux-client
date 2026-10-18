use super::{LinkState, MessageRouter};
use chatter_mesh::ids::PeerId;

impl MessageRouter {
    /// The first transport reporting a live link binding.
    ///
    /// bitchat picks the first and stays with it. Falling through to a *different*
    /// transport's secure session after the first one's link turned out to have no
    /// session would be a different decision than the tree describes.
    fn first_connected(&self, peer: &PeerId) -> Option<usize> {
        self.transports
            .iter()
            .position(|t| t.is_peer_connected(peer))
    }

    pub(super) fn first_reachable(&self, peer: &PeerId) -> Option<usize> {
        self.transports
            .iter()
            .position(|t| t.is_peer_reachable(peer))
    }

    /// How good the route to this peer is, and which transport carries it.
    ///
    /// The index is `None` exactly when the state is [`LinkState::Unreachable`] —
    /// there is nothing to transmit over.
    pub(super) fn route(&self, peer: &PeerId) -> (LinkState, Option<usize>) {
        if let Some(index) = self.first_connected(peer) {
            let link = if self.transports[index].can_deliver_securely(peer) {
                LinkState::Secure
            } else {
                LinkState::Insecure
            };
            return (link, Some(index));
        }
        match self.first_reachable(peer) {
            Some(index) => (LinkState::Reachable, Some(index)),
            None => (LinkState::Unreachable, None),
        }
    }
}
