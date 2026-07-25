//! Retained sender mail: the answer to "I have something for you and you are not
//! here right now".
//!
//! Ported from bitchat's `Services/Courier/MessageOutboxStore.swift`, together with
//! the outbox half of `Services/MessageRouter.swift` (`enqueue`, `flushOutbox`,
//! `markDelivered`, `cleanupExpiredMessages`, `mergeRecoveredOutbox`). The Swift
//! store and the Swift router split this state between them; keeping the *state* in
//! one place and leaving the *decision tree* to [`crate::router`] is the only
//! structural change.
//!
//! # What this module is responsible for
//!
//! Retention policy, and nothing else. It never sends, never looks at a clock, and
//! never learns which transports exist. The router decides how good the link to a
//! peer is right now, tells this store as a [`LinkState`], and gets back a
//! [`FlushPlan`] describing what to transmit and what has been given up on.
//!
//! # Nothing plaintext reaches disk
//!
//! A queued message holds its content in memory because
//! [`RouterTransport::send_private`](crate::transport::RouterTransport::send_private)
//! needs it to resend. It never reaches [`Storage`] in that form: serialization
//! produces a blob, the injected [`Sealer`] turns that blob into ciphertext, and
//! only the ciphertext is handed to [`Storage::save`]. The sealing key lives behind
//! the `Sealer` implementation and never enters this crate, so a bug here cannot
//! write plaintext even by accident. There is deliberately no code path that hands
//! `Storage` anything derived from message content unsealed — not even an "identity"
//! sealer for tests; the test sealer below is a real (if trivial) transform, and it
//! is `#[cfg(test)]`.
//!
//! # Persistence is explicit
//!
//! Every mutator changes memory and marks the store dirty; none of them touch
//! storage. The driver calls [`OutboxStore::persist`] when it is ready to pay for
//! IO, and a persist on a clean store is free. That keeps a disk failure from
//! swallowing a flush plan the router still has to act on, and it keeps this module
//! sans-IO in the same sense as the rest of the crate.
//!
//! # Cold-load recovery merges, it never replaces
//!
//! [`OutboxStore::load`] is idempotent and additive: it merges what it reads into
//! what is already in memory. bitchat's motivating case is iOS file protection —
//! a cold BLE wake before first unlock reads nothing, the app queues new mail
//! anyway, and the durable queue only becomes readable later. Replacing at that
//! point would discard everything accepted during the locked window. So `load` may
//! be called again whenever storage becomes readable, and the second call performs
//! the recovery merge.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use chatter_mesh::ids::PeerId;

use crate::error::StoreError;
use crate::storage::{Storage, StorageKey};
use crate::{
    MAX_COURIERS_PER_MESSAGE, MAX_MESSAGES_PER_PEER, MAX_SEND_ATTEMPTS, MESSAGE_TTL_SECS,
};

/// How many acked message IDs are remembered, newest wins.
///
/// The memory does two jobs: it stops a cold-load merge from resurrecting a message
/// that was acked while the durable queue was unreadable, and it stops a late
/// delivery ack from downgrading a read ack. Both only matter for the recent past,
/// so the set is bounded rather than kept for the life of the process.
pub const ACK_MEMORY_LIMIT: usize = 1024;

/// Wire tag for the serialized outbox, so a blob from another store is rejected as
/// malformed rather than decoded into nonsense.
const MAGIC: [u8; 3] = *b"OBX";
const FORMAT_VERSION: u8 = 1;

/// The whole outbox: per-peer queues, oldest first.
pub type Snapshot = BTreeMap<PeerId, Vec<QueuedMessage>>;

/// Turns the serialized outbox into ciphertext and back.
///
/// This is the seam that keeps the sealing key out of this crate. The
/// implementation lives in the integration layer next to the keyring; everything
/// this module sees is opaque bytes.
///
/// # The two failure modes are not the same
///
/// [`open`](Sealer::open) must distinguish them, because they lead to opposite
/// decisions:
///
/// - **Unrecoverable** — the blob is corrupt, or its key is definitively gone (a
///   restore onto a new device, say). Return [`StoreError::Malformed`]. The store
///   treats this as "there is nothing durable", starts empty, and is free to
///   overwrite the unreadable bytes.
/// - **Not readable yet** — the key is behind a lock that will open later. Return
///   any other error. The store stays un-loaded, refuses to persist over the blob it
///   could not read, and waits for another [`OutboxStore::load`].
///
/// Getting this backwards is how retained mail gets destroyed, so implementations
/// should default to the second when unsure.
pub trait Sealer: Send + Sync {
    /// Seal a serialized outbox. The result is what reaches disk.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError>;

    /// Open bytes previously produced by [`seal`](Sealer::seal).
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError>;
}

/// A private message being retained until it is acknowledged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    /// Plaintext, in memory only. Sealed before it reaches [`Storage`].
    pub content: String,
    /// The recipient's nickname at send time, needed to re-address the resend.
    pub nickname: String,
    pub message_id: String,
    /// Unix seconds. The TTL and the recovery sort are both against this.
    pub timestamp: u64,
    /// Sends made against a weak reachability signal. Only those count — see
    /// [`LinkState::Insecure`].
    pub send_attempts: u32,
    /// Noise static keys of couriers already carrying this message, so a deposit
    /// retry adds couriers instead of re-burning the ones already spent.
    pub deposited_courier_keys: BTreeSet<[u8; 32]>,
}

impl QueuedMessage {
    /// A fresh message with no attempts and no couriers.
    pub fn new(
        content: impl Into<String>,
        nickname: impl Into<String>,
        message_id: impl Into<String>,
        timestamp: u64,
    ) -> Self {
        Self {
            content: content.into(),
            nickname: nickname.into(),
            message_id: message_id.into(),
            timestamp,
            send_attempts: 0,
            deposited_courier_keys: BTreeSet::new(),
        }
    }

    /// Builder form for the router's "sent once already" case.
    #[must_use]
    pub fn with_send_attempts(mut self, attempts: u32) -> Self {
        self.send_attempts = attempts;
        self
    }

    /// Whether this message may ride with another courier.
    pub fn remaining_courier_slots(&self) -> usize {
        MAX_COURIERS_PER_MESSAGE.saturating_sub(self.deposited_courier_keys.len())
    }

    /// Whether the courier holding this static key already carries the message.
    pub fn is_carried_by(&self, courier_key: &[u8; 32]) -> bool {
        self.deposited_courier_keys.contains(courier_key)
    }

    /// Whether the retention window has closed on this message.
    pub fn is_expired(&self, now_unix_secs: u64) -> bool {
        // A clock that went backwards leaves the message un-expired rather than
        // instantly expiring every queue, which matches Swift's signed interval.
        now_unix_secs.saturating_sub(self.timestamp) > MESSAGE_TTL_SECS
    }
}

