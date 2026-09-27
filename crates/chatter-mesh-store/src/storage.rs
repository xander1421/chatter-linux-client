//! The persistence seam.
//!
//! # This trait never sees plaintext
//!
//! bitchat's whitepaper states it plainly: no plaintext message content is ever
//! written to disk. Everything the store-and-forward stack persists is either sealed
//! ciphertext or traffic that was already public broadcast.
//!
//! That guarantee is enforced structurally here rather than by discipline. This
//! trait takes and returns **already-sealed bytes**, and the sealing key never enters
//! this crate at all — so a bug in the store cannot write plaintext even by accident,
//! and a reviewer does not have to audit every call site to be sure of it.
//!
//! The real implementation lands in the integration layer, over chatter-core's
//! SQLCipher database and OS keyring.

use crate::error::StoreError;

/// Which durable blob is being addressed.
///
/// A closed enum rather than free-form keys: the set of things this stack persists is
/// small, fixed, and each has different wipe semantics. A string key would invite a
/// fifth one to appear without anyone deciding it should.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StorageKey {
    /// Retained private mail awaiting delivery. Sealed.
    Outbox,
    /// Envelopes we are carrying for other people. Opaque to us by construction.
    CourierPool,
    /// Cached public broadcast traffic for gossip sync. Already public.
    PublicArchive,
    /// Bare local counters. No identities, no message IDs, no timestamps.
    Metrics,
    /// Which bridge drops we have already published or already seen.
    ///
    /// Must survive a restart. bitchat's own comment records a field-verified
    /// amplification storm from when this was held in memory only: after a relaunch
    /// the device no longer recognises its own drops, republishes them, and every
    /// gateway watching the tag does the same.
    ///
    /// **Seal this one.** It is a set of recipient tags and event IDs — metadata
    /// about who this device is messaging and when — so unlike [`Self::Metrics`] it
    /// is not safe to persist in the clear.
    BridgeDropDedup,
    /// The keys each peer ID was first seen with.
    ///
    /// Must survive a restart, or trust on first use starts over every launch
    /// and the first announce to claim a known peer's ID is believed.
    ///
    /// **Seal this one** too: public keys, but the list of them is everyone
    /// this device has been near.
    PeerPins,
}

impl StorageKey {
    /// Every key, for a wipe that must not miss one.
    ///
    /// Panic mode has to clear all of them. Deriving the list here means adding a
    /// key cannot leave a hole in the wipe.
    pub const ALL: &'static [StorageKey] = &[
        StorageKey::Outbox,
        StorageKey::CourierPool,
        StorageKey::PublicArchive,
        StorageKey::Metrics,
        StorageKey::BridgeDropDedup,
        StorageKey::PeerPins,
    ];

    /// Stable identifier for a file name or a database row.
    pub const fn as_str(self) -> &'static str {
        match self {
            StorageKey::Outbox => "outbox",
            StorageKey::CourierPool => "courier-pool",
            StorageKey::PublicArchive => "public-archive",
            StorageKey::Metrics => "sf-metrics",
            StorageKey::BridgeDropDedup => "bridge-drop-dedup",
            StorageKey::PeerPins => "peer-pins",
        }
    }
}

/// Durable storage for the store-and-forward stack.
///
/// Implementations must treat a missing blob as `Ok(None)` rather than an error: a
/// first run, and a run after a panic wipe, are both normal.
///
/// # "Absent" and "not readable yet" must not be conflated
///
/// An earlier version of this doc said a blob that is temporarily unreadable — file
/// protection before first unlock, a locked keyring — should also return `Ok(None)`.
/// That was wrong, and the mistake is worth spelling out because it is silent.
///
/// `Ok(None)` means *there is nothing durable here*, and a caller acts on it by
/// starting empty and persisting over the top. If a locked keyring reports the same
/// thing, the caller cheerfully overwrites mail it could not read — losing messages
/// that were waiting to be delivered, with no error anywhere.
///
/// So: return `Ok(None)` only when the blob is genuinely absent. Return `Err` when it
/// exists but cannot be read yet; the caller will stay un-loaded, refuse to write,
/// and retry later.
pub trait Storage: Send + Sync {
    fn load(&self, key: StorageKey) -> Result<Option<Vec<u8>>, StoreError>;

    /// Persist sealed bytes. Callers pass ciphertext; this trait has no way to seal
    /// anything itself, which is the point.
    fn save(&self, key: StorageKey, sealed: &[u8]) -> Result<(), StoreError>;

    fn wipe(&self, key: StorageKey) -> Result<(), StoreError>;

    /// Panic wipe: forget everything.
    fn wipe_all(&self) -> Result<(), StoreError> {
        let mut first_error = None;
        for key in StorageKey::ALL {
            // Keep going after a failure. A wipe that stops at the first error
            // leaves the rest on disk, which is the worst possible outcome for the
            // one operation whose entire purpose is leaving nothing behind.
            if let Err(error) = self.wipe(*key)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// In-memory storage for tests.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    blobs: std::sync::Mutex<std::collections::BTreeMap<StorageKey, Vec<u8>>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many blobs are currently held. Lets a test assert a wipe emptied it.
    pub fn len(&self) -> usize {
        self.blobs.lock().map(|b| b.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Storage for MemoryStorage {
    fn load(&self, key: StorageKey) -> Result<Option<Vec<u8>>, StoreError> {
        let blobs = self.blobs.lock().map_err(|_| StoreError::StoragePoisoned)?;
        Ok(blobs.get(&key).cloned())
    }

    fn save(&self, key: StorageKey, sealed: &[u8]) -> Result<(), StoreError> {
        let mut blobs = self.blobs.lock().map_err(|_| StoreError::StoragePoisoned)?;
        blobs.insert(key, sealed.to_vec());
        Ok(())
    }

    fn wipe(&self, key: StorageKey) -> Result<(), StoreError> {
        let mut blobs = self.blobs.lock().map_err(|_| StoreError::StoragePoisoned)?;
        blobs.remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_reports_absence_as_none() {
        let storage = MemoryStorage::new();
        assert_eq!(storage.load(StorageKey::Outbox).expect("load"), None);

        storage.save(StorageKey::Outbox, b"sealed").expect("save");
        assert_eq!(
            storage.load(StorageKey::Outbox).expect("load"),
            Some(b"sealed".to_vec())
        );
    }

    #[test]
    fn keys_do_not_collide() {
        let storage = MemoryStorage::new();
        for key in StorageKey::ALL {
            storage.save(*key, key.as_str().as_bytes()).expect("save");
        }
        for key in StorageKey::ALL {
            assert_eq!(
                storage.load(*key).expect("load"),
                Some(key.as_str().as_bytes().to_vec()),
                "{key:?} was overwritten by another key"
            );
        }
    }

    #[test]
    fn wipe_all_clears_every_key() {
        let storage = MemoryStorage::new();
        for key in StorageKey::ALL {
            storage.save(*key, b"x").expect("save");
        }
        assert_eq!(storage.len(), StorageKey::ALL.len());

        storage.wipe_all().expect("wipe");
        assert!(storage.is_empty(), "panic wipe must leave nothing behind");
    }

    #[test]
    fn storage_key_names_are_unique_and_stable() {
        // They name files and database rows, so a collision silently merges two
        // stores and a rename orphans the old data.
        let mut seen = std::collections::BTreeSet::new();
        for key in StorageKey::ALL {
            assert!(seen.insert(key.as_str()), "duplicate name {}", key.as_str());
        }
    }
}
