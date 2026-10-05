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

pub mod actor;
mod identity;
pub mod kt_anchor;
mod sender_key;
mod session;

use libsignal_protocol::PublicKey;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use self::actor::SignalActorHandle;
use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;

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
    /// Per recipient, the `unrestricted_unidentified_access` flag of the last
    /// bundle fetch this run (see
    /// `ensure_sessions_and_access_policy_all_devices`). Not a device list:
    /// which devices a recipient has is the server's answer to each send.
    pub(crate) unrestricted_access: Arc<std::sync::Mutex<HashMap<String, bool>>>,
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
            unrestricted_access: self.unrestricted_access.clone(),
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
    pub fn new(key_store: Arc<KeyStore>, settings_repo: Arc<SettingsRepository>) -> Self {
        let trust_root = *crate::config::TRUST_ROOT_PUBLIC_KEY;
        let actor = SignalActorHandle::spawn(key_store.clone(), settings_repo.clone());

        Self {
            key_store,
            settings_repo,
            trust_root,
            actor,
            encrypt_locks: Arc::new(Mutex::new(HashMap::new())),
            encrypt_lock_lru: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            unrestricted_access: Arc::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SignalStore;
    use crate::error::ServiceError;
    use crate::storage::Database;
    use crate::storage::repositories::settings_keys;
    use libsignal_protocol::SessionRecord;
    use tempfile::NamedTempFile;

    async fn setup_service(
        path: &std::path::Path,
    ) -> (SignalService, Arc<KeyStore>, Arc<SettingsRepository>) {
        let db = Arc::new(Database::open(path, "test_passphrase").unwrap());
        db.run_migrations().unwrap();

        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));

        // Initialize settings
        settings_repo
            .set(settings_keys::CURRENT_USER_ID, &"bob_uuid".to_string())
            .unwrap();
        settings_repo
            .set(settings_keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();

        let service = SignalService::new(key_store.clone(), settings_repo.clone());
        (service, key_store, settings_repo)
    }

    #[tokio::test]
    async fn test_signal_service_creation() {
        let db_file = NamedTempFile::new().unwrap();
        let (service, _key_store, _settings) = setup_service(db_file.path()).await;

        // Basic test that service can be created
        assert!(
            service.key_store.get_local_identity().is_err()
                || service.key_store.get_local_identity().unwrap().is_none()
        );
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
        assert!(
            store
                .is_trusted_identity(&dev1, &real, Direction::Receiving)
                .await
                .unwrap()
        );
        store.save_identity(&dev1, &real).await.unwrap();

        // A second legitimate device shares the SAME identity key -> trusted,
        // even though its (user:device) address was never seen before.
        assert!(
            store
                .is_trusted_identity(&dev2, &real, Direction::Sending)
                .await
                .unwrap()
        );

        // A server-injected ghost device presents a DIFFERENT key -> rejected.
        assert!(
            !store
                .is_trusted_identity(&dev3, &attacker, Direction::Sending)
                .await
                .unwrap()
        );
    }

    /// A peer reinstall wedges the conversation, and accepting the new key must
    /// clear it COMPLETELY — in both directions and for groups.
    ///
    /// The wedge exists because identity keys are pinned per account and
    /// libsignal only calls `save_identity` AFTER `is_trusted_identity` passes
    /// (session.rs:61/186, session_cipher.rs:145/317), so nothing in the
    /// protocol can ever re-pin a changed key.
    ///
    /// Accepting has to do everything this repo already does for a peer
    /// identity/device change, which is two phases and not one: drop the
    /// pairwise sessions AND mark every shared channel for sender-key rotation.
    /// Marking only the first leaves group traffic flowing under a sender key
    /// the changed peer still holds.
    #[tokio::test]
    async fn accepting_a_reinstalled_identity_unwedges_dms_and_rotates_group_keys() {
        use crate::domain::{
            channel::{Channel, ChannelType, MemberRole, Membership, NotificationPreference},
            ids::{ChannelId, UserId},
        };
        use crate::storage::ChannelRepository;
        use libsignal_protocol::{
            DeviceId, Direction, IdentityKeyPair, IdentityKeyStore, ProtocolAddress,
        };
        use rand::TryRngCore;

        let db_file = NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));
        settings_repo
            .set(settings_keys::CURRENT_USER_ID, &"bob_uuid".to_string())
            .unwrap();
        settings_repo
            .set(settings_keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        let service = SignalService::new(key_store.clone(), settings_repo.clone());
        let channel_repo = ChannelRepository::new(db.clone());
        let mut store = SignalStore::new(key_store.clone());

        let alice = UserId::new();
        let alice_str = alice.to_string();

        let mut rng = rand::rngs::OsRng.unwrap_err();
        let old = *IdentityKeyPair::generate(&mut rng).identity_key();
        let reinstalled = *IdentityKeyPair::generate(&mut rng).identity_key();

        let dev1 = ProtocolAddress::new(alice_str.clone(), DeviceId::new(1).unwrap());

        // An established conversation: key pinned, safety number verified in
        // person, sessions on two devices, and a shared group.
        store.save_identity(&dev1, &old).await.unwrap();
        key_store
            .set_peer_verified(&alice_str, "12345 67890 12345 67890", true)
            .unwrap();
        let fresh = SessionRecord::new_fresh().serialize().unwrap();
        key_store
            .store_session(&format!("{alice_str}:1"), &fresh)
            .unwrap();
        key_store
            .store_session(&format!("{alice_str}:2"), &fresh)
            .unwrap();

        let shared_channel = ChannelId::new();
        channel_repo
            .insert(&Channel::new(
                shared_channel,
                "shared".into(),
                ChannelType::Public,
                UserId::new(),
            ))
            .unwrap();
        channel_repo
            .insert_membership(&Membership {
                channel_id: shared_channel,
                user_id: alice,
                role: MemberRole::Member,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::All,
            })
            .unwrap();

        // Alice reinstalls. Her new key is refused in BOTH directions: inbound
        // PreKey messages fail and no new session can be built. The wedge.
        assert!(
            !store
                .is_trusted_identity(&dev1, &reinstalled, Direction::Receiving)
                .await
                .unwrap(),
            "the reinstalled key must be untrusted, or the pin means nothing"
        );
        assert!(
            !store
                .is_trusted_identity(&dev1, &reinstalled, Direction::Sending)
                .await
                .unwrap()
        );

        // A malformed key must never reach the pin — it would fail to parse on
        // every later trust check and wedge the peer worse than the reinstall.
        assert!(
            service
                .accept_peer_identity_change(&alice, &[0u8; 7], &channel_repo)
                .is_err()
        );
        assert_eq!(
            key_store.get_identity(&alice_str).unwrap().as_deref(),
            Some(old.serialize().as_ref()),
            "a rejected key must leave the pin untouched"
        );

        // The user compares the new safety number and accepts.
        let dropped = service
            .accept_peer_identity_change(&alice, &reinstalled.serialize(), &channel_repo)
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
        // The old key is now the untrusted one, so this was a re-pin and not a
        // widening of trust.
        assert!(
            !store
                .is_trusted_identity(&dev1, &old, Direction::Receiving)
                .await
                .unwrap()
        );

        // Verification is stale and must be cleared, or the UI keeps claiming a
        // safety number the user never checked against this key.
        assert!(!key_store.is_peer_verified(&alice_str).unwrap());

        // No session survives: the next send re-runs PQXDH.
        assert!(
            key_store
                .load_session(&format!("{alice_str}:1"))
                .unwrap()
                .is_none()
        );
        assert!(
            key_store
                .load_session(&format!("{alice_str}:2"))
                .unwrap()
                .is_none()
        );

        // PHASE 2: the shared channel is marked for sender-key rotation.
        // Without this, group messages keep flowing under a sender key the
        // peer who just changed identity still holds.
        assert!(
            service
                .check_and_clear_rotation_needed(&shared_channel)
                .unwrap(),
            "accepting an identity change left the shared channel's sender key in place"
        );
    }

    /// Accepting must be one transaction, because the half-state is worse than
    /// the wedge.
    ///
    /// Pinning the new key while the old sessions survive gives a conversation
    /// that receives but cannot send: `ensure_session` sees a session, skips the
    /// prekey fetch, and keeps encrypting under a ratchet negotiated against the
    /// DEAD key — silently undeliverable. So the pin, the unverify and the
    /// session delete must commit together or not at all.
    #[tokio::test]
    async fn accepting_an_identity_change_is_one_transaction() {
        use libsignal_protocol::IdentityKeyPair;
        use rand::TryRngCore;

        let db_file = NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let key_store = Arc::new(KeyStore::new(db.clone()));

        let mut rng = rand::rngs::OsRng.unwrap_err();
        let old = *IdentityKeyPair::generate(&mut rng).identity_key();
        let reinstalled = *IdentityKeyPair::generate(&mut rng).identity_key();

        key_store
            .save_identity("alice_uuid", &old.serialize())
            .unwrap();
        key_store
            .set_peer_verified("alice_uuid", "12345 67890 12345 67890", true)
            .unwrap();
        let fresh = SessionRecord::new_fresh().serialize().unwrap();
        key_store.store_session("alice_uuid:1", &fresh).unwrap();

        // Poison the unverify step: dropping peer_verification makes its UPDATE
        // fail mid-transaction, standing in for a crash between the writes.
        db.connection()
            .unwrap()
            .execute("DROP TABLE peer_verification", [])
            .unwrap();

        let result = key_store.accept_peer_identity_change("alice_uuid", &reinstalled.serialize());
        assert!(result.is_err(), "the poisoned write must fail the accept");

        // Nothing may have been applied. In particular the new key must NOT be
        // pinned while the old session is still on disk.
        assert_eq!(
            key_store.get_identity("alice_uuid").unwrap().as_deref(),
            Some(old.serialize().as_ref()),
            "a failed accept left the new key pinned — sends now use a dead ratchet"
        );
        assert!(
            key_store.load_session("alice_uuid:1").unwrap().is_some(),
            "a failed accept dropped the session anyway — partial application"
        );
    }

    /// A surfaced identity change must be PARKED, or the send path can never
    /// learn about it.
    ///
    /// The send half of the wedge is not `establish_session`: with a stale
    /// session still on disk `ensure_session` short-circuits, so the only way it
    /// can know is to read what a receive path wrote down. This covers that
    /// write and read-back, provenance included.
    ///
    /// (That `ensure_session` actually consults it — against a session that can
    /// genuinely send — is
    /// `tests/signal_session_test.rs::ensure_session_refuses_a_stale_session_when_an_identity_change_is_parked`,
    /// which needs the real PQXDH handshake fixture to build one.)
    #[tokio::test]
    async fn a_surfaced_identity_change_is_parked_for_the_send_path() {
        use libsignal_protocol::IdentityKeyPair;
        use rand::TryRngCore;

        let db_file = NamedTempFile::new().unwrap();
        let (service, _key_store, _settings) = setup_service(db_file.path()).await;

        let mut rng = rand::rngs::OsRng.unwrap_err();
        let reinstalled = *IdentityKeyPair::generate(&mut rng).identity_key();

        assert!(
            service
                .pending_identity_change("alice_uuid")
                .unwrap()
                .is_none(),
            "precondition: nothing parked yet"
        );

        service.note_identity_change(&ServiceError::Crypto(
            crate::error::CryptoError::UntrustedIdentity {
                peer_id: "alice_uuid".into(),
                device_id: 1,
                new_identity_key: Some(reinstalled.serialize().to_vec()),
                attribution: crate::error::IdentityChangeSource::SenderCertificate,
            },
        ));

        let (key, attribution) = service
            .pending_identity_change("alice_uuid")
            .unwrap()
            .expect(
                "the identity change was not parked, so the send path can never see it and \
                 will keep encrypting under the dead ratchet",
            );
        assert_eq!(key, reinstalled.serialize().to_vec());
        assert!(
            attribution.is_authenticated(),
            "provenance was lost; a sealed-sender change must stay distinguishable from a \
             server-claimed one"
        );

        // First writer wins: a later, less-trusted sighting must not swap the
        // key out from under a prompt the user is already looking at.
        let injected = *IdentityKeyPair::generate(&mut rng).identity_key();
        service.note_identity_change(&ServiceError::Crypto(
            crate::error::CryptoError::UntrustedIdentity {
                peer_id: "alice_uuid".into(),
                device_id: 1,
                new_identity_key: Some(injected.serialize().to_vec()),
                attribution: crate::error::IdentityChangeSource::ServerClaimed,
            },
        ));
        let (key_after, attribution_after) = service
            .pending_identity_change("alice_uuid")
            .unwrap()
            .unwrap();
        assert_eq!(key_after, reinstalled.serialize().to_vec());
        assert!(attribution_after.is_authenticated());
    }

    /// Provenance must survive to the error, and the two receive paths must not
    /// claim the same thing.
    ///
    /// The sealed path's peer id comes from a sender certificate validated
    /// against the trust root; the identified path's comes from envelope
    /// metadata the SERVER supplied and nothing authenticates. A UI that cannot
    /// tell them apart would present a server-asserted identity change as
    /// though it had been cryptographically attributed.
    #[test]
    fn the_two_receive_paths_do_not_claim_the_same_provenance() {
        use crate::error::IdentityChangeSource;

        assert!(IdentityChangeSource::SenderCertificate.is_authenticated());
        assert!(
            !IdentityChangeSource::ServerClaimed.is_authenticated(),
            "a server-claimed sender must never read as authenticated"
        );
        assert!(!IdentityChangeSource::PreKeyBundle.is_authenticated());

        // Round-trips through the parked row, and an unreadable token degrades
        // to the LEAST trusted variant rather than the most.
        for v in [
            IdentityChangeSource::SenderCertificate,
            IdentityChangeSource::ServerClaimed,
            IdentityChangeSource::PreKeyBundle,
        ] {
            assert_eq!(IdentityChangeSource::from_db_str(v.as_db_str()), v);
        }
        assert!(!IdentityChangeSource::from_db_str("nonsense").is_authenticated());
    }
}