/// How good the route to a peer is *right now*, as judged by the router.
///
/// The four cases are the four branches of bitchat's `flushOutbox`, and the
/// difference between the middle two is the subtlest rule in the whole store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Connected, with a session that can complete an encrypted delivery now.
    /// Strong enough to stop retaining.
    Secure,
    /// Connected, but no secure session yet.
    ///
    /// The link binding is forgeable — a replayed announce can bind an absent
    /// peer's ID to the replayer's link — so the copy is retained. But the send
    /// **does not** count toward the attempt cap: the message really did go out
    /// over a live link, and a peer whose handshake stalls across reconnect
    /// flapping would otherwise burn all [`MAX_SEND_ATTEMPTS`] attempts and lose
    /// the very copy this retention exists to preserve. Retention stays bounded by
    /// the TTL and the per-peer FIFO cap instead.
    Insecure,
    /// Not connected, but a freshness heuristic says the peer is around. The send
    /// can silently go nowhere, so it is retained *and* charged an attempt —
    /// otherwise a peer on a client that never acks is retried forever.
    Reachable,
    /// No route. Nothing is sent, nothing is charged.
    Unreachable,
}

/// Why a retained message was given up on.
///
/// Every one of these has to reach the UI: a message that leaves the outbox without
/// an ack and without a visible failure sits on "sending" forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// Past the 24 h retention window.
    Expired,
    /// Sent [`MAX_SEND_ATTEMPTS`] times against a weak signal with no ack.
    AttemptsExhausted,
    /// Evicted by the per-peer FIFO cap to make room for newer mail.
    QueueOverflow,
}

/// A message that left the outbox undelivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedMessage {
    pub message_id: String,
    pub peer: PeerId,
    pub reason: DropReason,
}

/// What a flush decided.
///
/// The store has already applied the state changes by the time this is returned:
/// `send` messages under [`LinkState::Secure`] are gone from the outbox, attempts
/// under [`LinkState::Reachable`] are already charged, and `dropped` messages are
/// already removed. The router transmits `send` and surfaces `dropped`.
///
/// Acceptance by a transport is deliberately not fed back: bitchat treats the send
/// as made either way, and only an ack clears a retained copy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushPlan {
    /// Transmit these, oldest first.
    pub send: Vec<QueuedMessage>,
    /// Report these as failed.
    pub dropped: Vec<DroppedMessage>,
}

impl FlushPlan {
    pub fn is_empty(&self) -> bool {
        self.send.is_empty() && self.dropped.is_empty()
    }
}

/// Which kind of acknowledgment cleared a message.
///
/// Ordered so that `max` is the stronger one: a read ack implies delivery, and must
/// never be downgraded by a delivery ack that arrives afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AckKind {
    Delivered,
    Read,
}

/// The result of acking a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckOutcome {
    /// Whether a retained copy was actually removed. False when the ack arrived for
    /// a message this view never had — a duplicate ack, or one that landed while
    /// the durable queue was still unreadable. Either way the ack is remembered.
    pub cleared: bool,
    /// The strongest ack seen for this message ID, which is what the UI should
    /// display. A [`AckKind::Delivered`] arriving after a [`AckKind::Read`] reports
    /// `Read` here.
    pub strongest: AckKind,
}

/// Bounded memory of recently acked message IDs.
struct AckMemory {
    kinds: BTreeMap<String, AckKind>,
    order: VecDeque<String>,
}

