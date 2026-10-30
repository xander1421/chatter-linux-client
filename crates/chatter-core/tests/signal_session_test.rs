//! Tests for Signal session management and actor pattern.
//!
//! Verifies that:
//! 1. Sessions created from receiving messages are reused (not overwritten)
//! 2. All Signal operations go through the actor pattern
//! 3. ensure_session checks for existing sessions before fetching prekeys

use std::sync::Arc;
use tempfile::NamedTempFile;

use chatter_core::services::SignalService;
use chatter_core::storage::{Database, KeyStore, SettingsRepository, settings_keys};
use libsignal_protocol::{
    DeviceId, GenericSignedPreKey, IdentityKeyPair, InMemSignalProtocolStore, KeyPair,
    KyberPreKeyRecord, KyberPreKeyStore, PreKeyBundle, PreKeyId, PreKeyRecord, PreKeyStore,
    ProtocolAddress, SessionStore, SignedPreKeyId, SignedPreKeyRecord, SignedPreKeyStore,
    Timestamp, kem, process_prekey_bundle,
};

/// Alice is the local party in these fixtures; libsignal >= 0.99 wants her
/// address alongside the peer's on every session call.
fn alice_addr() -> libsignal_protocol::ProtocolAddress {
    libsignal_protocol::ProtocolAddress::new(
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
        DeviceId::new(1).unwrap(),
    )
}

use rand::{SeedableRng, rngs::StdRng};
use std::time::SystemTime;

/// A serialized `SessionRecord` that can actually send.
///
/// `SessionRecord::new_fresh()` is not good enough and must not be used here.
/// It deserializes cleanly but has no sender chain, and `has_session`
/// deliberately requires `has_usable_sender_chain` — an archived session (after
/// a peer's DecryptionErrorMessage, say) deserializes fine and still cannot
/// encrypt, so it has to read as "no session" or the next send never goes out
/// as a PreKeySignalMessage. A fixture that skips the handshake tests the
/// opposite of what these tests are named for.
///
/// So run the real thing: build a bundle, process it, and take the session the
/// X3DH/PQXDH handshake actually produced.
async fn usable_session_bytes(peer: &ProtocolAddress) -> Vec<u8> {
    let (alice_store, _bob_store) = handshake(peer).await;
    alice_store
        .load_session(peer)
        .await
        .expect("load session")
        .expect("the handshake must have produced one")
        .serialize()
        .expect("serialize")
}

/// Alice's registration id in [`handshake`].
const ALICE_REGISTRATION_ID: u32 = 0xAAAA;

/// Bob (at `peer`) publishes a bundle and Alice processes it: Alice holds an
/// initiator session with Bob, Bob holds his prekeys and no session yet.
async fn handshake(peer: &ProtocolAddress) -> (InMemSignalProtocolStore, InMemSignalProtocolStore) {
    // Seeded, so a failure here is reproducible rather than a new random key
    // every run.
    let mut bob_rng = StdRng::seed_from_u64(0x5E551_014);
    let mut alice_rng = StdRng::seed_from_u64(0xA11CE_5E5);

    let bob_identity = IdentityKeyPair::generate(&mut bob_rng);
    let bob_registration_id: u32 = 0x1234;
    let mut bob_store =
        InMemSignalProtocolStore::new(bob_identity, bob_registration_id).expect("bob store");

    let pre_key_pair = KeyPair::generate(&mut bob_rng);
    let signed_pre_key_pair = KeyPair::generate(&mut bob_rng);
    let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut bob_rng);

    let signed_pre_key_public = signed_pre_key_pair.public_key.serialize();
    let signed_pre_key_signature = bob_identity
        .private_key()
        .calculate_signature(&signed_pre_key_public, &mut bob_rng)
        .expect("sign signed prekey");
    let kyber_pre_key_public = kyber_pre_key_pair.public_key.serialize();
    let kyber_pre_key_signature = bob_identity
        .private_key()
        .calculate_signature(&kyber_pre_key_public, &mut bob_rng)
        .expect("sign kyber prekey");

    let pre_key_id = PreKeyId::from(31337u32);
    let signed_pre_key_id = SignedPreKeyId::from(22u32);
    let kyber_pre_key_id = 8_675_309u32.into();

    let bundle = PreKeyBundle::new(
        bob_registration_id,
        peer.device_id(),
        Some((pre_key_id, pre_key_pair.public_key)),
        signed_pre_key_id,
        signed_pre_key_pair.public_key,
        signed_pre_key_signature.to_vec(),
        kyber_pre_key_id,
        kyber_pre_key_pair.public_key.clone(),
        kyber_pre_key_signature.to_vec(),
        *bob_identity.identity_key(),
    )
    .expect("bundle build");

    bob_store
        .save_pre_key(pre_key_id, &PreKeyRecord::new(pre_key_id, &pre_key_pair))
        .await
        .expect("save prekey");
    bob_store
        .save_signed_pre_key(
            signed_pre_key_id,
            &SignedPreKeyRecord::new(
                signed_pre_key_id,
                Timestamp::from_epoch_millis(1_700_000_000_000),
                &signed_pre_key_pair,
                &signed_pre_key_signature,
            ),
        )
        .await
        .expect("save signed prekey");
    bob_store
        .save_kyber_pre_key(
            kyber_pre_key_id,
            &KyberPreKeyRecord::new(
                kyber_pre_key_id,
                Timestamp::from_epoch_millis(1_700_000_000_000),
                &kyber_pre_key_pair,
                &kyber_pre_key_signature,
            ),
        )
        .await
        .expect("save kyber prekey");

    let alice_identity = IdentityKeyPair::generate(&mut alice_rng);
    let mut alice_store =
        InMemSignalProtocolStore::new(alice_identity, ALICE_REGISTRATION_ID).expect("alice store");
    process_prekey_bundle(
        peer,
        &alice_addr(),
        &mut alice_store.session_store,
        &mut alice_store.identity_store,
        &bundle,
        SystemTime::now(),
        &mut alice_rng,
    )
    .await
    .expect("process bundle");
    (alice_store, bob_store)
}

