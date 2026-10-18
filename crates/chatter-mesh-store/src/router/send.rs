use super::{LinkState, MessageRouter, SendOutcome};
use crate::outbox::QueuedMessage;
use crate::transport::RouterTransport;
use chatter_mesh::ids::PeerId;

impl MessageRouter {
    /// The decision tree.
    pub fn send_private(
        &mut self,
        content: &str,
        to: &PeerId,
        recipient_nickname: &str,
        message_id: &str,
        now_unix_secs: u64,
    ) -> SendOutcome {
        let (link, index) = self.route(to);

        if let Some(index) = index {
            self.transports[index].send_private(content, to, message_id);
        }

        // Anything transmitted counts as one attempt. Branch 4 transmitted nothing,
        // so it must not be charged — otherwise a message that never left the device
        // burns through the cap while sitting still.
        let attempts = u32::from(link != LinkState::Unreachable);
        let mut events = self.retain(
            to,
            QueuedMessage::new(content, recipient_nickname, message_id, now_unix_secs)
                .with_send_attempts(attempts),
        );

        let deposit_with_couriers = match link {
            // Branch 2. "Connected" without an established secure session is
            // forgeable: link bindings heal on signature-verified "direct"
            // announces, but directness rides on the unsigned TTL, so a replayed
            // announce can bind an absent peer's ID to the replayer's link — where
            // the send stalls on a handshake the replayer can never complete. Send
            // now (a genuine link finishes the handshake and delivers), but retain
            // a copy and hand a sealed copy to couriers so nothing is silently
            // lost; receivers dedup resends by message ID.
            //
            // Deliberate metadata tradeoff, accepted with eyes open: every
            // pre-handshake first DM to a connected peer hands nearby verified peers
            // a sealed copy, so they learn a DM to this recipient exists — never its
            // content, the envelope is opaque. It buys delivery robustness against
            // exactly the forgery above, and the deposit is cleared on ack. Do not
            // "optimise" this courier call away.
            LinkState::Insecure => true,
            // Branch 3. Reachability without a connection is a freshness heuristic
            // — the mesh keeps a peer reachable for up to a minute after its radio
            // goes quiet — so the send can silently go nowhere. Retaining covers
            // that. But "reachable" and "not prompt" together mean the send merely
            // joined a queue (Nostr with every relay down), so also hand a sealed
            // copy to connected couriers rather than waiting on internet that may
            // never come. Double delivery is harmless: receivers dedup by message
            // ID, and delivered/read acks never downgrade each other.
            LinkState::Reachable => {
                index.is_some_and(|i| !self.transports[i].can_deliver_promptly(to))
            }
            // Branch 4. Nothing can carry it; couriers are the only hope.
            LinkState::Unreachable => true,
            // A queued BLE write can still fail after routing; keep it until ACK.
            LinkState::Secure => false,
        };

        if deposit_with_couriers {
            events.extend(self.attempt_courier_deposit(message_id, to));
        }

        SendOutcome { link, events }
    }

    /// Returns whether a reachable transport accepted the read receipt.
    ///
    /// A false result means the receipt was dropped for want of a route and **must
    /// not be recorded as sent**, or the sender's message stays unread forever. It
    /// is retried on the next read scan — chat open, foreground, or reconnect.
    ///
    /// The wire call is a closure because [`RouterTransport`] deliberately carries
    /// no receipt method: the router owns the *choice* of transport, not the
    /// receipt's encoding.
    pub fn send_read_receipt<F>(&self, to: &PeerId, deliver: F) -> bool
    where
        F: FnOnce(&dyn RouterTransport) -> bool,
    {
        match self.first_reachable(to) {
            Some(index) => deliver(&*self.transports[index]),
            None => false,
        }
    }
}