impl AckMemory {
    fn new() -> Self {
        Self {
            kinds: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Records an ack and returns the strongest one seen for this ID.
    fn record(&mut self, message_id: &str, kind: AckKind) -> AckKind {
        match self.kinds.get_mut(message_id) {
            Some(existing) => {
                // Never downgrade. A read ack outranks a delivery ack whatever
                // order the two arrive in.
                *existing = (*existing).max(kind);
                *existing
            }
            None => {
                self.kinds.insert(message_id.to_owned(), kind);
                self.order.push_back(message_id.to_owned());
                while self.order.len() > ACK_MEMORY_LIMIT {
                    if let Some(evicted) = self.order.pop_front() {
                        self.kinds.remove(&evicted);
                    }
                }
                kind
            }
        }
    }

    fn contains(&self, message_id: &str) -> bool {
        self.kinds.contains_key(message_id)
    }

    fn strongest(&self, message_id: &str) -> Option<AckKind> {
        self.kinds.get(message_id).copied()
    }

    fn clear(&mut self) {
        self.kinds.clear();
        self.order.clear();
    }
}

/// Per-peer retained mail with a TTL, an attempt cap, a FIFO size cap, and courier
/// deposit tracking.
pub struct OutboxStore {
    storage: Arc<dyn Storage>,
    sealer: Arc<dyn Sealer>,
    outbox: Snapshot,
    acks: AckMemory,
    /// Whether the durable blob has been read. Until it has, persisting would
    /// overwrite mail we have never seen.
    loaded: bool,
    /// Whether memory has diverged from what was last written.
    dirty: bool,
}

impl OutboxStore {
    pub fn new(storage: Arc<dyn Storage>, sealer: Arc<dyn Sealer>) -> Self {
        Self {
            storage,
            sealer,
            outbox: Snapshot::new(),
            acks: AckMemory::new(),
            loaded: false,
            dirty: false,
        }
    }

    // ----------------------------------------------------------------- reading

    /// Retained mail for one peer, oldest first.
    pub fn queue(&self, peer: &PeerId) -> &[QueuedMessage] {
        self.outbox.get(peer).map_or(&[], Vec::as_slice)
    }

    /// Every peer with retained mail.
    pub fn peers(&self) -> Vec<PeerId> {
        self.outbox.keys().cloned().collect()
    }

    /// Every queue, for the courier and bridge-drop retry sweeps.
    pub fn iter(&self) -> impl Iterator<Item = (&PeerId, &[QueuedMessage])> {
        self.outbox.iter().map(|(p, q)| (p, q.as_slice()))
    }

    /// One retained message, if it is still queued for that peer.
    pub fn queued(&self, peer: &PeerId, message_id: &str) -> Option<&QueuedMessage> {
        self.queue(peer)
            .iter()
            .find(|m| m.message_id == message_id)
    }

    /// Total retained messages across all peers.
    pub fn len(&self) -> usize {
        self.outbox.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.outbox.is_empty()
    }

    /// A copy of the whole outbox.
    pub fn snapshot(&self) -> Snapshot {
        self.outbox.clone()
    }

    /// Whether the durable blob has been read successfully at least once. While
    /// this is false, [`persist`](Self::persist) deliberately writes nothing.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Whether memory has diverged from the last successful write.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The strongest ack seen for a message ID this session, if any.
    pub fn ack_state(&self, message_id: &str) -> Option<AckKind> {
        self.acks.strongest(message_id)
    }

    // ---------------------------------------------------------------- mutating

    /// Retain a message for a peer.
    ///
    /// Re-enqueueing an ID that is already queued replaces the entry — keeping the
    /// attempt count fresh and moving it to the back — but **unions the courier
    /// keys**, or the replacement re-burns slots already spent on it.
    ///
    /// Returns the message evicted by the per-peer FIFO cap, if the queue was full.
    #[must_use = "an evicted message must be surfaced or it stays on 'sending' forever"]
    pub fn enqueue(&mut self, peer: &PeerId, message: QueuedMessage) -> Option<DroppedMessage> {
        let mut message = message;
        let queue = self.outbox.entry(peer.clone()).or_default();

        if let Some(index) = queue
            .iter()
            .position(|m| m.message_id == message.message_id)
        {
            if let Some(existing) = queue.get(index) {
                message
                    .deposited_courier_keys
                    .extend(existing.deposited_courier_keys.iter().copied());
            }
            queue.remove(index);
        }
        queue.push(message);

        let evicted = if queue.len() > MAX_MESSAGES_PER_PEER {
            let evicted = queue.remove(0);
            Some(DroppedMessage {
                message_id: evicted.message_id,
                peer: peer.clone(),
                reason: DropReason::QueueOverflow,
            })
        } else {
            None
        };

        self.dirty = true;
        evicted
    }

    /// Record that couriers now carry a message.
    ///
    /// Unions rather than replaces, and silently does nothing when the message has
    /// already been acked away — a deposit confirmation can land after delivery.
    pub fn record_courier_deposit(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        courier_keys: &[[u8; 32]],
    ) {
        let Some(queue) = self.outbox.get_mut(peer) else {
            return;
        };
        let Some(message) = queue.iter_mut().find(|m| m.message_id == message_id) else {
            return;
        };
        let before = message.deposited_courier_keys.len();
        message
            .deposited_courier_keys
            .extend(courier_keys.iter().copied());
        if message.deposited_courier_keys.len() != before {
            self.dirty = true;
        }
    }

    /// A delivery or read ack: stop retaining the message.
    ///
    /// Both kinds clear it. The ack is remembered either way, so that a cold-load
    /// merge cannot resurrect it and a later delivery ack cannot downgrade a read
    /// ack — see [`AckOutcome::strongest`].
    pub fn mark_acked(&mut self, message_id: &str, kind: AckKind) -> AckOutcome {
        let strongest = self.acks.record(message_id, kind);

        let mut cleared = false;
        self.outbox.retain(|_, queue| {
            let before = queue.len();
            queue.retain(|m| m.message_id != message_id);
            if queue.len() != before {
                cleared = true;
            }
            !queue.is_empty()
        });
        if cleared {
            self.dirty = true;
        }

        AckOutcome { cleared, strongest }
    }

    /// Decide what to do with everything retained for one peer.
    ///
    /// Expired mail is dropped first, whatever the link is. Then, per
    /// [`LinkState`]: a secure link clears the copy, an insecure one keeps it
    /// without charging an attempt, a merely reachable one keeps it and charges
    /// one, and no route at all leaves it untouched.
    pub fn flush(&mut self, peer: &PeerId, link: LinkState, now_unix_secs: u64) -> FlushPlan {
        let mut plan = FlushPlan::default();
        let Some(queue) = self.outbox.remove(peer) else {
            return plan;
        };
        let mut remaining: Vec<QueuedMessage> = Vec::new();

        for mut message in queue {
            if message.is_expired(now_unix_secs) {
                plan.dropped.push(DroppedMessage {
                    message_id: message.message_id,
                    peer: peer.clone(),
                    reason: DropReason::Expired,
                });
                continue;
            }

            match link {
                LinkState::Secure => {
                    // The only signal strong enough to stop retaining.
                    plan.send.push(message);
                }
                LinkState::Insecure => {
                    // Sent over a live link, so no attempt is charged. See the
                    // variant's documentation — this is the rule that keeps a
                    // flapping handshake from destroying the retained copy.
                    plan.send.push(message.clone());
                    remaining.push(message);
                }
                LinkState::Reachable => {
                    if message.send_attempts >= MAX_SEND_ATTEMPTS {
                        plan.dropped.push(DroppedMessage {
                            message_id: message.message_id,
                            peer: peer.clone(),
                            reason: DropReason::AttemptsExhausted,
                        });
                        continue;
                    }
                    plan.send.push(message.clone());
                    message.send_attempts = message.send_attempts.saturating_add(1);
                    remaining.push(message);
                }
                LinkState::Unreachable => remaining.push(message),
            }
        }

        if !remaining.is_empty() {
            self.outbox.insert(peer.clone(), remaining);
        }
        // A drop always changes the queue. So does a send, but only under the two
        // link states that alter retained state: `Secure` stops retaining and
        // `Reachable` charges an attempt. `Insecure` and `Unreachable` leave the
        // queue byte-identical, and a flush over them owes storage nothing.
        let mutated = !plan.dropped.is_empty()
            || (!plan.send.is_empty()
                && matches!(link, LinkState::Secure | LinkState::Reachable));
        if mutated {
            self.dirty = true;
        }
        plan
    }

    /// Drop everything past its TTL, across every peer.
    #[must_use = "expired messages must be surfaced or they stay on 'sending' forever"]
    pub fn cleanup_expired(&mut self, now_unix_secs: u64) -> Vec<DroppedMessage> {
        let mut dropped = Vec::new();
        self.outbox.retain(|peer, queue| {
            queue.retain(|message| {
                if message.is_expired(now_unix_secs) {
                    dropped.push(DroppedMessage {
                        message_id: message.message_id.clone(),
                        peer: peer.clone(),
                        reason: DropReason::Expired,
                    });
                    false
                } else {
                    true
                }
            });
            !queue.is_empty()
        });
        if !dropped.is_empty() {
            self.dirty = true;
        }
        dropped
    }

    /// Fold a recovered snapshot into memory.
    ///
    /// This is bitchat's `mergeRecoveredOutbox`, and every part of it is load
    /// bearing:
    ///
    /// - **Merge by message ID, never replace.** The recovered copy wins for
    ///   content, but `send_attempts` takes the *maximum* of the two and the
    ///   courier key sets are *unioned* — otherwise the recovered copy re-burns
    ///   courier slots and forgets attempts already spent.
    /// - **Sort by timestamp**, so the FIFO cap evicts by age rather than by
    ///   whichever half of the merge happened to be iterated first.
    /// - **Then apply the cap**, dropping the oldest and reporting them.
    /// - Messages already acked this session are not resurrected, even though the
    ///   durable blob predates the ack.
    #[must_use = "overflow drops must be surfaced or they stay on 'sending' forever"]
    pub fn merge_recovered(&mut self, recovered: Snapshot) -> Vec<DroppedMessage> {
        let mut dropped = Vec::new();
        let mut changed = false;

        for (peer, recovered_queue) in recovered {
            let mut queue = self.outbox.remove(&peer).unwrap_or_default();
            // The timestamp sort below can reorder a queue even when the merge
            // added nothing, and order is what the FIFO cap evicts by, so the
            // dirty flag is decided by comparing the whole queue rather than by
            // watching individual insertions.
            let before = queue.clone();

            for mut candidate in recovered_queue {
                if self.acks.contains(&candidate.message_id) {
                    continue;
                }
                match queue
                    .iter()
                    .position(|m| m.message_id == candidate.message_id)
                {
                    Some(index) => {
                        if let Some(existing) = queue.get(index) {
                            candidate.send_attempts =
                                candidate.send_attempts.max(existing.send_attempts);
                            candidate
                                .deposited_courier_keys
                                .extend(existing.deposited_courier_keys.iter().copied());
                        }
                        if let Some(slot) = queue.get_mut(index) {
                            *slot = candidate;
                        }
                    }
                    None => queue.push(candidate),
                }
            }

            queue.sort_by_key(|m| m.timestamp);

            if queue.len() > MAX_MESSAGES_PER_PEER {
                let overflow = queue.len() - MAX_MESSAGES_PER_PEER;
                for evicted in queue.drain(..overflow) {
                    dropped.push(DroppedMessage {
                        message_id: evicted.message_id,
                        peer: peer.clone(),
                        reason: DropReason::QueueOverflow,
                    });
                }
            }

            if queue != before {
                changed = true;
            }
            if !queue.is_empty() {
                self.outbox.insert(peer, queue);
            }
        }

        if changed {
            self.dirty = true;
        }
        dropped
    }

    // -------------------------------------------------------------- durability

    /// Read the durable outbox and merge it into memory.
    ///
    /// Safe and useful to call more than once. The first call is the cold load; a
    /// later one is the recovery pass after storage becomes readable, and it merges
    /// rather than replaces so nothing accepted in between is lost. A pending write
    /// that was deferred because the store had not loaded is flushed here.
    ///
    /// Returns the overflow drops the merge caused, which the caller must surface.
    pub fn load(&mut self) -> Result<Vec<DroppedMessage>, StoreError> {
        let sealed = self.storage.load(StorageKey::Outbox)?;

        let dropped = match sealed {
            // Nothing durable — a first run, or a run after a panic wipe.
            None => Vec::new(),
            Some(sealed) => match self.sealer.open(&sealed) {
                Ok(plaintext) => self.merge_recovered(decode(&plaintext)?),
                // Unrecoverable: the blob can never be read, so treat it as absent
                // and let the next write replace it.
                Err(StoreError::Malformed(_)) => Vec::new(),
                // Anything else means "not readable yet". Stay un-loaded so we
                // never overwrite mail we have not seen.
                Err(error) => return Err(error),
            },
        };

        self.loaded = true;
        if self.dirty {
            self.persist()?;
        }
        Ok(dropped)
    }

    /// Write the outbox, sealed.
    ///
    /// A no-op when nothing changed. Deliberately also a no-op while the store has
    /// not loaded: writing then would overwrite a durable queue that has never been
    /// read. The pending write happens on the next successful [`load`](Self::load).
    pub fn persist(&mut self) -> Result<(), StoreError> {
        if !self.dirty {
            return Ok(());
        }
        if !self.loaded {
            // Stay dirty; the write is owed, not lost.
            return Ok(());
        }

        if self.outbox.is_empty() {
            self.storage.wipe(StorageKey::Outbox)?;
        } else {
            let plaintext = encode(&self.outbox)?;
            let sealed = self.sealer.seal(&plaintext)?;
            self.storage.save(StorageKey::Outbox, &sealed)?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Panic wipe: forget the queued mail in memory and on disk.
    ///
    /// The ack memory goes too — it exists only to protect the queue it describes,
    /// and keeping a list of recently acked message IDs after a wipe would be
    /// exactly the kind of residue the wipe is for.
    pub fn wipe(&mut self) -> Result<(), StoreError> {
        self.outbox.clear();
        self.acks.clear();
        self.dirty = false;
        // A wipe leaves nothing to recover, so the store is trivially "loaded".
        self.loaded = true;
        self.storage.wipe(StorageKey::Outbox)
    }
}

// --------------------------------------------------------------------- codec

fn encode_error(field: &'static str) -> StoreError {
    StoreError::Storage {
        operation: "encode",
        key: StorageKey::Outbox.as_str(),
        reason: format!("{field} exceeds the encodable length"),
    }
}

fn put_u16_len(out: &mut Vec<u8>, len: usize, field: &'static str) -> Result<(), StoreError> {
    let len = u16::try_from(len).map_err(|_| encode_error(field))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn put_u32_len(out: &mut Vec<u8>, len: usize, field: &'static str) -> Result<(), StoreError> {
    let len = u32::try_from(len).map_err(|_| encode_error(field))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn put_short_bytes(out: &mut Vec<u8>, bytes: &[u8], field: &'static str) -> Result<(), StoreError> {
    put_u16_len(out, bytes.len(), field)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_long_bytes(out: &mut Vec<u8>, bytes: &[u8], field: &'static str) -> Result<(), StoreError> {
    put_u32_len(out, bytes.len(), field)?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Serialize the outbox. The result is plaintext and must be sealed before it
/// reaches [`Storage`] — [`OutboxStore::persist`] is the only caller, and it does.
fn encode(snapshot: &Snapshot) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    put_u32_len(&mut out, snapshot.len(), "peer count")?;

    for (peer, queue) in snapshot {
        put_short_bytes(&mut out, peer.as_str().as_bytes(), "peer id")?;
        put_u32_len(&mut out, queue.len(), "queue length")?;
        for message in queue {
            put_long_bytes(&mut out, message.content.as_bytes(), "content")?;
            put_short_bytes(&mut out, message.nickname.as_bytes(), "nickname")?;
            put_short_bytes(&mut out, message.message_id.as_bytes(), "message id")?;
            out.extend_from_slice(&message.timestamp.to_be_bytes());
            out.extend_from_slice(&message.send_attempts.to_be_bytes());
            put_u16_len(
                &mut out,
                message.deposited_courier_keys.len(),
                "courier key count",
            )?;
            for key in &message.deposited_courier_keys {
                out.extend_from_slice(key);
            }
        }
    }
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StoreError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(StoreError::Malformed("outbox"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(StoreError::Malformed("outbox"))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StoreError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(StoreError::Malformed("outbox"))
    }

    fn u16(&mut self) -> Result<u16, StoreError> {
        let raw: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u16::from_be_bytes(raw))
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u32::from_be_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, StoreError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u64::from_be_bytes(raw))
    }

    fn key32(&mut self) -> Result<[u8; 32], StoreError> {
        self.take(32)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))
    }

    fn short_string(&mut self) -> Result<String, StoreError> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| StoreError::Malformed("outbox"))
    }

    fn long_string(&mut self) -> Result<String, StoreError> {
        let len = usize::try_from(self.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| StoreError::Malformed("outbox"))
    }
}

/// Parse bytes produced by [`encode`]. Counts are attacker-influenced in principle,
/// so nothing is pre-allocated from them: a truncated blob fails on the next read
/// instead of reserving gigabytes first.
fn decode(bytes: &[u8]) -> Result<Snapshot, StoreError> {
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC.as_slice() {
        return Err(StoreError::Malformed("outbox"));
    }
    if reader.u8()? != FORMAT_VERSION {
        return Err(StoreError::Malformed("outbox"));
    }

    let peer_count = usize::try_from(reader.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
    let mut snapshot = Snapshot::new();

    for _ in 0..peer_count {
        let peer = PeerId::from_str(&reader.short_string()?);
        let queue_len =
            usize::try_from(reader.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
        let mut queue = Vec::new();

        for _ in 0..queue_len {
            let content = reader.long_string()?;
            let nickname = reader.short_string()?;
            let message_id = reader.short_string()?;
            let timestamp = reader.u64()?;
            let send_attempts = reader.u32()?;
            let key_count = usize::from(reader.u16()?);
            let mut deposited_courier_keys = BTreeSet::new();
            for _ in 0..key_count {
                deposited_courier_keys.insert(reader.key32()?);
            }
            queue.push(QueuedMessage {
                content,
                nickname,
                message_id,
                timestamp,
                send_attempts,
                deposited_courier_keys,
            });
        }

        if !queue.is_empty() {
            snapshot.insert(peer, queue);
        }
    }

    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A real (if trivial) seal: the stored bytes are not the serialized ones, and
    /// `open` rejects anything this did not produce. Deliberately not an identity
    /// transform — an identity sealer in the tree would be exactly the plaintext-
    /// to-disk path this crate promises not to have.
    struct TestSealer {
        pad: u8,
    }

    impl TestSealer {
        fn new() -> Self {
            Self { pad: 0x5a }
        }
    }

    impl Sealer for TestSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            let mut out = Vec::with_capacity(plaintext.len() + 1);
            out.push(self.pad);
            out.extend(plaintext.iter().map(|b| b ^ self.pad));
            Ok(out)
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            let (tag, body) = sealed.split_first().ok_or(StoreError::Malformed("outbox"))?;
            if *tag != self.pad {
                return Err(StoreError::Malformed("outbox"));
            }
            Ok(body.iter().map(|b| b ^ self.pad).collect())
        }
    }

    /// A sealer whose key is not available yet — the iOS-before-first-unlock case.
    /// Anything other than `Malformed` means "try again later".
    struct LockedSealer;

    impl Sealer for LockedSealer {
        fn seal(&self, _plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            Err(StoreError::StoragePoisoned)
        }
        fn open(&self, _sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            Err(StoreError::StoragePoisoned)
        }
    }

    /// A sealer that starts locked and becomes readable later, so one store
    /// instance can live through the whole cold-launch recovery.
    struct UnlockableSealer {
        inner: TestSealer,
        unlocked: AtomicBool,
    }

    impl UnlockableSealer {
        fn locked() -> Self {
            Self {
                inner: TestSealer::new(),
                unlocked: AtomicBool::new(false),
            }
        }
        fn unlock(&self) {
            self.unlocked.store(true, Ordering::SeqCst);
        }
        fn available(&self) -> bool {
            self.unlocked.load(Ordering::SeqCst)
        }
    }

    impl Sealer for UnlockableSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            if self.available() {
                self.inner.seal(plaintext)
            } else {
                Err(StoreError::StoragePoisoned)
            }
        }
        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            if self.available() {
                self.inner.open(sealed)
            } else {
                Err(StoreError::StoragePoisoned)
            }
        }
    }

    /// Storage that reports every load as absent, whatever was written. Models a
    /// backend that cannot read yet but happily accepts writes.
    struct BlindStorage {
        inner: MemoryStorage,
        writes: Mutex<usize>,
    }

    impl BlindStorage {
        fn new() -> Self {
            Self {
                inner: MemoryStorage::new(),
                writes: Mutex::new(0),
            }
        }
        fn writes(&self) -> usize {
            *self.writes.lock().expect("lock")
        }
    }

    impl Storage for BlindStorage {
        fn load(&self, _key: StorageKey) -> Result<Option<Vec<u8>>, StoreError> {
            Err(StoreError::StoragePoisoned)
        }
        fn save(&self, key: StorageKey, sealed: &[u8]) -> Result<(), StoreError> {
            *self.writes.lock().expect("lock") += 1;
            self.inner.save(key, sealed)
        }
        fn wipe(&self, key: StorageKey) -> Result<(), StoreError> {
            *self.writes.lock().expect("lock") += 1;
            self.inner.wipe(key)
        }
    }

    fn peer(name: &str) -> PeerId {
        PeerId::from_str(name)
    }

    fn new_store() -> (Arc<MemoryStorage>, OutboxStore) {
        let storage = Arc::new(MemoryStorage::new());
        let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
        store.load().expect("cold load");
        (storage, store)
    }

    fn message(id: &str, timestamp: u64) -> QueuedMessage {
        QueuedMessage::new(format!("content-{id}"), "nick", id, timestamp)
    }

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    // ------------------------------------------------------------------ basics

    #[test]
    fn enqueue_retains_in_order() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());
        assert!(store.enqueue(&p, message("m2", 200)).is_none());

        let queue = store.queue(&p);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.first().map(|m| m.message_id.as_str()), Some("m1"));
        assert_eq!(queue.get(1).map(|m| m.message_id.as_str()), Some("m2"));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn queued_finds_only_that_peers_mail() {
        let (_storage, mut store) = new_store();
        let a = peer("aa");
        let b = peer("bb");
        assert!(store.enqueue(&a, message("m1", 100)).is_none());

        assert!(store.queued(&a, "m1").is_some());
        assert!(store.queued(&b, "m1").is_none());
        assert!(store.queued(&a, "nope").is_none());
    }

    // --------------------------------------------------------------- TTL edges

    #[test]
    fn ttl_boundary_is_exact() {
        let sent = 1_000_000u64;
        for (age, expired) in [
            (MESSAGE_TTL_SECS - 1, false),
            (MESSAGE_TTL_SECS, false),
            (MESSAGE_TTL_SECS + 1, true),
        ] {
            let (_storage, mut store) = new_store();
            let p = peer("aa");
            assert!(store.enqueue(&p, message("m1", sent)).is_none());

            let dropped = store.cleanup_expired(sent + age);
            assert_eq!(
                !dropped.is_empty(),
                expired,
                "age {age}s should{} expire",
                if expired { "" } else { " not" }
            );
            assert_eq!(store.queue(&p).is_empty(), expired);
        }
    }

    #[test]
    fn expiry_reports_the_reason_and_the_peer() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 0)).is_none());

        let dropped = store.cleanup_expired(MESSAGE_TTL_SECS + 1);
        assert_eq!(dropped.len(), 1);
        let drop = dropped.first().expect("one drop");
        assert_eq!(drop.message_id, "m1");
        assert_eq!(drop.peer, p);
        assert_eq!(drop.reason, DropReason::Expired);
    }

    #[test]
    fn a_backwards_clock_does_not_expire_everything() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 5_000)).is_none());

        assert!(store.cleanup_expired(1_000).is_empty());
        assert_eq!(store.queue(&p).len(), 1);
    }

    #[test]
    fn flush_drops_expired_before_looking_at_the_link() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("old", 0)).is_none());
        assert!(
            store
                .enqueue(&p, message("new", MESSAGE_TTL_SECS))
                .is_none()
        );

        let plan = store.flush(&p, LinkState::Secure, MESSAGE_TTL_SECS + 1);
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(
            plan.dropped.first().map(|d| d.message_id.as_str()),
            Some("old")
        );
        assert_eq!(plan.send.len(), 1);
        assert_eq!(plan.send.first().map(|m| m.message_id.as_str()), Some("new"));
    }

    // ---------------------------------------------------------- per-peer cap

    #[test]
    fn per_peer_cap_admits_exactly_one_hundred() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        for i in 0..MAX_MESSAGES_PER_PEER {
            let id = format!("m{i}");
            assert!(
                store.enqueue(&p, message(&id, i as u64)).is_none(),
                "message {i} must not evict anything"
            );
        }
        assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
    }

    #[test]
    fn message_one_hundred_and_one_evicts_the_oldest_and_reports_it() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        for i in 0..MAX_MESSAGES_PER_PEER {
            let id = format!("m{i}");
            assert!(store.enqueue(&p, message(&id, i as u64)).is_none());
        }

        let evicted = store
            .enqueue(&p, message("overflow", 999_999))
            .expect("the 101st message must evict the oldest");
        assert_eq!(evicted.message_id, "m0");
        assert_eq!(evicted.peer, p);
        assert_eq!(evicted.reason, DropReason::QueueOverflow);
        assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
        assert!(store.queued(&p, "m0").is_none());
        assert!(store.queued(&p, "overflow").is_some());
    }

    // ------------------------------------------------------------ attempt cap

    #[test]
    fn attempt_cap_boundary_is_exact() {
        // 7 attempts spent: one more send is allowed, and it becomes the 8th.
        // 8 and 9: the cap is reached and the message is given up on.
        for (spent, sends) in [
            (MAX_SEND_ATTEMPTS - 1, true),
            (MAX_SEND_ATTEMPTS, false),
            (MAX_SEND_ATTEMPTS + 1, false),
        ] {
            let (_storage, mut store) = new_store();
            let p = peer("aa");
            assert!(
                store
                    .enqueue(&p, message("m1", 100).with_send_attempts(spent))
                    .is_none()
            );

            let plan = store.flush(&p, LinkState::Reachable, 200);
            if sends {
                assert_eq!(plan.send.len(), 1, "{spent} attempts should still send");
                assert!(plan.dropped.is_empty());
                assert_eq!(
                    store.queued(&p, "m1").map(|m| m.send_attempts),
                    Some(MAX_SEND_ATTEMPTS),
                    "the send must be charged"
                );
            } else {
                assert!(plan.send.is_empty(), "{spent} attempts must not send");
                assert_eq!(plan.dropped.len(), 1);
                assert_eq!(
                    plan.dropped.first().map(|d| d.reason),
                    Some(DropReason::AttemptsExhausted)
                );
                assert!(store.queue(&p).is_empty());
            }
        }
    }

    #[test]
    fn reachable_flushes_walk_the_cap_to_exhaustion() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 0)).is_none());

        for attempt in 0..MAX_SEND_ATTEMPTS {
            let plan = store.flush(&p, LinkState::Reachable, 100);
            assert_eq!(plan.send.len(), 1, "attempt {attempt} should send");
            assert!(plan.dropped.is_empty());
        }
        let plan = store.flush(&p, LinkState::Reachable, 100);
        assert!(plan.send.is_empty());
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(
            plan.dropped.first().map(|d| d.reason),
            Some(DropReason::AttemptsExhausted)
        );
    }

    // --------------------------------------------------- secure vs insecure

    #[test]
    fn a_secure_flush_sends_and_stops_retaining() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());

        let plan = store.flush(&p, LinkState::Secure, 200);
        assert_eq!(plan.send.len(), 1);
        assert!(plan.dropped.is_empty());
        assert!(
            store.queue(&p).is_empty(),
            "a secure link is the one signal strong enough to stop retaining"
        );
    }

    #[test]
    fn an_insecure_flush_sends_retains_and_charges_nothing() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());

        let plan = store.flush(&p, LinkState::Insecure, 200);
        assert_eq!(plan.send.len(), 1);
        assert!(plan.dropped.is_empty());
        assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));
    }

    #[test]
    fn reconnect_flapping_without_a_session_never_burns_the_message() {
        // The whole point of the insecure rule: a handshake that keeps stalling
        // must not consume the retention that exists to survive it. Far more
        // flushes than the attempt cap, and the copy is still there.
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 0)).is_none());

        for _ in 0..(MAX_SEND_ATTEMPTS * 5) {
            let plan = store.flush(&p, LinkState::Insecure, 100);
            assert_eq!(plan.send.len(), 1);
            assert!(plan.dropped.is_empty());
        }
        assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));

        // Retention is still bounded — by the TTL, not by the attempt cap.
        let dropped = store.cleanup_expired(MESSAGE_TTL_SECS + 1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped.first().map(|d| d.reason), Some(DropReason::Expired));
    }

    #[test]
    fn insecure_and_reachable_differ_only_in_the_attempt_charge() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 0)).is_none());
        assert!(store.enqueue(&peer("bb"), message("m2", 0)).is_none());

        let insecure = store.flush(&p, LinkState::Insecure, 100);
        let reachable = store.flush(&peer("bb"), LinkState::Reachable, 100);
        assert_eq!(insecure.send.len(), reachable.send.len());
        assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));
        assert_eq!(
            store.queued(&peer("bb"), "m2").map(|m| m.send_attempts),
            Some(1)
        );
    }

    #[test]
    fn an_unreachable_flush_changes_nothing() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(
            store
                .enqueue(&p, message("m1", 100).with_send_attempts(3))
                .is_none()
        );

        let plan = store.flush(&p, LinkState::Unreachable, 200);
        assert!(plan.is_empty());
        assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(3));
    }

    #[test]
    fn flushing_an_unknown_peer_is_a_no_op() {
        let (_storage, mut store) = new_store();
        let plan = store.flush(&peer("nobody"), LinkState::Secure, 100);
        assert!(plan.is_empty());
    }

    // ------------------------------------------------------------------- acks

    #[test]
    fn a_delivery_ack_clears_the_message() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());

        let outcome = store.mark_acked("m1", AckKind::Delivered);
        assert!(outcome.cleared);
        assert_eq!(outcome.strongest, AckKind::Delivered);
        assert!(store.is_empty(), "the peer entry goes when its queue empties");
    }

    #[test]
    fn a_read_ack_clears_the_message() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());

        let outcome = store.mark_acked("m1", AckKind::Read);
        assert!(outcome.cleared);
        assert_eq!(outcome.strongest, AckKind::Read);
        assert!(store.queue(&p).is_empty());
    }

    #[test]
    fn a_late_delivery_ack_never_downgrades_a_read_ack() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());

        assert_eq!(store.mark_acked("m1", AckKind::Read).strongest, AckKind::Read);
        let late = store.mark_acked("m1", AckKind::Delivered);
        assert!(!late.cleared, "there was nothing left to clear");
        assert_eq!(
            late.strongest,
            AckKind::Read,
            "a delivery ack must never walk a read ack backwards"
        );
        assert_eq!(store.ack_state("m1"), Some(AckKind::Read));
    }

    #[test]
    fn a_read_ack_upgrades_an_earlier_delivery_ack() {
        let (_storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());

        assert_eq!(
            store.mark_acked("m1", AckKind::Delivered).strongest,
            AckKind::Delivered
        );
        assert_eq!(store.mark_acked("m1", AckKind::Read).strongest, AckKind::Read);
    }

    #[test]
    fn an_ack_for_unseen_mail_is_still_remembered() {
        // The ack can arrive while the durable queue is unreadable. Recording it
        // is what stops the recovery merge resurrecting the message.
        let (_storage, mut store) = new_store();
        let outcome = store.mark_acked("ghost", AckKind::Delivered);
        assert!(!outcome.cleared);
        assert_eq!(store.ack_state("ghost"), Some(AckKind::Delivered));
    }

    #[test]
    fn an_ack_clears_the_message_whichever_peer_holds_it() {
        let (_storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        assert!(store.enqueue(&peer("bb"), message("m2", 100)).is_none());

        assert!(store.mark_acked("m2", AckKind::Delivered).cleared);
        assert_eq!(store.queue(&peer("aa")).len(), 1);
        assert!(store.queue(&peer("bb")).is_empty());
    }

    #[test]
    fn ack_memory_stays_bounded() {
        let (_storage, mut store) = new_store();
        for i in 0..(ACK_MEMORY_LIMIT + 10) {
            let _ = store.mark_acked(&format!("m{i}"), AckKind::Delivered);
        }
        assert_eq!(store.ack_state("m0"), None, "the oldest acks are forgotten");
        assert_eq!(
            store.ack_state(&format!("m{}", ACK_MEMORY_LIMIT + 9)),
            Some(AckKind::Delivered)
        );
    }

    // -------------------------------------------------------- courier keys

    #[test]
    fn re_enqueue_preserves_deposited_courier_keys() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());
        store.record_courier_deposit(&p, "m1", &[key(1), key(2)]);

        // The router re-sends the same ID with a fresh attempt count.
        assert!(
            store
                .enqueue(&p, message("m1", 150).with_send_attempts(1))
                .is_none()
        );

        let queued = store.queued(&p, "m1").expect("still queued");
        assert_eq!(
            queued.deposited_courier_keys.len(),
            2,
            "the replacement must not re-burn courier slots already spent"
        );
        assert!(queued.is_carried_by(&key(1)));
        assert!(queued.is_carried_by(&key(2)));
        assert_eq!(queued.send_attempts, 1);
        assert_eq!(queued.timestamp, 150);
        assert_eq!(store.queue(&p).len(), 1, "the ID is replaced, not duplicated");
    }

    #[test]
    fn re_enqueue_moves_the_message_to_the_back() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());
        assert!(store.enqueue(&p, message("m2", 200)).is_none());
        assert!(store.enqueue(&p, message("m1", 300)).is_none());

        let ids: Vec<&str> = store
            .queue(&p)
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(ids, vec!["m2", "m1"]);
    }

    #[test]
    fn courier_deposits_union_and_track_remaining_slots() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());
        assert_eq!(
            store.queued(&p, "m1").map(QueuedMessage::remaining_courier_slots),
            Some(MAX_COURIERS_PER_MESSAGE)
        );

        store.record_courier_deposit(&p, "m1", &[key(1)]);
        store.record_courier_deposit(&p, "m1", &[key(1), key(2)]);
        let queued = store.queued(&p, "m1").expect("queued");
        assert_eq!(queued.deposited_courier_keys.len(), 2);
        assert_eq!(
            queued.remaining_courier_slots(),
            MAX_COURIERS_PER_MESSAGE - 2
        );

        store.record_courier_deposit(&p, "m1", &[key(3)]);
        let queued = store.queued(&p, "m1").expect("queued");
        assert_eq!(queued.remaining_courier_slots(), 0);
        assert!(!queued.is_carried_by(&key(9)));
    }

    #[test]
    fn a_deposit_for_acked_mail_is_ignored() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", 100)).is_none());
        assert!(store.mark_acked("m1", AckKind::Delivered).cleared);

        store.record_courier_deposit(&p, "m1", &[key(1)]);
        assert!(store.queued(&p, "m1").is_none());
    }

    // ------------------------------------------------------- recovery merge

    #[test]
    fn recovery_merges_rather_than_replaces() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        // Accepted during the locked window.
        assert!(store.enqueue(&p, message("local", 200)).is_none());

        let mut durable = Snapshot::new();
        durable.insert(p.clone(), vec![message("durable", 100)]);
        assert!(store.merge_recovered(durable).is_empty());

        let ids: Vec<&str> = store
            .queue(&p)
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["durable", "local"],
            "both survive, sorted by timestamp"
        );
    }

    #[test]
    fn recovery_takes_the_higher_attempt_count_and_unions_courier_keys() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        let mut local = message("m1", 100).with_send_attempts(5);
        local.deposited_courier_keys.insert(key(1));
        assert!(store.enqueue(&p, local).is_none());

        let mut recovered = message("m1", 100).with_send_attempts(2);
        recovered.deposited_courier_keys.insert(key(2));
        let mut durable = Snapshot::new();
        durable.insert(p.clone(), vec![recovered]);
        assert!(store.merge_recovered(durable).is_empty());

        let merged = store.queued(&p, "m1").expect("merged");
        assert_eq!(merged.send_attempts, 5, "attempts must not regress");
        assert_eq!(merged.deposited_courier_keys.len(), 2);
        assert!(merged.is_carried_by(&key(1)));
        assert!(merged.is_carried_by(&key(2)));
    }

    #[test]
    fn recovery_lets_a_higher_recovered_attempt_count_win() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(
            store
                .enqueue(&p, message("m1", 100).with_send_attempts(1))
                .is_none()
        );

        let mut durable = Snapshot::new();
        durable.insert(p.clone(), vec![message("m1", 100).with_send_attempts(6)]);
        assert!(store.merge_recovered(durable).is_empty());
        assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(6));
    }

    #[test]
    fn recovery_sorts_by_timestamp_before_applying_the_cap() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        // Newest in memory, oldest on disk: an unsorted merge would evict the
        // wrong half.
        for i in 0..MAX_MESSAGES_PER_PEER {
            let id = format!("new{i}");
            assert!(
                store
                    .enqueue(&p, message(&id, 10_000 + i as u64))
                    .is_none()
            );
        }
        let durable_queue: Vec<QueuedMessage> = (0..5)
            .map(|i| message(&format!("old{i}"), i as u64))
            .collect();
        let mut durable = Snapshot::new();
        durable.insert(p.clone(), durable_queue);

        let dropped = store.merge_recovered(durable);
        assert_eq!(dropped.len(), 5, "the overflow is the five oldest");
        let dropped_ids: Vec<&str> = dropped.iter().map(|d| d.message_id.as_str()).collect();
        assert_eq!(dropped_ids, vec!["old0", "old1", "old2", "old3", "old4"]);
        assert!(
            dropped.iter().all(|d| d.reason == DropReason::QueueOverflow),
            "the UI has to be able to fail these"
        );
        assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
        assert!(store.queued(&p, "new0").is_some());
    }

    #[test]
    fn recovery_does_not_resurrect_acked_mail() {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        // The ack landed while the durable queue was still unreadable.
        let outcome = store.mark_acked("m1", AckKind::Read);
        assert!(!outcome.cleared);

        let mut durable = Snapshot::new();
        durable.insert(p.clone(), vec![message("m1", 100), message("m2", 100)]);
        assert!(store.merge_recovered(durable).is_empty());

        assert!(store.queued(&p, "m1").is_none(), "the ack is a tombstone");
        assert!(store.queued(&p, "m2").is_some());
    }

    #[test]
    fn recovery_leaves_untouched_peers_alone() {
        let (_storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());

        let mut durable = Snapshot::new();
        durable.insert(peer("bb"), vec![message("m2", 100)]);
        assert!(store.merge_recovered(durable).is_empty());

        assert_eq!(store.queue(&peer("aa")).len(), 1);
        assert_eq!(store.queue(&peer("bb")).len(), 1);
    }

    // -------------------------------------------------------- durability

    #[test]
    fn nothing_plaintext_reaches_storage() {
        let storage = Arc::new(MemoryStorage::new());
        let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
        store.load().expect("cold load");
        let secret = "meet me at the usual place";
        assert!(
            store
                .enqueue(
                    &peer("aa"),
                    QueuedMessage::new(secret, "nick", "m1", 100)
                )
                .is_none()
        );
        store.persist().expect("persist");

        let blob = storage
            .load(StorageKey::Outbox)
            .expect("load")
            .expect("something was written");
        assert!(
            !blob
                .windows(secret.len())
                .any(|w| w == secret.as_bytes()),
            "the sealed blob must not contain message plaintext"
        );
    }

    #[test]
    fn persistence_round_trips_every_field() {
        let storage = Arc::new(MemoryStorage::new());
        let sealer = Arc::new(TestSealer::new());
        let p = peer("aa");

        let mut original = QueuedMessage::new("hello ☃", "nick ☃", "m1", 1_700_000_000);
        original.send_attempts = 4;
        original.deposited_courier_keys.insert(key(7));
        original.deposited_courier_keys.insert(key(9));

        let mut store = OutboxStore::new(storage.clone(), sealer.clone());
        store.load().expect("cold load");
        assert!(store.enqueue(&p, original.clone()).is_none());
        store.persist().expect("persist");

        let mut reloaded = OutboxStore::new(storage.clone(), sealer);
        assert!(reloaded.load().expect("reload").is_empty());
        assert_eq!(reloaded.queued(&p, "m1"), Some(&original));
    }

    #[test]
    fn persist_is_free_when_nothing_changed() {
        let (_storage, mut store) = new_store();
        assert!(!store.is_dirty());
        store.persist().expect("persist");
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        assert!(store.is_dirty());
        store.persist().expect("persist");
        assert!(!store.is_dirty());
    }

    #[test]
    fn an_emptied_outbox_wipes_its_blob() {
        let (storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        store.persist().expect("persist");
        assert!(storage.load(StorageKey::Outbox).expect("load").is_some());

        assert!(store.mark_acked("m1", AckKind::Delivered).cleared);
        store.persist().expect("persist");
        assert_eq!(
            storage.load(StorageKey::Outbox).expect("load"),
            None,
            "an empty outbox leaves nothing behind"
        );
    }

    #[test]
    fn an_unreadable_store_is_never_overwritten() {
        let storage = Arc::new(BlindStorage::new());
        let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));

        assert!(store.load().is_err(), "storage could not be read");
        assert!(!store.is_loaded());
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        store.persist().expect("persist defers rather than failing");
        assert_eq!(storage.writes(), 0, "a deferred write must not clobber");
        assert!(store.is_dirty(), "the write is owed, not lost");
    }

    #[test]
    fn a_locked_sealer_defers_instead_of_starting_empty() {
        let storage = Arc::new(MemoryStorage::new());
        // Something durable exists, but the key is not available yet.
        storage
            .save(StorageKey::Outbox, b"sealed-by-someone-else")
            .expect("save");

        let mut store = OutboxStore::new(storage.clone(), Arc::new(LockedSealer));
        assert!(store.load().is_err());
        assert!(!store.is_loaded());
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        store.persist().expect("persist defers");
        assert_eq!(
            storage.load(StorageKey::Outbox).expect("load"),
            Some(b"sealed-by-someone-else".to_vec()),
            "unreadable mail must survive the locked window"
        );
    }

    #[test]
    fn recovery_after_unlock_merges_and_flushes_the_owed_write() {
        // The whole cold-locked-launch story in one test: a durable queue exists,
        // the wake cannot read it, work is accepted anyway, and the unlock has to
        // end with both halves on disk.
        let storage = Arc::new(MemoryStorage::new());
        let p = peer("aa");

        let mut writer = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
        writer.load().expect("load");
        assert!(writer.enqueue(&p, message("durable", 100)).is_none());
        writer.persist().expect("persist");
        let durable_blob = storage.load(StorageKey::Outbox).expect("load");

        let sealer = Arc::new(UnlockableSealer::locked());
        let mut store = OutboxStore::new(storage.clone(), sealer.clone());
        assert!(store.load().is_err(), "the key is not available yet");
        assert!(store.enqueue(&p, message("local", 200)).is_none());
        store.persist().expect("a deferred write is not a failure");
        assert_eq!(
            storage.load(StorageKey::Outbox).expect("load"),
            durable_blob,
            "the unreadable blob must not be overwritten"
        );

        sealer.unlock();
        assert!(store.load().expect("recovery load").is_empty());
        assert!(store.is_loaded());
        assert!(!store.is_dirty(), "the owed write lands with the recovery");

        let mut verifier = OutboxStore::new(storage, Arc::new(TestSealer::new()));
        assert!(verifier.load().expect("load").is_empty());
        let ids: Vec<&str> = verifier
            .queue(&p)
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["durable", "local"],
            "recovery merges; replacing would discard the locked-window work"
        );
    }

    #[test]
    fn load_merges_when_called_again() {
        let storage = Arc::new(MemoryStorage::new());
        let sealer = Arc::new(TestSealer::new());
        let p = peer("aa");

        let mut writer = OutboxStore::new(storage.clone(), sealer.clone());
        writer.load().expect("load");
        assert!(writer.enqueue(&p, message("durable", 100)).is_none());
        writer.persist().expect("persist");

        let mut store = OutboxStore::new(storage.clone(), sealer);
        assert!(store.load().expect("first load").is_empty());
        assert_eq!(store.queue(&p).len(), 1);
        // A second load must be idempotent, not a duplicator.
        assert!(store.load().expect("second load").is_empty());
        assert_eq!(store.queue(&p).len(), 1);
    }

    #[test]
    fn a_corrupt_blob_starts_empty_and_is_replaceable() {
        let storage = Arc::new(MemoryStorage::new());
        storage
            .save(StorageKey::Outbox, b"\x00not sealed by us")
            .expect("save");

        let mut store = OutboxStore::new(storage.clone(), Arc::new(TestSealer::new()));
        assert!(
            store.load().expect("a corrupt blob is not fatal").is_empty(),
            "losing retained mail is bad; refusing to start is worse"
        );
        assert!(store.is_loaded());

        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        store.persist().expect("persist");
        let mut reloaded = OutboxStore::new(storage, Arc::new(TestSealer::new()));
        assert!(reloaded.load().expect("load").is_empty());
        assert_eq!(reloaded.queue(&peer("aa")).len(), 1);
    }

    #[test]
    fn a_truncated_blob_is_malformed_not_a_panic() {
        let full = encode(&{
            let mut snapshot = Snapshot::new();
            snapshot.insert(peer("aa"), vec![message("m1", 100)]);
            snapshot
        })
        .expect("encode");

        for cut in 0..full.len() {
            let truncated = full.get(..cut).expect("in range");
            assert!(
                decode(truncated).is_err(),
                "a blob cut at {cut} must be rejected"
            );
        }
        assert!(decode(&full).is_ok());
    }

    #[test]
    fn a_foreign_blob_is_rejected() {
        assert!(decode(b"XXX\x01").is_err(), "wrong magic");
        assert!(decode(b"OBX\x02").is_err(), "wrong version");
    }

    #[test]
    fn wipe_leaves_nothing_behind() {
        let (storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        store.persist().expect("persist");
        assert!(store.mark_acked("m1", AckKind::Read).cleared);
        assert!(store.enqueue(&peer("aa"), message("m2", 100)).is_none());

        store.wipe().expect("wipe");
        assert!(store.is_empty());
        assert_eq!(store.ack_state("m1"), None, "acked IDs are residue too");
        assert_eq!(storage.load(StorageKey::Outbox).expect("load"), None);
    }

    #[test]
    fn iteration_covers_every_peer() {
        let (_storage, mut store) = new_store();
        assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
        assert!(store.enqueue(&peer("bb"), message("m2", 100)).is_none());
        assert!(store.enqueue(&peer("bb"), message("m3", 100)).is_none());

        let mut peers = store.peers();
        peers.sort();
        assert_eq!(peers, vec![peer("aa"), peer("bb")]);
        let total: usize = store.iter().map(|(_, q)| q.len()).sum();
        assert_eq!(total, 3);
        assert_eq!(store.len(), 3);
    }
}
