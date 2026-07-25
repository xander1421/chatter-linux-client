//! Local-only counters for the store-and-forward stack.
//!
//! Ported from bitchat's `Services/Courier/StoreAndForwardMetrics.swift`.
//!
//! # What these deliberately are not
//!
//! Bare event tallies. No message IDs, no peer identities, no timestamps, no
//! sequence — nothing that could reconstruct who talked to whom or when. That is
//! the entire design: delivery behaviour is worth measuring on-device, and the only
//! way to measure it without building a local traffic log is to throw away
//! everything except the count.
//!
//! They never leave the device, and they die in the panic wipe alongside the stores
//! they describe. Keep them that way — a "just add the peer ID so we can debug it"
//! change turns a privacy-safe counter into exactly the record this stack exists to
//! avoid keeping.
//!
//! # Why these persist unsealed
//!
//! [`StorageKey::Metrics`] is the one blob in this crate with nothing to protect:
//! ten integers, derived from no message content and naming nobody. There is
//! therefore no [`Sealer`](crate::outbox::Sealer) on this path, which is also why
//! adding an identifying field here would be a real leak rather than a stylistic
//! objection.
//!
//! # Recording is free; writing is not
//!
//! [`record`](StoreAndForwardMetrics::record) takes `&self` and only touches an
//! atomic, so the router can hold this behind a shared handle and count from
//! anywhere without threading a mutable borrow through the call tree. Storage is
//! touched only by [`persist`](StoreAndForwardMetrics::persist),
//! [`load`](StoreAndForwardMetrics::load) and
//! [`reset`](StoreAndForwardMetrics::reset). bitchat writes on every increment;
//! that is an acceptable cost for `UserDefaults` and a poor one for a database, so
//! the write point belongs to whoever owns the loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::error::StoreError;
use crate::storage::{Storage, StorageKey};

/// Wire tag for the serialized counters.
const MAGIC: [u8; 3] = *b"SFM";
const FORMAT_VERSION: u8 = 1;

/// A countable store-and-forward event.
///
/// The string forms are bitchat's, and they are the serialization keys, so renaming
/// one silently zeroes that counter on the next load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SfEvent {
    /// A private message entered the outbox — no prompt route was available.
    OutboxQueued,
    /// A retained message was re-sent on a flush.
    OutboxResent,
    /// A delivery or read ack cleared a retained message.
    OutboxDelivered,
    /// A retained message was given up on: attempt cap, TTL, or queue overflow.
    OutboxDropped,
    /// We handed sealed mail to a courier.
    CourierDeposited,
    /// We accepted sealed mail to carry for a third party.
    CourierAccepted,
    /// We handed carried mail to its recipient over a direct link.
    CourierHandedOver,
    /// We pushed carried mail toward a recipient heard via a relay.
    CourierRemoteHandover,
    /// We split spray copies to another courier.
    CourierSprayed,
    /// Couriered mail addressed to us was opened and delivered.
    CourierOpened,
}

impl SfEvent {
    /// Every event, in counter order.
    pub const ALL: &'static [SfEvent] = &[
        SfEvent::OutboxQueued,
        SfEvent::OutboxResent,
        SfEvent::OutboxDelivered,
        SfEvent::OutboxDropped,
        SfEvent::CourierDeposited,
        SfEvent::CourierAccepted,
        SfEvent::CourierHandedOver,
        SfEvent::CourierRemoteHandover,
        SfEvent::CourierSprayed,
        SfEvent::CourierOpened,
    ];

    /// Stable serialization key, matching bitchat's raw values.
    pub const fn as_str(self) -> &'static str {
        match self {
            SfEvent::OutboxQueued => "outbox.queued",
            SfEvent::OutboxResent => "outbox.resent",
            SfEvent::OutboxDelivered => "outbox.delivered",
            SfEvent::OutboxDropped => "outbox.dropped",
            SfEvent::CourierDeposited => "courier.deposited",
            SfEvent::CourierAccepted => "courier.accepted",
            SfEvent::CourierHandedOver => "courier.handedOver",
            SfEvent::CourierRemoteHandover => "courier.remoteHandover",
            SfEvent::CourierSprayed => "courier.sprayed",
            SfEvent::CourierOpened => "courier.opened",
        }
    }

    /// Index into the counter array.
    const fn index(self) -> usize {
        match self {
            SfEvent::OutboxQueued => 0,
            SfEvent::OutboxResent => 1,
            SfEvent::OutboxDelivered => 2,
            SfEvent::OutboxDropped => 3,
            SfEvent::CourierDeposited => 4,
            SfEvent::CourierAccepted => 5,
            SfEvent::CourierHandedOver => 6,
            SfEvent::CourierRemoteHandover => 7,
            SfEvent::CourierSprayed => 8,
            SfEvent::CourierOpened => 9,
        }
    }

    fn from_str(name: &str) -> Option<SfEvent> {
        SfEvent::ALL.iter().copied().find(|e| e.as_str() == name)
    }
}