/// Helper to set up a test database and SignalService.
async fn setup_test_service() -> (
    SignalService,
    Arc<KeyStore>,
    Arc<SettingsRepository>,
    NamedTempFile,
) {
    let db_file = NamedTempFile::new().unwrap();
    let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
    db.run_migrations().unwrap();

    let key_store = Arc::new(KeyStore::new(db.clone()));
    let settings_repo = Arc::new(SettingsRepository::new(db.clone()));

    // Initialize required settings
    settings_repo
        .set(
            settings_keys::CURRENT_USER_ID,
            &"test-user-uuid".to_string(),
        )
        .unwrap();
    settings_repo
        .set(settings_keys::CURRENT_DEVICE_ID, &1u32)
        .unwrap();

    let service = SignalService::new(key_store.clone(), settings_repo.clone());
    (service, key_store, settings_repo, db_file)
}

/// Test that has_session returns false when no session exists.
#[tokio::test]
async fn test_has_session_returns_false_when_no_session() {
    let (service, _key_store, _settings, _db_file) = setup_test_service().await;

    let address = libsignal_protocol::ProtocolAddress::new(
        "peer-uuid".to_string(),
        libsignal_protocol::DeviceId::try_from(1u32).unwrap(),
    );

    let has_session = service.actor.has_session(address).await.unwrap();
    assert!(!has_session, "Should not have session for unknown peer");
}

/// Test that has_session returns true after storing a session.
#[tokio::test]
async fn test_has_session_returns_true_after_session_created() {
    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    let peer_id = "peer-uuid";
    let device_id = 1u32;
    let session_key = format!("{}:{}", peer_id, device_id);

    let address = libsignal_protocol::ProtocolAddress::new(
        peer_id.to_string(),
        libsignal_protocol::DeviceId::try_from(device_id).unwrap(),
    );
    // A session that can actually send — see `usable_session_bytes`.
    let session_bytes = usable_session_bytes(&address).await;
    key_store
        .store_session(&session_key, &session_bytes)
        .unwrap();

    let has_session = service.actor.has_session(address).await.unwrap();
    assert!(has_session, "Should have session after storing one");
}

