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
pub mod actor;

use std::collections::HashMap;
use std::sync::Arc;
use libsignal_protocol::PublicKey;
use tokio::sync::Mutex;

use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;
use self::actor::SignalActorHandle;

/// Signal Service for end-to-end encryption.
pub struct SignalService {
    pub(crate) key_store: Arc<KeyStore>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) trust_root: PublicKey,
    /// Handle to the Signal Actor for thread-safe crypto operations.
    pub actor: SignalActorHandle,
    /// Per-recipient encryption locks (now managed within or by the actor logic).
    pub(crate) encrypt_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) encrypt_lock_lru: Arc<Mutex<std::collections::VecDeque<String>>>,
}

impl Clone for SignalService {
    fn clone(&self) -> Self {
        Self {
            key_store: self.key_store.clone(),
            settings_repo: self.settings_repo.clone(),
            trust_root: self.trust_root,
            actor: self.actor.clone(),
            encrypt_locks: self.encrypt_locks.clone(),
            encrypt_lock_lru: self.encrypt_lock_lru.clone(),
        }
    }
}

impl SignalService {
    /// Raw key store access, for integration tests that must observe Signal
    /// state directly (e.g. asserting the Double Ratchet advances per message).
    ///
    /// Gated on `testing` for the same reason as `client::service_accessor!` —
    /// chatter-ui must not reach past the façade, but external integration
    /// tests need real state to make falsifiable claims about it.
    #[cfg(feature = "testing")]
    pub fn key_store(&self) -> &Arc<KeyStore> {
        &self.key_store
    }

    /// Create a new SignalService.
    pub fn new(
        key_store: Arc<KeyStore>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        let trust_root = *crate::config::TRUST_ROOT_PUBLIC_KEY;
        let actor = SignalActorHandle::spawn(key_store.clone(), settings_repo.clone());

        Self {
            key_store,
            settings_repo,
            trust_root,
            actor,
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

    /// Audit C2: identity is pinned per ACCOUNT, so a server-injected "ghost"
    /// device (new address, divergent identity key) is rejected, while a
    /// legitimate second device sharing the account's identity key is trusted.
    #[tokio::test]
    async fn ghost_device_with_divergent_identity_key_is_rejected() {
        use libsignal_protocol::{
            DeviceId, Direction, IdentityKeyPair, IdentityKeyStore, ProtocolAddress,
        };
        use rand::TryRngCore;

        let db_file = NamedTempFile::new().unwrap();
        let (_service, key_store, _settings) = setup_service(db_file.path()).await;
        let mut store = SignalStore::new(key_store);

        let mut rng = rand::rngs::OsRng.unwrap_err();
        let real = *IdentityKeyPair::generate(&mut rng).identity_key();
        let attacker = *IdentityKeyPair::generate(&mut rng).identity_key();

        let dev1 = ProtocolAddress::new("alice_uuid".to_string(), DeviceId::new(1).unwrap());
        let dev2 = ProtocolAddress::new("alice_uuid".to_string(), DeviceId::new(2).unwrap());
        let dev3 = ProtocolAddress::new("alice_uuid".to_string(), DeviceId::new(3).unwrap());

        // First contact TOFU-pins the account identity (device 1, real key).
        assert!(store
            .is_trusted_identity(&dev1, &real, Direction::Receiving)
            .await
            .unwrap());
        store.save_identity(&dev1, &real).await.unwrap();

        // A second legitimate device shares the SAME identity key -> trusted,
        // even though its (user:device) address was never seen before.
        assert!(store
            .is_trusted_identity(&dev2, &real, Direction::Sending)
            .await
            .unwrap());

        // A server-injected ghost device presents a DIFFERENT key -> rejected.
        assert!(!store
            .is_trusted_identity(&dev3, &attacker, Direction::Sending)
            .await
            .unwrap());
    }
}