const EVENT_COUNT: usize = 10;

/// Privacy-safe local tallies for the store-and-forward stack.
pub struct StoreAndForwardMetrics {
    storage: Arc<dyn Storage>,
    counts: [AtomicU64; EVENT_COUNT],
    dirty: AtomicBool,
}

impl StoreAndForwardMetrics {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            counts: [const { AtomicU64::new(0) }; EVENT_COUNT],
            dirty: AtomicBool::new(false),
        }
    }

    /// Count one event. Infallible, lock-free, and never touches storage.
    pub fn record(&self, event: SfEvent) {
        if let Some(counter) = self.counts.get(event.index()) {
            // Saturating so a long-lived install cannot wrap a counter back to a
            // number that reads as "this never happens".
            let mut current = counter.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_add(1);
                match counter.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// The tally for one event.
    pub fn count(&self, event: SfEvent) -> u64 {
        self.counts
            .get(event.index())
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Every tally, in [`SfEvent::ALL`] order.
    pub fn snapshot(&self) -> Vec<(SfEvent, u64)> {
        SfEvent::ALL
            .iter()
            .map(|event| (*event, self.count(*event)))
            .collect()
    }

    /// Whether anything has been counted since the last write.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Read the persisted tallies, replacing whatever is in memory.
    ///
    /// Unlike the outbox, this replaces rather than merges: a counter has no
    /// identity to reconcile, and adding a "merge" would double-count every event
    /// recorded before the load.
    ///
    /// A missing blob is a first run, and a malformed one is treated as a first run
    /// too — refusing to start over an unreadable counter file would be absurd.
    pub fn load(&self) -> Result<(), StoreError> {
        let Some(bytes) = self.storage.load(StorageKey::Metrics)? else {
            return Ok(());
        };
        let Ok(decoded) = decode(&bytes) else {
            return Ok(());
        };
        for (index, counter) in self.counts.iter().enumerate() {
            let value = SfEvent::ALL
                .get(index)
                .and_then(|event| decoded.iter().find(|(e, _)| e == event))
                .map_or(0, |(_, value)| *value);
            counter.store(value, Ordering::Relaxed);
        }
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Write the tallies. A no-op when nothing has been counted since the last one.
    pub fn persist(&self) -> Result<(), StoreError> {
        if !self.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let encoded = encode(&self.snapshot())?;
        self.storage.save(StorageKey::Metrics, &encoded)?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Panic wipe: zero the tallies and remove the blob.
    pub fn reset(&self) -> Result<(), StoreError> {
        for counter in &self.counts {
            counter.store(0, Ordering::Relaxed);
        }
        self.dirty.store(false, Ordering::Relaxed);
        self.storage.wipe(StorageKey::Metrics)
    }
}

/// Serialize as name/value pairs rather than a positional array, so reordering
/// [`SfEvent`] cannot silently reattribute one counter's history to another.
fn encode(counts: &[(SfEvent, u64)]) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    let len = u16::try_from(counts.len()).map_err(|_| StoreError::Storage {
        operation: "encode",
        key: StorageKey::Metrics.as_str(),
        reason: "too many counters".to_owned(),
    })?;
    out.extend_from_slice(&len.to_be_bytes());

    for (event, value) in counts {
        let name = event.as_str().as_bytes();
        let name_len = u8::try_from(name.len()).map_err(|_| StoreError::Storage {
            operation: "encode",
            key: StorageKey::Metrics.as_str(),
            reason: "counter name is too long".to_owned(),
        })?;
        out.push(name_len);
        out.extend_from_slice(name);
        out.extend_from_slice(&value.to_be_bytes());
    }
    Ok(out)
}

/// Parse bytes produced by [`encode`]. Names this build does not know are skipped,
/// so a blob written by a build with an extra counter still loads.
fn decode(bytes: &[u8]) -> Result<Vec<(SfEvent, u64)>, StoreError> {
    fn malformed() -> StoreError {
        StoreError::Malformed("sf-metrics")
    }

    fn take<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], StoreError> {
        let end = pos.checked_add(len).ok_or_else(malformed)?;
        let slice = bytes.get(*pos..end).ok_or_else(malformed)?;
        *pos = end;
        Ok(slice)
    }

    let mut pos = 0usize;
    if take(bytes, &mut pos, MAGIC.len())? != MAGIC.as_slice() {
        return Err(malformed());
    }
    if take(bytes, &mut pos, 1)?.first().copied() != Some(FORMAT_VERSION) {
        return Err(malformed());
    }
    let entry_count = {
        let raw: [u8; 2] = take(bytes, &mut pos, 2)?
            .try_into()
            .map_err(|_| malformed())?;
        usize::from(u16::from_be_bytes(raw))
    };

    let mut out = Vec::new();
    for _ in 0..entry_count {
        let name_len = usize::from(
            take(bytes, &mut pos, 1)?
                .first()
                .copied()
                .ok_or_else(malformed)?,
        );
        let name_bytes = take(bytes, &mut pos, name_len)?.to_vec();
        let raw: [u8; 8] = take(bytes, &mut pos, 8)?
            .try_into()
            .map_err(|_| malformed())?;
        let value = u64::from_be_bytes(raw);

        let Ok(name) = String::from_utf8(name_bytes) else {
            return Err(malformed());
        };
        if let Some(event) = SfEvent::from_str(&name) {
            out.push((event, value));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

    fn metrics() -> (Arc<MemoryStorage>, StoreAndForwardMetrics) {
        let storage = Arc::new(MemoryStorage::new());
        let metrics = StoreAndForwardMetrics::new(storage.clone());
        (storage, metrics)
    }

    #[test]
    fn counters_start_at_zero_and_count_independently() {
        let (_storage, metrics) = metrics();
        for event in SfEvent::ALL {
            assert_eq!(metrics.count(*event), 0);
        }

        metrics.record(SfEvent::OutboxQueued);
        metrics.record(SfEvent::OutboxQueued);
        metrics.record(SfEvent::CourierOpened);

        assert_eq!(metrics.count(SfEvent::OutboxQueued), 2);
        assert_eq!(metrics.count(SfEvent::CourierOpened), 1);
        assert_eq!(metrics.count(SfEvent::OutboxDropped), 0);
    }

    #[test]
    fn every_event_has_its_own_slot() {
        let (_storage, metrics) = metrics();
        for (bump, event) in SfEvent::ALL.iter().enumerate() {
            for _ in 0..=bump {
                metrics.record(*event);
            }
        }
        for (bump, event) in SfEvent::ALL.iter().enumerate() {
            assert_eq!(
                metrics.count(*event),
                bump as u64 + 1,
                "{} shares a slot with another event",
                event.as_str()
            );
        }
    }

    #[test]
    fn names_are_unique_and_indices_do_not_collide() {
        // Both are silent-corruption bugs: a duplicate name merges two counters on
        // disk, a duplicate index merges them in memory.
        let mut names = std::collections::BTreeSet::new();
        let mut indices = std::collections::BTreeSet::new();
        for event in SfEvent::ALL {
            assert!(names.insert(event.as_str()), "duplicate name");
            assert!(indices.insert(event.index()), "duplicate index");
        }
        assert_eq!(names.len(), EVENT_COUNT);
        assert_eq!(indices.len(), EVENT_COUNT);
        assert_eq!(SfEvent::ALL.len(), EVENT_COUNT);
        for event in SfEvent::ALL {
            assert!(event.index() < EVENT_COUNT);
            assert_eq!(SfEvent::from_str(event.as_str()), Some(*event));
        }
    }

    #[test]
    fn persistence_round_trips() {
        let storage = Arc::new(MemoryStorage::new());
        let metrics = StoreAndForwardMetrics::new(storage.clone());
        metrics.record(SfEvent::OutboxResent);
        metrics.record(SfEvent::OutboxResent);
        metrics.record(SfEvent::CourierSprayed);
        metrics.persist().expect("persist");

        let reloaded = StoreAndForwardMetrics::new(storage);
        reloaded.load().expect("load");
        assert_eq!(reloaded.count(SfEvent::OutboxResent), 2);
        assert_eq!(reloaded.count(SfEvent::CourierSprayed), 1);
        assert_eq!(reloaded.count(SfEvent::OutboxQueued), 0);
        assert!(!reloaded.is_dirty());
    }

    #[test]
    fn persist_is_free_when_nothing_was_counted() {
        let (storage, metrics) = metrics();
        assert!(!metrics.is_dirty());
        metrics.persist().expect("persist");
        assert_eq!(storage.load(StorageKey::Metrics).expect("load"), None);

        metrics.record(SfEvent::OutboxDropped);
        assert!(metrics.is_dirty());
        metrics.persist().expect("persist");
        assert!(!metrics.is_dirty());
        assert!(storage.load(StorageKey::Metrics).expect("load").is_some());
    }

    #[test]
    fn load_replaces_rather_than_accumulates() {
        let storage = Arc::new(MemoryStorage::new());
        let writer = StoreAndForwardMetrics::new(storage.clone());
        writer.record(SfEvent::OutboxQueued);
        writer.persist().expect("persist");

        let reader = StoreAndForwardMetrics::new(storage);
        reader.record(SfEvent::OutboxQueued);
        reader.record(SfEvent::OutboxQueued);
        reader.load().expect("load");
        assert_eq!(
            reader.count(SfEvent::OutboxQueued),
            1,
            "a load must not double-count what was already in memory"
        );
    }

    #[test]
    fn a_first_run_loads_cleanly() {
        let (_storage, metrics) = metrics();
        metrics.load().expect("a missing blob is normal");
        assert_eq!(metrics.count(SfEvent::OutboxQueued), 0);
    }

    #[test]
    fn a_malformed_blob_is_treated_as_a_first_run() {
        let storage = Arc::new(MemoryStorage::new());
        storage
            .save(StorageKey::Metrics, b"not our counters")
            .expect("save");

        let metrics = StoreAndForwardMetrics::new(storage);
        metrics.load().expect("refusing to start over counters is absurd");
        assert_eq!(metrics.count(SfEvent::OutboxQueued), 0);
    }

    #[test]
    fn a_truncated_blob_is_rejected_not_a_panic() {
        let full = encode(&[(SfEvent::OutboxQueued, 7), (SfEvent::CourierOpened, 3)])
            .expect("encode");
        for cut in 0..full.len() {
            let truncated = full.get(..cut).expect("in range");
            assert!(decode(truncated).is_err(), "a blob cut at {cut} must fail");
        }
        assert_eq!(
            decode(&full).expect("decode"),
            vec![(SfEvent::OutboxQueued, 7), (SfEvent::CourierOpened, 3)]
        );
    }

    #[test]
    fn an_unknown_counter_name_is_skipped() {
        // A blob from a build that knows an event this one does not.
        let mut blob = Vec::new();
        blob.extend_from_slice(&MAGIC);
        blob.push(FORMAT_VERSION);
        blob.extend_from_slice(&2u16.to_be_bytes());
        for (name, value) in [("courier.teleported", 9u64), ("outbox.queued", 4)] {
            blob.push(name.len() as u8);
            blob.extend_from_slice(name.as_bytes());
            blob.extend_from_slice(&value.to_be_bytes());
        }

        let decoded = decode(&blob).expect("decode");
        assert_eq!(decoded, vec![(SfEvent::OutboxQueued, 4)]);
    }

    #[test]
    fn a_foreign_blob_is_rejected() {
        assert!(decode(b"XXX\x01\x00\x00").is_err(), "wrong magic");
        assert!(decode(b"SFM\x02\x00\x00").is_err(), "wrong version");
    }

    #[test]
    fn reset_zeroes_and_wipes() {
        let (storage, metrics) = metrics();
        metrics.record(SfEvent::CourierDeposited);
        metrics.persist().expect("persist");
        assert!(storage.load(StorageKey::Metrics).expect("load").is_some());

        metrics.reset().expect("reset");
        assert_eq!(metrics.count(SfEvent::CourierDeposited), 0);
        assert!(!metrics.is_dirty());
        assert_eq!(storage.load(StorageKey::Metrics).expect("load"), None);
    }

    #[test]
    fn a_reset_store_persists_nothing_until_something_happens_again() {
        let (storage, metrics) = metrics();
        metrics.record(SfEvent::CourierAccepted);
        metrics.persist().expect("persist");
        metrics.reset().expect("reset");
        metrics.persist().expect("persist");
        assert_eq!(
            storage.load(StorageKey::Metrics).expect("load"),
            None,
            "the wipe must not be undone by a stale dirty flag"
        );
    }

    #[test]
    fn snapshot_covers_every_event_in_order() {
        let (_storage, metrics) = metrics();
        metrics.record(SfEvent::CourierHandedOver);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.len(), EVENT_COUNT);
        for (index, (event, _)) in snapshot.iter().enumerate() {
            assert_eq!(Some(event), SfEvent::ALL.get(index));
        }
        assert_eq!(
            snapshot
                .iter()
                .find(|(e, _)| *e == SfEvent::CourierHandedOver)
                .map(|(_, v)| *v),
            Some(1)
        );
    }

    #[test]
    fn counters_saturate_rather_than_wrapping() {
        let (_storage, metrics) = metrics();
        if let Some(counter) = metrics.counts.get(SfEvent::OutboxQueued.index()) {
            counter.store(u64::MAX, Ordering::Relaxed);
        }
        metrics.record(SfEvent::OutboxQueued);
        assert_eq!(metrics.count(SfEvent::OutboxQueued), u64::MAX);
    }
}