/// A session reset must archive the peer's sessions, or it is a no-op:
/// `ensure_sessions_all_devices` reuses every device `session_devices` lists
/// and fetches no bundle, so the broken sessions survive the "reset".
#[tokio::test]
async fn archive_peer_sessions_leaves_no_reusable_session_for_that_peer_only() {
    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    for (peer, device) in [("peer-uuid", 1u32), ("peer-uuid", 2), ("other-uuid", 1)] {
        let address = ProtocolAddress::new(peer.to_string(), DeviceId::try_from(device).unwrap());
        key_store
            .store_session(
                &format!("{peer}:{device}"),
                &usable_session_bytes(&address).await,
            )
            .unwrap();
    }
    assert_eq!(
        service.session_devices("peer-uuid").await.unwrap(),
        vec![1, 2]
    );

    assert_eq!(service.archive_peer_sessions("peer-uuid").await.unwrap(), 2);

    assert!(
        service
            .session_devices("peer-uuid")
            .await
            .unwrap()
            .is_empty(),
        "an archived session still reads as usable, so the rebuild would skip it"
    );
    assert!(
        key_store.load_session("peer-uuid:1").unwrap().is_some(),
        "archived, not deleted: in-flight messages under the old state still decrypt"
    );
    assert_eq!(
        service.session_devices("other-uuid").await.unwrap(),
        vec![1]
    );
}

/// OPEN-ITEMS §125: the registration id a sealed send names for a device is
/// read from our session with it (`SignalService::session_registration_id`).
/// When the PEER started the session — we are the responder, our session
/// came from its PreKeySignalMessage — libsignal must have recorded the
/// peer's registration id from that message, or every send to such a device
/// would name no id and be refused as stale.
#[tokio::test]
async fn responder_session_records_the_initiators_registration_id() {
    let (service, key_store, _settings, _db_file) = setup_test_service().await;
    let bob = ProtocolAddress::new(
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string(),
        DeviceId::new(2).unwrap(),
    );
    let (mut alice_store, mut bob_store) = handshake(&bob).await;
    let mut rng = StdRng::seed_from_u64(7);

    let ciphertext = libsignal_protocol::message_encrypt(
        b"hello bob",
        &bob,
        &alice_addr(),
        &mut alice_store.session_store,
        &mut alice_store.identity_store,
        SystemTime::now(),
        &mut rng,
    )
    .await
    .expect("alice encrypts");
    assert!(matches!(
        ciphertext,
        libsignal_protocol::CiphertextMessage::PreKeySignalMessage(_)
    ));
    let plaintext = libsignal_protocol::message_decrypt(
        &ciphertext,
        &alice_addr(),
        &bob,
        &mut bob_store.session_store,
        &mut bob_store.identity_store,
        &mut bob_store.pre_key_store,
        &bob_store.signed_pre_key_store,
        &mut bob_store.kyber_pre_key_store,
        &mut rng,
    )
    .await
    .expect("bob decrypts the PreKey message");
    assert_eq!(plaintext, b"hello bob");

    // Bob's responder session, stored where chatter keeps sessions.
    let bob_session = bob_store
        .load_session(&alice_addr())
        .await
        .expect("load")
        .expect("decrypting the PreKey message made a session")
        .serialize()
        .expect("serialize");
    let alice = alice_addr();
    key_store
        .store_session(
            &format!("{}:{}", alice.name(), u32::from(alice.device_id())),
            &bob_session,
        )
        .unwrap();

    assert_eq!(
        service.session_registration_id(alice.name(), u32::from(alice.device_id())),
        Some(ALICE_REGISTRATION_ID),
        "the responder session names the initiator's registration id"
    );
}

/// F2 / OPEN-ITEMS §124: a recipient we hold sessions with is sent to on
/// those sessions with NO prekey-bundle fetch — the server's answer to the
/// send names any device we are missing. Before, a 10-minute
/// `DEVICE_LIST_TTL` re-fetched the list (popping a one-time prekey per
/// device) on any send it had not cached. The auth client here points at a
/// closed port, so any fetch fails the call.
#[tokio::test]
async fn held_sessions_are_used_without_a_bundle_fetch() {
    let (service, key_store, _settings, _db_file) = setup_test_service().await;
    for device in [1u32, 2] {
        let address =
            ProtocolAddress::new("peer-uuid".to_string(), DeviceId::try_from(device).unwrap());
        key_store
            .store_session(
                &format!("peer-uuid:{device}"),
                &usable_session_bytes(&address).await,
            )
            .unwrap();
    }
    let mut auth_client = unreachable_auth_client();

    assert_eq!(
        service
            .ensure_sessions_all_devices(&mut auth_client, "peer-uuid")
            .await
            .unwrap(),
        vec![1, 2]
    );

    // Only a caller that needs the recipient's sealed-sender access policy,
    // not read yet this run, fetches (and so fails here).
    assert!(
        service
            .ensure_sessions_and_access_policy_all_devices(&mut auth_client, "peer-uuid", true)
            .await
            .is_err()
    );
}

