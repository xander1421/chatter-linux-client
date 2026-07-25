//! Small adapters between the mesh crates' generics and this service's `Arc`s.

use std::sync::Arc;

use chatter_mesh_store::storage::{Storage, StorageKey};
use chatter_mesh_store::StoreError;

/// `Storage` for a shared handle.
///
/// [`OutboxStore`](chatter_mesh_store::outbox::OutboxStore) and friends take
/// `Arc<dyn Storage>`, but [`CourierStore`](chatter_mesh_store::courier::CourierStore)
/// is generic over an owned `S: Storage`. One shared database backs both, so
/// this delegates rather than duplicating a connection.
pub struct SharedStorage(Arc<dyn Storage>);

impl SharedStorage {
    #[must_use]
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        Self(inner)
    }
}

impl Clone for SharedStorage {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Storage for SharedStorage {
    fn load(&self, key: StorageKey) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.load(key)
    }

    fn save(&self, key: StorageKey, sealed: &[u8]) -> Result<(), StoreError> {
        self.0.save(key, sealed)
    }

    fn wipe(&self, key: StorageKey) -> Result<(), StoreError> {
        self.0.wipe(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_store::MemoryStorage;

    #[test]
    fn the_adapter_reaches_the_same_blobs() {
        let backing = Arc::new(MemoryStorage::new());
        let shared = SharedStorage::new(backing.clone() as Arc<dyn Storage>);

        shared.save(StorageKey::CourierPool, b"sealed").expect("save");
        assert_eq!(backing.len(), 1);
        assert_eq!(
            shared.load(StorageKey::CourierPool).expect("load"),
            Some(b"sealed".to_vec())
        );

        // A clone is the same store, not a copy of it.
        let twin = shared.clone();
        twin.wipe(StorageKey::CourierPool).expect("wipe");
        assert!(shared.load(StorageKey::CourierPool).expect("load").is_none());
        assert!(backing.is_empty());
    }
}
