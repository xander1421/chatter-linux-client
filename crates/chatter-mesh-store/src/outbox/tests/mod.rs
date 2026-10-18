use super::*;
use crate::storage::MemoryStorage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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
        let (tag, body) = sealed
            .split_first()
            .ok_or(StoreError::Malformed("outbox"))?;
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

mod ack;
mod durability;
mod queue;
mod recovery;