fn unreachable_auth_client() -> chatter_core::network::grpc::AuthClient {
    let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
    chatter_core::network::grpc::AuthClient::new(
        channel.into(),
        chatter_core::network::grpc::interceptors::AuthInterceptor::default(),
    )
}

#[tokio::test]
async fn new_local_device_archives_every_peer_even_when_warmup_is_offline() {
    let (service, key_store, settings, _db_file) = setup_test_service().await;
    let mut expected_archives = Vec::new();
    for peer in ["peer-a", "peer-b"] {
        let address = ProtocolAddress::new(peer.into(), DeviceId::new(1).unwrap());
        key_store
            .store_session(&format!("{peer}:1"), &usable_session_bytes(&address).await)
            .unwrap();
        assert!(service.has_session(peer, 1).await.unwrap());
        let mut record = libsignal_protocol::SessionRecord::deserialize(
            &key_store
                .load_session(&format!("{peer}:1"))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        record.archive_current_state().unwrap();
        expected_archives.push(record.serialize().unwrap());
    }
    settings
        .set(settings_keys::CURRENT_DEVICE_ID, &2u32)
        .unwrap();
    service
        .rebind_device_sessions(&mut unreachable_auth_client())
        .await
        .unwrap();
    for (peer, expected) in ["peer-a", "peer-b"].into_iter().zip(expected_archives) {
        assert!(
            !service.has_session(peer, 1).await.unwrap(),
            "old-address ratchet must never be reused"
        );
        let bytes = key_store
            .load_session(&format!("{peer}:1"))
            .unwrap()
            .unwrap();
        assert_eq!(
            bytes, expected,
            "retain the archived in-flight receive state"
        );
    }
}

/// Test that actor operations are serialized through the message channel.
/// This verifies the actor pattern is being used correctly.
#[tokio::test]
async fn test_actor_operations_are_serialized() {
    let (service, _key_store, _settings, _db_file) = setup_test_service().await;

    // Spawn multiple concurrent has_session checks
    // All should go through the actor and return consistent results
    let mut handles = vec![];

    for i in 0..10 {
        let actor = service.actor.clone();
        let handle = tokio::spawn(async move {
            let address = libsignal_protocol::ProtocolAddress::new(
                format!("peer-{}", i),
                libsignal_protocol::DeviceId::try_from(1u32).unwrap(),
            );
            actor.has_session(address).await
        });
        handles.push(handle);
    }

    // All should complete without deadlock
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "Actor operation should succeed");
        assert!(!result.unwrap(), "No sessions should exist for test peers");
    }
}

/// Test that the actor spawns on a dedicated thread.
/// This verifies non-Send types are isolated properly.
#[tokio::test]
async fn test_actor_runs_on_dedicated_thread() {
    let (service, _key_store, _settings, _db_file) = setup_test_service().await;

    // Get identity fingerprint through actor
    // This operation uses non-Send libsignal types internally
    let result = service.actor.get_identity_fingerprint().await;

    // Should either succeed (if identity exists) or fail gracefully
    // The important thing is it doesn't panic due to Send requirements
    match result {
        Ok(fingerprint) => {
            // Fingerprint should be hex-formatted
            assert!(
                fingerprint
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ' ')
            );
        }
        Err(e) => {
            // Expected if no identity key is generated yet
            let error_str = e.to_string();
            assert!(
                error_str.contains("identity")
                    || error_str.contains("not found")
                    || error_str.contains("Protocol"),
                "Unexpected error: {}",
                error_str
            );
        }
    }
}

/// Test that sender key operations go through the actor.
#[tokio::test]
async fn test_sender_key_operations_use_actor() {
    let (service, _key_store, _settings, _db_file) = setup_test_service().await;

    let channel_id = chatter_core::domain::ids::ChannelId::new();
    let our_address = libsignal_protocol::ProtocolAddress::new(
        "our-uuid".to_string(),
        libsignal_protocol::DeviceId::try_from(1u32).unwrap(),
    );

    // Check has_sender_key through actor
    let has_key = service
        .actor
        .has_sender_key(channel_id, our_address.clone())
        .await;
    assert!(has_key.is_ok(), "has_sender_key should succeed");
    assert!(
        !has_key.unwrap(),
        "Should not have sender key for new channel"
    );
}

