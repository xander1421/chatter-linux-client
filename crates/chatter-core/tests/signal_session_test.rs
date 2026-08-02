//! Tests for Signal session management and actor pattern.
//!
//! Verifies that:
//! 1. Sessions created from receiving messages are reused (not overwritten)
//! 2. All Signal operations go through the actor pattern
//! 3. ensure_session checks for existing sessions before fetching prekeys

use std::sync::Arc;
use tempfile::NamedTempFile;

use chatter_core::storage::{Database, KeyStore, SettingsRepository, settings_keys};
use chatter_core::services::SignalService;
use libsignal_protocol::{
    kem, process_prekey_bundle, DeviceId, GenericSignedPreKey, IdentityKeyPair,
    InMemSignalProtocolStore, KeyPair, KyberPreKeyRecord, KyberPreKeyStore, PreKeyBundle, PreKeyId,
    PreKeyRecord, PreKeyStore, ProtocolAddress, SessionStore, SignedPreKeyId, SignedPreKeyRecord,
    SignedPreKeyStore, Timestamp,
};
use rand::{rngs::StdRng, SeedableRng};
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
    let mut alice_store = InMemSignalProtocolStore::new(alice_identity, 0xAAAA).expect("alice store");
    process_prekey_bundle(
        peer,
        &mut alice_store.session_store,
        &mut alice_store.identity_store,
        &bundle,
        SystemTime::now(),
        &mut alice_rng,
    )
    .await
    .expect("process bundle");

    alice_store
        .load_session(peer)
        .await
        .expect("load session")
        .expect("the handshake must have produced one")
        .serialize()
        .expect("serialize")
}

/// Helper to set up a test database and SignalService.
async fn setup_test_service() -> (SignalService, Arc<KeyStore>, Arc<SettingsRepository>, NamedTempFile) {
    let db_file = NamedTempFile::new().unwrap();
    let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
    db.run_migrations().unwrap();

    let key_store = Arc::new(KeyStore::new(db.clone()));
    let settings_repo = Arc::new(SettingsRepository::new(db.clone()));

    // Initialize required settings
    settings_repo.set(settings_keys::CURRENT_USER_ID, &"test-user-uuid".to_string()).unwrap();
    settings_repo.set(settings_keys::CURRENT_DEVICE_ID, &1u32).unwrap();

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
    key_store.store_session(&session_key, &session_bytes).unwrap();

    let has_session = service.actor.has_session(address).await.unwrap();
    assert!(has_session, "Should have session after storing one");
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
            assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit() || c == ' '));
        }
        Err(e) => {
            // Expected if no identity key is generated yet
            let error_str = e.to_string();
            assert!(
                error_str.contains("identity") || error_str.contains("not found") || error_str.contains("Protocol"),
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
    let has_key = service.actor.has_sender_key(channel_id, our_address.clone()).await;
    assert!(has_key.is_ok(), "has_sender_key should succeed");
    assert!(!has_key.unwrap(), "Should not have sender key for new channel");
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
    key_store.store_session(&session_key, &session_bytes).unwrap();

    let has_session = service.actor.has_session(address).await.unwrap();
    assert!(has_session, "Session should exist after message decryption simulation");

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
    key_store.store_session(&format!("{peer_id}:1"), &session_bytes).unwrap();
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
    let mut auth_client = AuthClient::new(channel, AuthInterceptor::new());

    match service.ensure_session(&mut auth_client, peer_id, 1).await {
        Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
            peer_id: got, attribution, ..
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
    let recipient = ProtocolAddress::new("recipient".to_string(), DeviceId::try_from(1u32).unwrap());

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

/// RECEIVE sibling: the Sesame convergence path must surface a changed identity
/// key too.
///
/// `decrypt_identified_message` routes through convergence whenever the peer has
/// any sesame session, and that loop used to swallow every per-attempt
/// `SignalProtocolError` with `if let Ok(..)`, reporting only a generic
/// `AllFailed`. So on the single most common route for a peer reinstall the
/// wedge was invisible: no candidate session can ever fix a changed identity
/// key — every one of them fails the same trust check — yet the caller was told
/// "session convergence failed" and offered a session-level remedy that cannot
/// converge.
#[tokio::test]
async fn convergence_surfaces_a_changed_identity_instead_of_a_generic_failure() {
    use chatter_core::error::{CryptoError, ServiceError};
    use libsignal_protocol::SessionRecord;

    let (service, key_store, _settings, _db_file) = setup_test_service().await;

    let mut rng = StdRng::seed_from_u64(0xA11CE_0117);
    let old = *IdentityKeyPair::generate(&mut rng).identity_key();
    let reinstalled_pair = IdentityKeyPair::generate(&mut rng);

    // Bob pinned Alice's OLD key; Alice reinstalled and sends under a new one.
    key_store.save_identity("alice_uuid", &old.serialize()).unwrap();
    let envelope = prekey_message_from(reinstalled_pair, 0xBEEF).await;

    // Alice has sesame sessions, which is what routes this through convergence.
    let candidates = vec![
        (vec![1u8; 8], SessionRecord::new_fresh()),
        (vec![2u8; 8], SessionRecord::new_fresh()),
    ];

    match service
        .decrypt_with_convergence(&envelope, "alice_uuid", 1, candidates)
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
        Ok(other) => panic!(
            "convergence reported {other:?} instead of the identity change — the user is \
             offered a session-level remedy that can never converge"
        ),
        Err(e) => panic!("expected UntrustedIdentity, got {e:?}"),
    }
}
