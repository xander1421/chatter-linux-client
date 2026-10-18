use super::{MessageRouter, RouterEvent};
use crate::error::StoreError;
use crate::outbox::{AckKind, AckOutcome, QueuedMessage};
use chatter_mesh::ids::PeerId;

impl MessageRouter {
    /// A delivery or read ack confirms receipt; stop retaining the message.
    ///
    /// Both kinds clear it, and a read ack is never downgraded by a delivery ack
    /// arriving afterwards — see [`AckOutcome::strongest`].
    pub fn mark_acked(
        &mut self,
        from: &PeerId,
        message_id: &str,
        kind: AckKind,
    ) -> (AckOutcome, Vec<RouterEvent>) {
        let outcome = self.outbox.mark_acked(from, message_id, kind);
        let mut events = Vec::new();
        self.persist_into(&mut events);
        (outcome, events)
    }

    /// Panic wipe: forget queued mail in memory and on disk.
    pub fn wipe_outbox(&mut self) -> Result<(), StoreError> {
        self.outbox.wipe()
    }

    /// Read the durable outbox and immediately resume delivery.
    ///
    /// A cold restore can start before storage is readable, so this is safe and
    /// useful to call more than once: the store merges rather than replaces, and
    /// nothing accepted in the meantime is lost. bitchat does the same three things
    /// after a recovery — surface the overflow drops, flush everything, and retry
    /// the bridge deposits that the locked window skipped.
    pub fn recover_outbox(&mut self, now_unix_secs: u64) -> Vec<RouterEvent> {
        let mut events = Vec::new();
        match self.outbox.load() {
            Ok(dropped) => events.extend(dropped.into_iter().map(RouterEvent::Dropped)),
            Err(error) => {
                // Not readable yet. The store stays un-loaded and will not overwrite
                // the blob it could not read, so a later call still recovers it.
                events.push(RouterEvent::StorageFailed { error });
                return events;
            }
        }
        events.extend(self.flush_all_outbox(now_unix_secs));
        events.extend(self.retry_bridge_deposits(now_unix_secs));
        events
    }

    /// Re-attempt everything retained for one peer.
    ///
    /// The same four cases as [`send_private`](Self::send_private). This picks the
    /// [`LinkState`]; [`OutboxStore::flush`] applies its consequences — including
    /// the subtlest rule in the store, that an [`LinkState::Insecure`] flush does
    /// **not** count toward the attempt cap. The message went out over a live link,
    /// so a peer whose handshake stalls across reconnect flapping must not burn the
    /// cap and lose the very copy the retention exists to preserve.
    pub fn flush_outbox(&mut self, peer: &PeerId, now_unix_secs: u64) -> Vec<RouterEvent> {
        let (link, index) = self.route(peer);
        let plan = self.outbox.flush(peer, link, now_unix_secs);

        if let Some(index) = index {
            for message in &plan.send {
                self.transports[index].send_private(&message.content, peer, &message.message_id);
            }
        }

        let mut events: Vec<RouterEvent> =
            plan.dropped.into_iter().map(RouterEvent::Dropped).collect();
        self.persist_into(&mut events);
        events
    }

    pub fn flush_all_outbox(&mut self, now_unix_secs: u64) -> Vec<RouterEvent> {
        let mut events = Vec::new();
        for peer in self.outbox.peers() {
            events.extend(self.flush_outbox(&peer, now_unix_secs));
        }
        events
    }

    /// Expire stale retained mail across every peer.
    ///
    /// Without this, an in-session DM to a peer that never reconnects sits on
    /// "sending" until the next relaunch instead of surfacing as failed.
    pub fn cleanup_expired(&mut self, now_unix_secs: u64) -> Vec<RouterEvent> {
        let mut events: Vec<RouterEvent> = self
            .outbox
            .cleanup_expired(now_unix_secs)
            .into_iter()
            .map(RouterEvent::Dropped)
            .collect();
        self.persist_into(&mut events);
        events
    }

    // ------------------------------------------------------------------ helpers

    /// Retain a copy, surfacing whatever the per-peer FIFO cap evicted to make room.
    pub(super) fn retain(&mut self, peer: &PeerId, message: QueuedMessage) -> Vec<RouterEvent> {
        let evicted = self.outbox.enqueue(peer, message);
        let mut events: Vec<RouterEvent> = evicted.map(RouterEvent::Dropped).into_iter().collect();
        self.persist_into(&mut events);
        events
    }

    /// Write the outbox, turning a failure into an event instead of an error.
    ///
    /// A storage failure must never stop a send: the in-memory copy is intact and
    /// the store still owes the write, so the worst case is losing retention across
    /// a restart. Reporting it and carrying on is strictly better than refusing.
    pub(super) fn persist_into(&mut self, events: &mut Vec<RouterEvent>) {
        if let Err(error) = self.outbox.persist() {
            events.push(RouterEvent::StorageFailed { error });
        }
    }
}