/// Integration test simulating the bootstrap flow session reuse.
///
/// This tests the critical fix: when we receive a PreKey message from a peer,
/// the session created should be reused for the response, not overwritten
/// by fetching new prekeys.
#[tokio::test]
async fn test_session_reuse_after_message_decrypt() {
    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    let peer_id = "bootstrap-peer-uuid";
    let device_id = 1u32;
    let session_key = format!("{}:{}", peer_id, device_id);

    // Simulate: we received and decrypted a PreKey message from peer. That
    // normally happens in decrypt_message, which stores the session — so the
    // fixture has to be a session that can send, not merely one that parses.
    let address = libsignal_protocol::ProtocolAddress::new(
        peer_id.to_string(),
        libsignal_protocol::DeviceId::try_from(device_id).unwrap(),
    );
    let session_bytes = usable_session_bytes(&address).await;
    key_store
        .store_session(&session_key, &session_bytes)
        .unwrap();

    let has_session = service.actor.has_session(address).await.unwrap();
    assert!(
        has_session,
        "Session should exist after message decryption simulation"
    );

    // The session data should be preserved (not overwritten)
    let loaded_session = key_store.load_session(&session_key).unwrap();
    assert!(loaded_session.is_some(), "Session should still be stored");
    assert_eq!(
        loaded_session.unwrap(),
        session_bytes,
        "Session data should not be modified"
    );
}

/// The SEND half of the peer-reinstall wedge.
///
/// It is NOT `establish_session` in the common case. After a peer reinstalls, a
/// stale session is normally still on disk, so `ensure_session`'s `has_session`
/// short-circuit fires, the prekey fetch never happens, and `establish_session`
/// is never reached. libsignal's send-side trust check
/// (`session_cipher.rs:137`) then compares the SESSION's stored remote identity
/// against the pinned identity — both are the peer's OLD key, so it passes and
/// we encrypt under a ratchet the peer no longer holds. The message goes out
/// undeliverable, with nothing to signal that anything is wrong.
///
/// Nothing local can detect that on its own, so once a receive path has parked
/// the change, `ensure_session` has to consult it and refuse.
///
/// The auth client points at a dead port ON PURPOSE: the refusal must happen
/// before any network use. If the gate is missing, the short-circuit returns
/// `Ok(())` without touching it and this fails on the assert rather than
/// hanging.
#[tokio::test]
async fn ensure_session_refuses_a_stale_session_when_an_identity_change_is_parked() {
    use chatter_core::error::{CryptoError, IdentityChangeSource, ServiceError};
    use chatter_core::network::grpc::auth_client::AuthClient;
    use chatter_core::network::grpc::interceptors::AuthInterceptor;
    use libsignal_protocol::IdentityKeyPair;

    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    let peer_id = "peer-uuid";
    let address = ProtocolAddress::new(peer_id.to_string(), DeviceId::try_from(1u32).unwrap());

    // A session that can genuinely send — this is what makes ensure_session
    // short-circuit, and the whole point of the test.
    let session_bytes = usable_session_bytes(&address).await;
    key_store
        .store_session(&format!("{peer_id}:1"), &session_bytes)
        .unwrap();
    assert!(
        service.has_session(peer_id, 1).await.unwrap(),
        "precondition: without a usable session there is no short-circuit to defeat"
    );

    let mut rng = StdRng::seed_from_u64(0x0DD_1DEA);
    let reinstalled = *IdentityKeyPair::generate(&mut rng).identity_key();

    // A receive path surfaced the change and parked it (this is what
    // `SignalService::note_identity_change` writes).
    key_store
        .record_pending_identity_change(
            peer_id,
            &reinstalled.serialize(),
            IdentityChangeSource::SenderCertificate.as_db_str(),
            1,
        )
        .unwrap();

    let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
    let mut auth_client = AuthClient::new(channel.into(), AuthInterceptor::new());

    match service.ensure_session(&mut auth_client, peer_id, 1).await {
        Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
            peer_id: got,
            attribution,
            ..
        })) => {
            assert_eq!(got, peer_id);
            assert!(
                attribution.is_authenticated(),
                "the sealed-sender provenance must survive to the send path"
            );
        }
        Ok(()) => panic!(
            "ensure_session reused the stale session despite a parked identity change — every \
             message from here is encrypted to a key the peer no longer has, and nothing says so"
        ),
        Err(e) => panic!("expected UntrustedIdentity, got {e:?}"),
    }
}

