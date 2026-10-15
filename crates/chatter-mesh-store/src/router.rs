//! Transport selection: how a private message actually reaches someone.
//!
//! Ported from bitchat's `Services/MessageRouter.swift`.
//!
//! # The decision tree
//!
//! Four outcomes, tried in order. The differences between them are the whole point
//! of the module, and they fall out of the four reachability predicates on
//! [`RouterTransport`], which mean genuinely different things:
//!
//! 1. **Connected *and* [`can_deliver_securely`] — send, done.** A live link that
//!    can complete an encrypted delivery is a strong enough signal to trust
//!    outright, so nothing is retained.
//! 2. **Connected, no secure session — send, retain, *and* deposit with couriers.**
//!    "Connected" without an established session is forgeable, so the send may be
//!    stalling on a handshake the peer at the other end can never complete.
//! 3. **Reachable but not connected — send and retain.** Reachability is a freshness
//!    heuristic, so the send can silently go nowhere. If the transport also cannot
//!    [`can_deliver_promptly`], deposit with couriers too: the send only joined a
//!    queue waiting on internet that may never arrive.
//! 4. **Nothing — retain with zero attempts and deposit with couriers.**
//!
//! The same four cases drive [`flush_outbox`](MessageRouter::flush_outbox), where
//! they are expressed as [`LinkState`] and the per-message consequences belong to
//! [`OutboxStore::flush`]. This module owns the *choice*; the outbox owns what the
//! choice does to retained mail.
//!
//! [`can_deliver_securely`]: RouterTransport::can_deliver_securely
//! [`can_deliver_promptly`]: RouterTransport::can_deliver_promptly

use std::collections::BTreeSet;

use chatter_mesh::ids::PeerId;

use crate::error::StoreError;
use crate::outbox::{AckKind, AckOutcome, DroppedMessage, LinkState, OutboxStore, QueuedMessage};
use crate::transport::{PeerSnapshot, RouterTransport};

/// Identity lookups the router needs to address couriered mail.
///
/// Injected rather than reached for: peer keys live in the app's mesh state, and
/// this crate is sans-IO. Whether a peer may carry mail is read off the transport
/// snapshot (`is_verified`), not from here.
pub trait CourierDirectory {
    /// The recipient's Noise static key, when we know one.
    ///
    /// A peer whose key we do not have cannot be addressed offline at all — nothing
    /// can be sealed to them — so `None` disables the whole courier path for them.
    fn noise_key(&self, peer: &PeerId) -> Option<[u8; 32]>;
}

/// Parks a sealed copy on relays as a courier drop, so delivery stops requiring a
/// physical courier encounter.
///
/// Injected because `bridge_drop.rs` owns the relay side. Called with the message
/// content, its ID, and the recipient's static key. Must return true only once a
/// relay has *accepted* the event: a socket write followed by a rejection would
/// otherwise show the sender's message as carried when nothing holds it.
///
/// Runs alongside mesh couriers rather than instead of them; receivers dedup by
/// message ID.
pub type BridgeDeposit = Box<dyn Fn(&str, &str, &[u8; 32]) -> bool>;

/// Something the caller has to react to.
///
/// Returned rather than invoked as a callback: the router is sans-IO, and a
/// `&mut self` method that also calls back into the app is how re-entrancy bugs
/// start. bitchat's `onMessageDropped` / `onMessageCarried` map onto these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterEvent {
    /// Retained mail left the outbox without a delivery ack. The UI must surface a
    /// failure rather than leaving the message on "sending" forever — see
    /// [`DroppedMessage::reason`] for which bound was hit.
    Dropped(DroppedMessage),

    /// The message was handed to at least one courier, or parked as a bridge drop.
    /// Delivery stays best-effort: the retained copy is only cleared by an ack.
    Carried { message_id: String, peer: PeerId },

    /// Retained mail could not be read from or written to durable storage. Never
    /// fatal: losing the durable copy is bad, refusing to send is worse. The
    /// in-memory copy survives the session, and [`OutboxStore`] still owes the
    /// write.
    StorageFailed { error: StoreError },
}

/// The result of a send: which branch of the tree was taken, and what the caller
/// must react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendOutcome {
    /// The route the message took. [`LinkState::Secure`] is the only one that does
    /// not retain a copy.
    pub link: LinkState,
    pub events: Vec<RouterEvent>,
}

/// A peer who can be asked to carry a sealed envelope.
struct CourierCandidate {
    peer: PeerId,
    noise_key: [u8; 32],
}

/// Routes private messages across the available transports.
pub struct MessageRouter {
    transports: Vec<Box<dyn RouterTransport>>,
    directory: Box<dyn CourierDirectory>,
    outbox: OutboxStore,
    bridge_deposit: Option<BridgeDeposit>,
}

impl MessageRouter {
    pub fn new(
        transports: Vec<Box<dyn RouterTransport>>,
        directory: Box<dyn CourierDirectory>,
        outbox: OutboxStore,
    ) -> Self {
        Self {
            transports,
            directory,
            outbox,
            bridge_deposit: None,
        }
    }

    /// Wire the relay-side courier drop. Without it the router still couriers over
    /// the mesh; the bridge simply never fires.
    pub fn set_bridge_deposit(&mut self, deposit: BridgeDeposit) {
        self.bridge_deposit = Some(deposit);
    }

