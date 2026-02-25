//! Signal Protocol orchestration service.
//!
//! Bridges the gap between business logic (MessageService) and crypto primitives.
//! Handles session establishment, message encryption, and decryption.
//!
//! # Module Structure
//!
//! - `session` - Session management and encryption/decryption
//! - `sender_key` - Group sender key operations
//! - `identity` - Identity and key management

mod session;
mod sender_key;
mod identity;

use std::collections::HashMap;
use std::sync::Arc;
use libsignal_protocol::PublicKey;
use tokio::sync::Mutex;

use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Signal Service for end-to-end encryption.
///
/// Uses per-recipient locks to prevent race conditions when multiple messages
/// are encrypted concurrently to the same recipient. Without this, rapid sends
/// (e.g., message + typing indicator) can both read the same session counter,
/// resulting in duplicate counters and "old counter" errors on the receiving side.
pub struct SignalService {
    pub(crate) key_store: Arc<KeyStore>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) trust_root: PublicKey,
    /// Per-recipient encryption locks.
    /// Ensures only one encryption to a given recipient at a time,
    /// preventing counter collisions in the Double Ratchet.
    pub(crate) encrypt_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// LRU order for lock eviction (most recent at back).
    pub(crate) encrypt_lock_lru: Arc<Mutex<std::collections::VecDeque<String>>>,
}

impl Clone for SignalService {
    fn clone(&self) -> Self {
        Self {
            key_store: self.key_store.clone(),
            settings_repo: self.settings_repo.clone(),
            trust_root: self.trust_root,
            encrypt_locks: self.encrypt_locks.clone(),
            encrypt_lock_lru: self.encrypt_lock_lru.clone(),
        }
    }
}

impl SignalService {
    /// Create a new SignalService.
    pub fn new(
        key_store: Arc<KeyStore>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        let trust_root = *crate::config::TRUST_ROOT_PUBLIC_KEY;

        Self {
            key_store,
            settings_repo,
            trust_root,
            encrypt_locks: Arc::new(Mutex::new(HashMap::new())),
            encrypt_lock_lru: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use crate::storage::Database;
    use crate::crypto::SignalStore;
    use crate::storage::repositories::settings_keys;

    async fn setup_service(path: &std::path::Path) -> (SignalService, Arc<KeyStore>, Arc<SettingsRepository>) {
        let db = Arc::new(Database::open(path, "test_passphrase").unwrap());
        db.run_migrations().unwrap();

        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));

        // Initialize settings
        settings_repo.set(settings_keys::CURRENT_USER_ID, &"bob_uuid".to_string()).unwrap();
        settings_repo.set(settings_keys::CURRENT_DEVICE_ID, &1u32).unwrap();

        let service = SignalService::new(key_store.clone(), settings_repo.clone());
        (service, key_store, settings_repo)
    }

    #[tokio::test]
    async fn test_signal_service_creation() {
        let db_file = NamedTempFile::new().unwrap();
        let (service, _key_store, _settings) = setup_service(db_file.path()).await;

        // Basic test that service can be created
        assert!(service.key_store.get_local_identity().is_err() ||
                service.key_store.get_local_identity().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_signal_store_creation() {
        let db_file = NamedTempFile::new().unwrap();
        let (_service, key_store, _settings) = setup_service(db_file.path()).await;

        let store = SignalStore::new(key_store);
        // Store should be created successfully
        drop(store);
    }
}
