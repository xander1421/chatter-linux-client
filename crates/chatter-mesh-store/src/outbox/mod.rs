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
use crate::{MAX_COURIERS_PER_MESSAGE, MAX_MESSAGES_PER_PEER, MAX_SEND_ATTEMPTS, MESSAGE_TTL_SECS};

/// How many acked message IDs are remembered, newest wins.
///
/// The memory does two jobs: it stops a cold-load merge from resurrecting a message
/// that was acked while the durable queue was unreadable, and it stops a late
/// delivery ack from downgrading a read ack. Both only matter for the recent past,
/// so the set is bounded rather than kept for the life of the process.
pub const ACK_MEMORY_LIMIT: usize = 1024;

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

mod model;
pub use model::{
    AckKind, AckOutcome, DropReason, DroppedMessage, FlushPlan, LinkState, QueuedMessage,
};

mod ack;
use ack::AckMemory;
mod codec;
mod durability;
mod queue;
mod recovery;

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
        self.queue(peer).iter().find(|m| m.message_id == message_id)
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
}

#[cfg(test)]
mod tests;