    pub fn outbox(&self) -> &OutboxStore {
        &self.outbox
    }

    pub fn outbox_mut(&mut self) -> &mut OutboxStore {
        &mut self.outbox
    }

    // ----------------------------------------------------------------- selection

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

    fn first_reachable(&self, peer: &PeerId) -> Option<usize> {
        self.transports
            .iter()
            .position(|t| t.is_peer_reachable(peer))
    }

    /// How good the route to this peer is, and which transport carries it.
    ///
    /// The index is `None` exactly when the state is [`LinkState::Unreachable`] —
    /// there is nothing to transmit over.
    fn route(&self, peer: &PeerId) -> (LinkState, Option<usize>) {
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

    // ------------------------------------------------------------------ sending

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

    // ----------------------------------------------------------------- couriers

    /// Seal the message to the recipient's known static key and hand it to peers who
    /// may physically encounter them.
    ///
    /// Only signature-verified peers carry mail; the envelopes are opaque to them.
    /// The retained copy stays retained, so a direct delivery still wins if the
    /// peer reappears first.
    fn attempt_courier_deposit(&mut self, message_id: &str, peer: &PeerId) -> Vec<RouterEvent> {
        let Some(recipient_key) = self.directory.noise_key(peer) else {
            return Vec::new();
        };
        let Some(entry) = self.outbox.queued(peer, message_id).cloned() else {
            return Vec::new();
        };

        // The bridge drop needs no connected courier — only the recipient key — so
        // it runs before any courier-slot bookkeeping, and independently of it.
        let mut events = Vec::new();
        events.extend(self.request_bridge_deposit(&entry, peer, &recipient_key));

        let remaining = entry.remaining_courier_slots();
        if remaining == 0 {
            return events;
        }

        for index in 0..self.transports.len() {
            let couriers = self.eligible_couriers(
                index,
                &recipient_key,
                &entry.deposited_courier_keys,
                remaining,
            );
            if couriers.is_empty() {
                continue;
            }
            let via: Vec<PeerId> = couriers.iter().map(|c| c.peer.clone()).collect();
            let accepted = self.transports[index].send_courier(
                &entry.content,
                message_id,
                &recipient_key,
                &via,
            );
            // Credit only the couriers that actually took it. Crediting every
            // candidate would burn the message's remaining slots against carriers
            // that never received it, and no further deposit would be attempted —
            // the mail then rides with fewer couriers than the outbox believes.
            let keys: Vec<[u8; 32]> = couriers
                .iter()
                .filter(|c| accepted.contains(&c.peer))
                .map(|c| c.noise_key)
                .collect();
            if keys.is_empty() {
                continue;
            }
            self.outbox.record_courier_deposit(peer, message_id, &keys);
            self.persist_into(&mut events);
            events.push(RouterEvent::Carried {
                message_id: message_id.to_string(),
                peer: peer.clone(),
            });
            return events;
        }
        events
    }

    /// A courier candidate just connected: hand it any queued mail it is not already
    /// carrying.
    ///
    /// This is what turns couriering from "a courier happened to be nearby at send
    /// time" into eventual spread — deposits retry as eligible peers appear, until
    /// each message rides with its full complement of distinct couriers or expires.
    pub fn courier_became_available(
        &mut self,
        peer: &PeerId,
        now_unix_secs: u64,
    ) -> Vec<RouterEvent> {
        let mut events = Vec::new();

        for index in 0..self.transports.len() {
            if !self.transports[index].is_peer_connected(peer) {
                continue;
            }
            // Only a signature-verified peer may carry mail, and only one whose
            // static key we know — the envelope has to be addressed.
            let Some(courier_key) = self.transports[index]
                .peer_snapshots()
                .into_iter()
                .find(|s| &s.peer == peer && s.is_connected)
                .and_then(|s| {
                    let key = s.noise_public_key?;
                    s.is_verified.then_some(key)
                })
            else {
                continue;
            };

            // Snapshot the queues first: recording a deposit mutates the outbox.
            let queues: Vec<(PeerId, Vec<QueuedMessage>)> = self
                .outbox
                .iter()
                .map(|(recipient, queue)| (recipient.clone(), queue.to_vec()))
                .collect();

            for (recipient, queue) in queues {
                // Mail *to* this peer flushes directly on connect. Handing someone
                // their own mail back as an opaque envelope achieves nothing.
                if &recipient == peer {
                    continue;
                }
                let Some(recipient_key) = self.directory.noise_key(&recipient) else {
                    continue;
                };
                if recipient_key == courier_key {
                    continue;
                }

                for message in queue {
                    if message.remaining_courier_slots() == 0
                        || message.is_carried_by(&courier_key)
                        || message.is_expired(now_unix_secs)
                    {
                        continue;
                    }
                    let accepted = self.transports[index].send_courier(
                        &message.content,
                        &message.message_id,
                        &recipient_key,
                        std::slice::from_ref(peer),
                    );
                    // Single-courier deposit, so acceptance is membership rather
                    // than a subset filter.
                    if !accepted.contains(peer) {
                        continue;
                    }
                    self.outbox.record_courier_deposit(
                        &recipient,
                        &message.message_id,
                        &[courier_key],
                    );
                    events.push(RouterEvent::Carried {
                        message_id: message.message_id.clone(),
                        peer: recipient.clone(),
                    });
                }
            }

            self.persist_into(&mut events);
            // bitchat stops at the first transport this peer is an eligible courier
            // on: a second would only hand the same peer a duplicate.
            return events;
        }
        events
    }

    /// Verified candidates on one transport, in snapshot order, capped at `limit`.
    fn eligible_couriers(
        &self,
        transport_index: usize,
        recipient_key: &[u8; 32],
        excluded: &BTreeSet<[u8; 32]>,
        limit: usize,
    ) -> Vec<CourierCandidate> {
        if limit == 0 {
            return Vec::new();
        }
        self.transports[transport_index]
            .peer_snapshots()
            .into_iter()
            .filter_map(|snapshot: PeerSnapshot| {
                if !snapshot.is_connected {
                    return None;
                }
                // No static key means nothing can be sealed to them, so they cannot
                // be handed an envelope either.
                let key = snapshot.noise_public_key?;
                // Never hand the recipient their own mail, and never re-burn a
                // courier already carrying this message.
                if &key == recipient_key || excluded.contains(&key) {
                    return None;
                }
                // Unverified strangers may not carry mail at all.
                if !snapshot.is_verified {
                    return None;
                }
                Some(CourierCandidate {
                    peer: snapshot.peer,
                    noise_key: key,
                })
            })
            .take(limit)
            .collect()
    }

    fn request_bridge_deposit(
        &self,
        message: &QueuedMessage,
        peer: &PeerId,
        recipient_key: &[u8; 32],
    ) -> Option<RouterEvent> {
        let deposit = self.bridge_deposit.as_ref()?;
        // The drop layer dedups by message ID, so re-depositing is cheap and this
        // does not have to track in-flight IDs itself.
        deposit(
            message.content.as_str(),
            message.message_id.as_str(),
            recipient_key,
        )
        .then(|| RouterEvent::Carried {
            message_id: message.message_id.clone(),
            peer: peer.clone(),
        })
    }

    /// Re-attempts bridge drops for retained messages that no transport can promptly
    /// reach.
    ///
    /// This covers sends that raced the reachability retention window: a peer stays
    /// "reachable" for a minute after its radio disappears, so the original send
    /// trusted the mesh and skipped the deposit — and nothing else ever retried.
    /// bitchat found this in the field. Safe to call often; the drop layer dedups.
    pub fn retry_bridge_deposits(&mut self, now_unix_secs: u64) -> Vec<RouterEvent> {
        if self.bridge_deposit.is_none() {
            return Vec::new();
        }
        let mut events = Vec::new();
        for peer in self.outbox.peers() {
            let Some(recipient_key) = self.directory.noise_key(&peer) else {
                continue;
            };
            // Note `is_peer_reachable && can_deliver_promptly` on the *same*
            // transport: a peer that one transport claims is reachable and another
            // could deliver promptly to is not actually promptly deliverable.
            let promptly = self
                .transports
                .iter()
                .any(|t| t.is_peer_reachable(&peer) && t.can_deliver_promptly(&peer));
            if promptly {
                continue;
            }
            for message in self.outbox.queue(&peer).to_vec() {
                if message.is_expired(now_unix_secs) {
                    continue;
                }
                events.extend(self.request_bridge_deposit(&message, &peer, &recipient_key));
            }
        }
        events
    }

    // ------------------------------------------------------------------- outbox

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
    fn retain(&mut self, peer: &PeerId, message: QueuedMessage) -> Vec<RouterEvent> {
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
    fn persist_into(&mut self, events: &mut Vec<RouterEvent>) {
        if let Err(error) = self.outbox.persist() {
            events.push(RouterEvent::StorageFailed { error });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::{DropReason, Sealer};
    use crate::storage::MemoryStorage;
    use crate::{MAX_COURIERS_PER_MESSAGE, MAX_SEND_ATTEMPTS, MESSAGE_TTL_SECS};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    // ---------------------------------------------------------------- fixtures

    fn peer(n: u8) -> PeerId {
        PeerId::from_str(&format!("{n:016x}"))
    }

    fn key(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn snapshot(p: &PeerId, k: Option<[u8; 32]>, verified: bool) -> PeerSnapshot {
        PeerSnapshot {
            peer: p.clone(),
            is_connected: true,
            noise_public_key: k,
            is_verified: verified,
        }
    }

    /// The router never sees the sealing key, so tests do not need a real one.
    struct PlainSealer;

    impl Sealer for PlainSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(plaintext.to_vec())
        }
        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(sealed.to_vec())
        }
    }

    #[derive(Default)]
    struct MockState {
        connected: Vec<PeerId>,
        reachable: Vec<PeerId>,
        promptly: Vec<PeerId>,
        securely: Vec<PeerId>,
        snapshots: Vec<PeerSnapshot>,
        courier_accepts: bool,
        private_sends: Vec<(String, PeerId, String)>,
        courier_sends: Vec<(String, Vec<PeerId>)>,
    }

    /// Shared-state mock, so a test can hand the router a transport and keep
    /// inspecting the same one afterwards.
    #[derive(Clone, Default)]
    struct MockTransport(Rc<RefCell<MockState>>);

    impl MockTransport {
        fn new() -> Self {
            Self::default()
        }

        /// Branch 1: a live link with an established secure session.
        fn connected_secure(self, p: &PeerId) -> Self {
            {
                let mut s = self.0.borrow_mut();
                s.connected.push(p.clone());
                s.reachable.push(p.clone());
                s.promptly.push(p.clone());
                s.securely.push(p.clone());
            }
            self
        }

        /// Branch 2: connected, no secure session.
        fn connected_insecure(self, p: &PeerId) -> Self {
            {
                let mut s = self.0.borrow_mut();
                s.connected.push(p.clone());
                s.reachable.push(p.clone());
                s.promptly.push(p.clone());
            }
            self
        }

        /// Branch 3, prompt variant: the send really leaves the device.
        fn reachable_prompt(self, p: &PeerId) -> Self {
            {
                let mut s = self.0.borrow_mut();
                s.reachable.push(p.clone());
                s.promptly.push(p.clone());
            }
            self
        }

        /// Branch 3, queue variant: the send only joins a queue waiting on internet.
        fn reachable_slow(self, p: &PeerId) -> Self {
            self.0.borrow_mut().reachable.push(p.clone());
            self
        }

        fn with_snapshots(self, snapshots: Vec<PeerSnapshot>) -> Self {
            self.0.borrow_mut().snapshots = snapshots;
            self
        }

        fn accepting_couriers(self) -> Self {
            self.0.borrow_mut().courier_accepts = true;
            self
        }

        /// A courier walks into range after the send already happened.
        fn courier_arrives(&self, p: &PeerId, k: [u8; 32], verified: bool) {
            let mut s = self.0.borrow_mut();
            s.connected.push(p.clone());
            s.snapshots.push(snapshot(p, Some(k), verified));
        }

        fn becomes_connected(&self, p: &PeerId, securely: bool) {
            let mut s = self.0.borrow_mut();
            s.connected.push(p.clone());
            if securely {
                s.securely.push(p.clone());
            }
        }

        fn becomes_reachable(&self, p: &PeerId, promptly: bool) {
            let mut s = self.0.borrow_mut();
            s.reachable.push(p.clone());
            if promptly {
                s.promptly.push(p.clone());
            }
        }

        fn private_sends(&self) -> Vec<(String, PeerId, String)> {
            self.0.borrow().private_sends.clone()
        }

        fn courier_sends(&self) -> Vec<(String, Vec<PeerId>)> {
            self.0.borrow().courier_sends.clone()
        }

        fn boxed(&self) -> Box<dyn RouterTransport> {
            Box::new(self.clone())
        }
    }

    impl RouterTransport for MockTransport {
        fn is_peer_connected(&self, p: &PeerId) -> bool {
            self.0.borrow().connected.contains(p)
        }
        fn is_peer_reachable(&self, p: &PeerId) -> bool {
            self.0.borrow().reachable.contains(p)
        }
        fn can_deliver_promptly(&self, p: &PeerId) -> bool {
            self.0.borrow().promptly.contains(p)
        }
        fn can_deliver_securely(&self, p: &PeerId) -> bool {
            self.0.borrow().securely.contains(p)
        }
        fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
            self.0.borrow().snapshots.clone()
        }
        fn send_private(&self, content: &str, to: &PeerId, message_id: &str) -> bool {
            self.0.borrow_mut().private_sends.push((
                content.to_string(),
                to.clone(),
                message_id.to_string(),
            ));
            true
        }
        fn send_courier(
            &self,
            _content: &str,
            message_id: &str,
            _recipient_key: &[u8; 32],
            via: &[PeerId],
        ) -> Vec<PeerId> {
            let accepts = self.0.borrow().courier_accepts;
            if !accepts {
                return Vec::new();
            }
            self.0
                .borrow_mut()
                .courier_sends
                .push((message_id.to_string(), via.to_vec()));
            via.to_vec()
        }
    }

    #[derive(Default)]
    struct TestDirectory {
        keys: BTreeMap<PeerId, [u8; 32]>,
    }

    impl TestDirectory {
        fn with_key(mut self, p: &PeerId, k: [u8; 32]) -> Self {
            self.keys.insert(p.clone(), k);
            self
        }
    }

    impl CourierDirectory for TestDirectory {
        fn noise_key(&self, p: &PeerId) -> Option<[u8; 32]> {
            self.keys.get(p).copied()
        }
    }

    fn router(
        transports: Vec<Box<dyn RouterTransport>>,
        directory: TestDirectory,
    ) -> MessageRouter {
        let mut outbox = OutboxStore::new(Arc::new(MemoryStorage::new()), Arc::new(PlainSealer));
        // Load so the persistence path is genuinely exercised rather than deferred.
        let dropped = outbox.load().expect("cold load of an empty store");
        assert!(dropped.is_empty());
        MessageRouter::new(transports, Box::new(directory), outbox)
    }

    /// A bridge deposit that records every message ID it was asked to park.
    fn recording_bridge(log: Rc<RefCell<Vec<String>>>, accepts: bool) -> BridgeDeposit {
        Box::new(move |_content, message_id, _recipient_key| {
            log.borrow_mut().push(message_id.to_string());
            accepts
        })
    }

    fn carried(events: &[RouterEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, RouterEvent::Carried { .. }))
            .count()
    }

