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

    /// A peer reinstall wedges the conversation, and `accept_peer_identity_change`
    /// is the only thing that clears it.
    ///
    /// After a reinstall the peer's key is not the pinned one, so libsignal refuses
    /// every inbound PreKey message and every fresh session
    /// (`is_trusted_identity` -> false). Nothing in the protocol re-pins it:
    /// `save_identity` is only reached AFTER the trust check passes, so the wedge
    /// is permanent by construction. This asserts both halves — that the wedge is
    /// real, and that accepting clears it completely: key re-pinned, stale
    /// verification dropped (the old safety number describes the old key), and
    /// every session for the peer deleted so the next send re-runs PQXDH.
    #[tokio::test]
    async fn peer_reinstall_is_wedged_until_the_new_identity_is_accepted() {
        use libsignal_protocol::{
            DeviceId, Direction, IdentityKeyPair, IdentityKeyStore, ProtocolAddress, SessionRecord,
        };
        use rand::TryRngCore;

        let db_file = NamedTempFile::new().unwrap();
        let (service, key_store, _settings) = setup_service(db_file.path()).await;
        let mut store = SignalStore::new(key_store.clone());

        let mut rng = rand::rngs::OsRng.unwrap_err();
        let old = *IdentityKeyPair::generate(&mut rng).identity_key();
        let reinstalled = *IdentityKeyPair::generate(&mut rng).identity_key();

        let dev1 = ProtocolAddress::new("alice_uuid".to_string(), DeviceId::new(1).unwrap());

        // Established conversation: key pinned, safety number verified in person,
        // sessions on both of Alice's devices.
        store.save_identity(&dev1, &old).await.unwrap();
        key_store
            .set_peer_verified("alice_uuid", "12345 67890 12345 67890", true)
            .unwrap();
        let fresh = SessionRecord::new_fresh().serialize().unwrap();
        key_store.store_session("alice_uuid:1", &fresh).unwrap();
        key_store.store_session("alice_uuid:2", &fresh).unwrap();

        // Alice reinstalls. Her new key is refused in BOTH directions — inbound
        // PreKey messages fail and no new session can be built. This is the wedge.
        assert!(
            !store
                .is_trusted_identity(&dev1, &reinstalled, Direction::Receiving)
                .await
                .unwrap(),
            "the reinstalled key must be untrusted, or the pin means nothing"
        );
        assert!(!store
            .is_trusted_identity(&dev1, &reinstalled, Direction::Sending)
            .await
            .unwrap());

        // A malformed key must never reach the pin — it would fail to parse on
        // every later trust check and wedge the peer worse than the reinstall did.
        assert!(service
            .accept_peer_identity_change("alice_uuid", &[0u8; 7])
            .is_err());
        assert_eq!(
            key_store.get_identity("alice_uuid").unwrap().as_deref(),
            Some(old.serialize().as_ref()),
            "a rejected key must leave the pin untouched"
        );

        // The user compares the new safety number and accepts.
        let dropped = service
            .accept_peer_identity_change("alice_uuid", &reinstalled.serialize())
            .unwrap();
        assert_eq!(dropped, 2, "both device sessions must be dropped");

        // Wedge cleared: the new key is trusted and pinned.
        assert!(
            store
                .is_trusted_identity(&dev1, &reinstalled, Direction::Receiving)
                .await
                .unwrap(),
            "the conversation is still bricked after an explicit accept"
        );
        assert_eq!(
            key_store.get_identity("alice_uuid").unwrap().as_deref(),
            Some(reinstalled.serialize().as_ref())
        );

        // The old key is now the untrusted one, so accepting is a re-pin and not
        // a widening of trust.
        assert!(!store
            .is_trusted_identity(&dev1, &old, Direction::Receiving)
            .await
            .unwrap());

        // Verification is stale and must be cleared, or the UI would keep claiming
        // a safety number the user never checked against this key.
        assert!(!key_store.is_peer_verified("alice_uuid").unwrap());

        // No session survives: the next send has to fetch a bundle and re-run PQXDH.
        assert!(key_store.load_session("alice_uuid:1").unwrap().is_none());
        assert!(key_store.load_session("alice_uuid:2").unwrap().is_none());

        // Accepting the already-pinned key is a no-op — a duplicate accept must not
        // silently clear a verification the user has since re-done.
        key_store
            .set_peer_verified("alice_uuid", "09876 54321 09876 54321", true)
            .unwrap();
        assert_eq!(
            service
                .accept_peer_identity_change("alice_uuid", &reinstalled.serialize())
                .unwrap(),
            0
        );
        assert!(key_store.is_peer_verified("alice_uuid").unwrap());
    }
}