/// Build a real `PreKeySignalMessage` from `sender_identity` addressed to a
/// throwaway recipient bundle.
///
/// The recipient's prekeys deliberately do NOT have to match the store that
/// will decrypt this: libsignal checks the sender's identity FIRST in
/// `process_prekey` (session.rs:57-63), before any prekey lookup, so a message
/// carrying an untrusted identity key never reaches them.
async fn prekey_message_from(sender_identity: IdentityKeyPair, seed: u64) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let recipient =
        ProtocolAddress::new("recipient".to_string(), DeviceId::try_from(1u32).unwrap());

    let rcpt_identity = IdentityKeyPair::generate(&mut rng);
    let pre_key_pair = KeyPair::generate(&mut rng);
    let signed_pre_key_pair = KeyPair::generate(&mut rng);
    let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);

    let signed_sig = rcpt_identity
        .private_key()
        .calculate_signature(&signed_pre_key_pair.public_key.serialize(), &mut rng)
        .unwrap();
    let kyber_sig = rcpt_identity
        .private_key()
        .calculate_signature(&kyber_pre_key_pair.public_key.serialize(), &mut rng)
        .unwrap();

    let bundle = PreKeyBundle::new(
        0x1234,
        recipient.device_id(),
        Some((PreKeyId::from(31337u32), pre_key_pair.public_key)),
        SignedPreKeyId::from(22u32),
        signed_pre_key_pair.public_key,
        signed_sig.to_vec(),
        8_675_309u32.into(),
        kyber_pre_key_pair.public_key.clone(),
        kyber_sig.to_vec(),
        *rcpt_identity.identity_key(),
    )
    .unwrap();

    let mut sender_store = InMemSignalProtocolStore::new(sender_identity, 0xAAAA).unwrap();
    process_prekey_bundle(
        &recipient,
        &alice_addr(),
        &mut sender_store.session_store,
        &mut sender_store.identity_store,
        &bundle,
        SystemTime::now(),
        &mut rng,
    )
    .await
    .unwrap();

    let msg = libsignal_protocol::message_encrypt(
        b"hello from the other side",
        &recipient,
        &alice_addr(),
        &mut sender_store.session_store,
        &mut sender_store.identity_store,
        SystemTime::now(),
        &mut rng,
    )
    .await
    .unwrap();

    assert_eq!(
        msg.message_type(),
        libsignal_protocol::CiphertextMessageType::PreKey,
        "the reinstall case is a PreKey message; the fixture is wrong otherwise"
    );
    msg.serialize().to_vec()
}

/// RECEIVE sibling: identified-sender decrypt must surface a changed identity
/// key as `UntrustedIdentity`, not a generic protocol failure — no session
/// remedy can fix a changed key, and the accept path is offered only on this
/// error.
#[tokio::test]
async fn identified_decrypt_surfaces_a_changed_identity_instead_of_a_generic_failure() {
    use chatter_core::error::{CryptoError, ServiceError};

    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    let mut rng = StdRng::seed_from_u64(0xA11CE_0117);
    let old = *IdentityKeyPair::generate(&mut rng).identity_key();
    let reinstalled_pair = IdentityKeyPair::generate(&mut rng);

    // Bob pinned Alice's OLD key; Alice reinstalled and sends under a new one.
    key_store
        .save_identity("alice_uuid", &old.serialize())
        .unwrap();
    let envelope = prekey_message_from(reinstalled_pair, 0xBEEF).await;

    match service
        .decrypt_message_identified(&envelope, "alice_uuid", 1)
        .await
    {
        Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
            peer_id,
            new_identity_key,
            attribution,
            ..
        })) => {
            assert_eq!(peer_id, "alice_uuid");
            assert_eq!(
                new_identity_key.as_deref(),
                Some(reinstalled_pair.identity_key().serialize().as_ref()),
                "the key the user is asked to accept must be the one on the wire"
            );
            assert!(
                !attribution.is_authenticated(),
                "an identified-sender peer id is server-claimed and must never read as proven"
            );
        }
        Ok(_) => panic!("decrypted under a changed identity key instead of refusing it"),
        Err(e) => panic!("expected UntrustedIdentity, got {e:?}"),
    }
}