    fn drops(events: &[RouterEvent]) -> Vec<(String, DropReason)> {
        events
            .iter()
            .filter_map(|e| match e {
                RouterEvent::Dropped(d) => Some((d.message_id.clone(), d.reason)),
                _ => None,
            })
            .collect()
    }

    fn no_storage_failures(events: &[RouterEvent]) {
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RouterEvent::StorageFailed { .. })),
            "unexpected storage failure: {events:?}"
        );
    }

    // ----------------------------------------------------------- the four branches

    #[test]
    fn branch_one_connected_and_secure_sends_without_retaining_or_couriering() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .connected_secure(&recipient)
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);
        let bridge_log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(bridge_log.clone(), true));

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Secure);
        assert!(outcome.events.is_empty());
        assert_eq!(transport.private_sends().len(), 1);
        // The strongest signal there is: nothing retained, nobody handed a copy,
        // nothing parked on a relay.
        assert!(router.outbox().queue(&recipient).is_empty());
        assert!(transport.courier_sends().is_empty());
        assert!(bridge_log.borrow().is_empty());
    }

    #[test]
    fn branch_two_connected_without_a_session_sends_retains_and_couriers() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .connected_insecure(&recipient)
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Insecure);
        no_storage_failures(&outcome.events);
        assert_eq!(transport.private_sends().len(), 1);

        // Retained: a replayed announce could have bound this ID to a link whose
        // owner can never finish the handshake.
        let queue = router.outbox().queue(&recipient);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].send_attempts, 1);

        // And couriered. This is the deliberate metadata tradeoff — do not
        // "optimise" it away.
        assert_eq!(
            transport.courier_sends(),
            vec![("m1".to_string(), vec![courier])]
        );
        assert_eq!(carried(&outcome.events), 1);
    }

    #[test]
    fn branch_three_reachable_and_prompt_retains_but_does_not_courier() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .reachable_prompt(&recipient)
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Reachable);
        assert_eq!(router.outbox().queue(&recipient).len(), 1);
        assert_eq!(router.outbox().queue(&recipient)[0].send_attempts, 1);
        // It really left the device; no need to spend courier slots on it.
        assert!(transport.courier_sends().is_empty());
        assert_eq!(carried(&outcome.events), 0);
    }

    #[test]
    fn branch_three_reachable_but_only_queued_also_couriers() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .reachable_slow(&recipient)
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        // Same LinkState as the prompt case — the *retention* rule is identical.
        assert_eq!(outcome.link, LinkState::Reachable);
        assert_eq!(router.outbox().queue(&recipient).len(), 1);
        // But the send only joined a queue waiting on internet that may never come,
        // so couriers get a copy too.
        assert_eq!(transport.courier_sends().len(), 1);
        assert_eq!(carried(&outcome.events), 1);
    }

    #[test]
    fn branch_four_no_transport_retains_with_zero_attempts_and_couriers() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Unreachable);
        assert!(transport.private_sends().is_empty());
        let queue = router.outbox().queue(&recipient);
        assert_eq!(queue.len(), 1);
        // Nothing was transmitted, so nothing counts against the attempt cap.
        assert_eq!(queue[0].send_attempts, 0);
        assert_eq!(transport.courier_sends().len(), 1);
    }

    #[test]
    fn a_recipient_with_no_known_static_key_cannot_be_couriered_to() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new()
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        // No key for the recipient: nothing can be sealed to them.
        let directory = TestDirectory::default();
        let mut router = router(vec![transport.boxed()], directory);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Unreachable);
        assert!(transport.courier_sends().is_empty());
        assert!(log.borrow().is_empty());
        assert_eq!(carried(&outcome.events), 0);
        // Still retained: a direct delivery can still happen later.
        assert_eq!(router.outbox().queue(&recipient).len(), 1);
    }

    #[test]
    fn a_refused_courier_deposit_is_not_recorded_as_carried() {
        let recipient = peer(1);
        let courier = peer(2);
        // Snapshots offer a courier, but the transport cannot courier at all.
        let transport =
            MockTransport::new().with_snapshots(vec![snapshot(&courier, Some(key(2)), true)]);
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(carried(&outcome.events), 0);
        assert!(
            router.outbox().queue(&recipient)[0]
                .deposited_courier_keys
                .is_empty(),
            "a refused deposit must not burn a courier slot"
        );
    }

    // ------------------------------------------------------------ courier choice

    #[test]
    fn courier_selection_takes_verified_peers_in_snapshot_order_capped() {
        let recipient = peer(1);
        let (v1, v2, unverified, v3, v4) = (peer(10), peer(11), peer(12), peer(13), peer(14));
        let transport = MockTransport::new()
            .with_snapshots(vec![
                snapshot(&v1, Some(key(10)), true),
                snapshot(&v2, Some(key(11)), true),
                snapshot(&unverified, Some(key(12)), false),
                snapshot(&v3, Some(key(13)), true),
                snapshot(&v4, Some(key(14)), true),
            ])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);

        let sends = transport.courier_sends();
        assert_eq!(sends.len(), 1);
        // Verified peers in snapshot order, the unverified one skipped, capped.
        assert_eq!(sends[0].1, vec![v1, v2, v3]);
        assert_eq!(sends[0].1.len(), MAX_COURIERS_PER_MESSAGE);
    }

    #[test]
    fn courier_selection_excludes_the_recipient_unverified_and_keyless_peers() {
        let recipient = peer(1);
        let stranger = peer(10);
        let keyless = peer(12);
        let verified = peer(11);
        let transport = MockTransport::new()
            .with_snapshots(vec![
                // The recipient itself, matched by key: never hand someone their own
                // mail back as an opaque envelope.
                snapshot(&recipient, Some(key(1)), true),
                // Not verified: may not carry mail.
                snapshot(&stranger, Some(key(10)), false),
                // No static key at all: invisible to the courier path.
                snapshot(&keyless, None, true),
                snapshot(&verified, Some(key(11)), true),
            ])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);

        let sends = transport.courier_sends();
        assert_eq!(sends[0].1, vec![verified]);
    }

    #[test]
    fn a_disconnected_peer_is_not_a_courier() {
        let recipient = peer(1);
        let transport = MockTransport::new()
            .with_snapshots(vec![PeerSnapshot {
                peer: peer(10),
                is_connected: false,
                noise_public_key: Some(key(10)),
                is_verified: true,
            }])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert!(transport.courier_sends().is_empty());
    }

    #[test]
    fn a_courier_already_carrying_a_message_is_not_asked_twice() {
        let recipient = peer(1);
        let first = peer(10);
        let second = peer(11);
        let transport = MockTransport::new()
            .with_snapshots(vec![snapshot(&first, Some(key(10)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        assert_eq!(
            transport.courier_sends(),
            vec![("m1".to_string(), vec![first])]
        );

        // A second courier appears and the same message ID is re-sent. The outbox
        // preserves the deposit record across the replacement, so the first courier
        // is excluded rather than re-burned.
        transport
            .0
            .borrow_mut()
            .snapshots
            .push(snapshot(&second, Some(key(11)), true));
        router.send_private("hi", &recipient, "bob", "m1", 1_100);

        let sends = transport.courier_sends();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[1].1, vec![second]);
    }

    // ------------------------------------------------- courier_became_available

    #[test]
    fn a_newly_connected_courier_picks_up_mail_it_is_not_already_carrying() {
        let recipient = peer(1);
        let courier = peer(2);
        // Nobody around at send time.
        let transport = MockTransport::new().accepting_couriers();
        let directory = TestDirectory::default()
            .with_key(&recipient, key(1))
            .with_key(&courier, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        assert!(transport.courier_sends().is_empty());

        // Now a verified courier walks in. This is what turns couriering from "a
        // courier happened to be nearby at send time" into eventual spread.
        transport.courier_arrives(&courier, key(2), true);
        let events = router.courier_became_available(&courier, 1_100);

        assert_eq!(
            transport.courier_sends(),
            vec![("m1".to_string(), vec![courier.clone()])]
        );
        assert_eq!(carried(&events), 1);
        assert!(router.outbox().queue(&recipient)[0].is_carried_by(&key(2)));

        // A second announce from the same peer must not re-burn a slot on a copy
        // they already hold.
        let again = router.courier_became_available(&courier, 1_200);
        assert_eq!(transport.courier_sends().len(), 1);
        assert_eq!(carried(&again), 0);
    }

    #[test]
    fn a_courier_is_never_handed_mail_addressed_to_itself() {
        let courier = peer(2);
        let transport = MockTransport::new().accepting_couriers();
        let directory = TestDirectory::default().with_key(&courier, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        // Mail *for* the courier. It flushes directly on connect instead.
        router.send_private("hi", &courier, "bob", "m1", 1_000);
        transport.courier_arrives(&courier, key(2), true);

        let events = router.courier_became_available(&courier, 1_100);

        assert!(transport.courier_sends().is_empty());
        assert_eq!(carried(&events), 0);
    }

    #[test]
    fn an_unverified_peer_may_not_pick_up_mail() {
        let recipient = peer(1);
        let stranger = peer(2);
        let transport = MockTransport::new().accepting_couriers();
        let directory = TestDirectory::default()
            .with_key(&recipient, key(1))
            .with_key(&stranger, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.courier_arrives(&stranger, key(2), false);

        let events = router.courier_became_available(&stranger, 1_100);

        assert!(transport.courier_sends().is_empty());
        assert_eq!(carried(&events), 0);
    }

    #[test]
    fn a_verified_stranger_may_pick_up_mail() {
        let recipient = peer(1);
        let stranger = peer(2);
        let transport = MockTransport::new().accepting_couriers();
        let directory = TestDirectory::default()
            .with_key(&recipient, key(1))
            .with_key(&stranger, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.courier_arrives(&stranger, key(2), true);

        let events = router.courier_became_available(&stranger, 1_100);

        assert_eq!(transport.courier_sends().len(), 1);
        assert_eq!(carried(&events), 1);
    }

    #[test]
    fn expired_mail_is_not_handed_to_a_new_courier() {
        let recipient = peer(1);
        let courier = peer(2);
        let transport = MockTransport::new().accepting_couriers();
        let directory = TestDirectory::default()
            .with_key(&recipient, key(1))
            .with_key(&courier, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.courier_arrives(&courier, key(2), true);

        let events = router.courier_became_available(&courier, 1_000 + MESSAGE_TTL_SECS + 1);

        assert!(transport.courier_sends().is_empty());
        assert_eq!(carried(&events), 0);
    }

    // ------------------------------------------------------------ read receipts

    #[test]
    fn a_read_receipt_with_no_reachable_transport_is_not_recorded_as_sent() {
        let sender = peer(1);
        let transport = MockTransport::new();
        let router = router(vec![transport.boxed()], TestDirectory::default());

        let accepted = router.send_read_receipt(&sender, |t| t.send_private("r", &peer(1), "r1"));

        // False means "dropped, no route". Recording it as sent would leave the
        // sender's message unread forever; it is retried on the next read scan.
        assert!(!accepted);
        assert!(transport.private_sends().is_empty());
    }

    #[test]
    fn a_read_receipt_goes_over_the_first_reachable_transport() {
        let sender = peer(1);
        let dead = MockTransport::new();
        let live = MockTransport::new().reachable_prompt(&sender);
        let router = router(vec![dead.boxed(), live.boxed()], TestDirectory::default());

        let accepted = router.send_read_receipt(&sender, |t| t.send_private("r", &peer(1), "r1"));

        assert!(accepted);
        assert!(dead.private_sends().is_empty());
        assert_eq!(live.private_sends().len(), 1);
    }

    #[test]
    fn a_read_receipt_the_transport_refuses_is_not_recorded_as_sent() {
        let sender = peer(1);
        let transport = MockTransport::new().reachable_prompt(&sender);
        let router = router(vec![transport.boxed()], TestDirectory::default());

        // Reachable, but the wire call itself failed. Same consequence as no route:
        // it must not be recorded as sent.
        assert!(!router.send_read_receipt(&sender, |_| false));
    }

    // ----------------------------------------------------------------- flushes

    #[test]
    fn a_secure_flush_sends_and_stops_retaining() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        assert_eq!(router.outbox().queue(&recipient).len(), 1);

        transport.becomes_connected(&recipient, true);
        let events = router.flush_outbox(&recipient, 1_100);

        assert_eq!(transport.private_sends().len(), 1);
        assert!(router.outbox().queue(&recipient).is_empty());
        assert!(drops(&events).is_empty());
        no_storage_failures(&events);
    }

    #[test]
    fn an_insecure_flush_never_counts_toward_the_attempt_cap() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.becomes_connected(&recipient, false);

        // A handshake stalling across reconnect flapping must not burn the cap and
        // lose the very copy the retention exists to preserve.
        for _ in 0..(MAX_SEND_ATTEMPTS * 3) {
            let events = router.flush_outbox(&recipient, 1_100);
            assert!(drops(&events).is_empty(), "insecure flush must not drop");
        }

        let queue = router.outbox().queue(&recipient);
        assert_eq!(queue.len(), 1, "the retained copy must survive");
        assert_eq!(queue[0].send_attempts, 0);
        assert_eq!(
            transport.private_sends().len() as u32,
            MAX_SEND_ATTEMPTS * 3,
            "every flush still transmits over the live link"
        );
    }

    #[test]
    fn a_reachable_flush_counts_attempts_and_drops_at_the_cap() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        // Branch 4 retains at zero attempts.
        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.becomes_reachable(&recipient, true);

        for expected in 1..=MAX_SEND_ATTEMPTS {
            let events = router.flush_outbox(&recipient, 1_100);
            assert!(drops(&events).is_empty());
            assert_eq!(router.outbox().queue(&recipient)[0].send_attempts, expected);
        }

        // The cap is reached: give up visibly rather than leaving the message on
        // "sending" forever.
        let events = router.flush_outbox(&recipient, 1_100);
        assert_eq!(
            drops(&events),
            vec![("m1".to_string(), DropReason::AttemptsExhausted)]
        );
        assert!(router.outbox().queue(&recipient).is_empty());
        assert_eq!(transport.private_sends().len() as u32, MAX_SEND_ATTEMPTS);
    }

    #[test]
    fn a_flush_with_no_route_leaves_the_message_untouched() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        for _ in 0..(MAX_SEND_ATTEMPTS * 2) {
            router.flush_outbox(&recipient, 1_100);
        }

        let queue = router.outbox().queue(&recipient);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].send_attempts, 0);
        assert!(transport.private_sends().is_empty());
    }

    #[test]
    fn a_flush_drops_expired_mail_instead_of_sending_it() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        transport.becomes_connected(&recipient, true);

        let events = router.flush_outbox(&recipient, 1_000 + MESSAGE_TTL_SECS + 1);

        assert_eq!(
            drops(&events),
            vec![("m1".to_string(), DropReason::Expired)]
        );
        assert!(transport.private_sends().is_empty());
        assert!(router.outbox().queue(&recipient).is_empty());
    }

    #[test]
    fn flush_all_covers_every_peer_with_retained_mail() {
        let (a, b) = (peer(1), peer(2));
        let transport = MockTransport::new();
        let directory = TestDirectory::default()
            .with_key(&a, key(1))
            .with_key(&b, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("x", &a, "ann", "m1", 1_000);
        router.send_private("y", &b, "ben", "m2", 1_000);
        transport.becomes_connected(&a, true);
        transport.becomes_connected(&b, true);

        router.flush_all_outbox(1_100);

        assert_eq!(transport.private_sends().len(), 2);
        assert!(router.outbox().is_empty());
    }

    #[test]
    fn cleanup_expires_stale_mail_and_reports_it() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("old", &recipient, "bob", "m1", 1_000);
        router.send_private("new", &recipient, "bob", "m2", 1_000 + MESSAGE_TTL_SECS);

        let events = router.cleanup_expired(1_000 + MESSAGE_TTL_SECS + 1);

        assert_eq!(
            drops(&events),
            vec![("m1".to_string(), DropReason::Expired)]
        );
        assert_eq!(router.outbox().queue(&recipient).len(), 1);
    }

    #[test]
    fn an_ack_stops_the_retention_and_a_read_ack_is_never_downgraded() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);

        let (outcome, events) = router.mark_acked(&recipient, "m1", AckKind::Read);
        assert!(outcome.cleared);
        assert_eq!(outcome.strongest, AckKind::Read);
        no_storage_failures(&events);
        assert!(router.outbox().queue(&recipient).is_empty());

        // A delivery ack landing afterwards must not downgrade the read.
        let (late, _) = router.mark_acked(&recipient, "m1", AckKind::Delivered);
        assert!(!late.cleared);
        assert_eq!(late.strongest, AckKind::Read);
    }

    // ------------------------------------------------------------ bridge drops

    #[test]
    fn the_bridge_drop_runs_even_with_no_connected_courier() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(*log.borrow(), vec!["m1".to_string()]);
        assert_eq!(carried(&outcome.events), 1);
    }

    #[test]
    fn a_rejected_bridge_drop_is_not_reported_as_carried() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), false));

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(log.borrow().len(), 1);
        // A socket write followed by a relay rejection must not show as "carried".
        assert_eq!(carried(&outcome.events), 0);
    }

    #[test]
    fn the_bridge_retry_sweep_skips_peers_a_transport_can_promptly_reach() {
        let stalled = peer(1);
        let live = peer(2);
        let transport = MockTransport::new();
        let directory = TestDirectory::default()
            .with_key(&stalled, key(1))
            .with_key(&live, key(2));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("a", &stalled, "ann", "m1", 1_000);
        router.send_private("b", &live, "ben", "m2", 1_000);

        // Wired only now, so the original sends never deposited: exactly the race
        // the sweep exists to cover.
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));
        transport.becomes_reachable(&live, true);

        let events = router.retry_bridge_deposits(1_100);

        assert_eq!(*log.borrow(), vec!["m1".to_string()]);
        assert_eq!(carried(&events), 1);
    }

    #[test]
    fn the_bridge_retry_sweep_still_fires_for_a_peer_reachable_but_not_promptly() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));
        // Reachable, but the send would only join a queue.
        transport.becomes_reachable(&recipient, false);

        router.retry_bridge_deposits(1_100);

        assert_eq!(*log.borrow(), vec!["m1".to_string()]);
    }

    #[test]
    fn the_bridge_retry_sweep_ignores_expired_mail() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));

        router.retry_bridge_deposits(1_000 + MESSAGE_TTL_SECS + 1);

        assert!(log.borrow().is_empty());
    }

    #[test]
    fn recovery_flushes_and_retries_the_bridge() {
        let recipient = peer(1);
        let transport = MockTransport::new();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![transport.boxed()], directory);
        let log = Rc::new(RefCell::new(Vec::new()));
        router.set_bridge_deposit(recording_bridge(log.clone(), true));

        router.send_private("hi", &recipient, "bob", "m1", 1_000);
        log.borrow_mut().clear();
        transport.becomes_reachable(&recipient, true);

        let events = router.recover_outbox(1_100);

        no_storage_failures(&events);
        // The flush re-sent it over the reachable transport...
        assert_eq!(transport.private_sends().len(), 1);
        // ...and the sweep skipped it, because that transport can deliver promptly.
        assert!(log.borrow().is_empty());
    }

    // ----------------------------------------------------- transport precedence

    #[test]
    fn the_first_connected_transport_decides_even_without_a_secure_session() {
        // bitchat picks the first connected transport and stays with it. Falling
        // through to a *different* transport's secure session would be a different
        // decision than the tree describes.
        let recipient = peer(1);
        let insecure = MockTransport::new().connected_insecure(&recipient);
        let secure = MockTransport::new().connected_secure(&recipient);
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![insecure.boxed(), secure.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Insecure);
        assert_eq!(insecure.private_sends().len(), 1);
        assert!(secure.private_sends().is_empty());
        assert_eq!(router.outbox().queue(&recipient).len(), 1);
    }

    #[test]
    fn a_connected_transport_outranks_a_reachable_one() {
        let recipient = peer(1);
        let reachable = MockTransport::new().reachable_prompt(&recipient);
        let connected = MockTransport::new().connected_secure(&recipient);
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![reachable.boxed(), connected.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert_eq!(outcome.link, LinkState::Secure);
        assert!(reachable.private_sends().is_empty());
        assert_eq!(connected.private_sends().len(), 1);
    }

    #[test]
    fn couriers_are_tried_on_every_transport_until_one_accepts() {
        let recipient = peer(1);
        let courier = peer(2);
        // The first transport knows the courier but refuses to carry.
        let refuses =
            MockTransport::new().with_snapshots(vec![snapshot(&courier, Some(key(2)), true)]);
        let accepts = MockTransport::new()
            .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
            .accepting_couriers();
        let directory = TestDirectory::default().with_key(&recipient, key(1));
        let mut router = router(vec![refuses.boxed(), accepts.boxed()], directory);

        let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

        assert!(refuses.courier_sends().is_empty());
        assert_eq!(accepts.courier_sends().len(), 1);
        assert_eq!(carried(&outcome.events), 1);
    }
}
