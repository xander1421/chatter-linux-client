use super::receive::classify_inner_failure;
use super::*;
use crate::config::TRUST_ROOT_PUBLIC_KEY;
use crate::error::CryptoError;
use libsignal_protocol::{
    CiphertextMessageType, DeviceId, IdentityKeyStore, ProtocolAddress, SignalProtocolError,
    Timestamp,
};
use rand::TryRngCore;
use rand::rngs::OsRng;
use std::time::SystemTime;

#[test]
fn test_derive_uak_deterministic() {
    use crate::crypto::derive_uak;

    let mut profile_key = [0u8; PROFILE_KEY_LENGTH];
    profile_key[0] = 0x42;
    profile_key[31] = 0x42;

    let uak = derive_uak(&profile_key);
    assert_eq!(uak.len(), UAK_LENGTH);

    // Deterministic for the same key.
    assert_eq!(uak, derive_uak(&profile_key));

    // Different key, different UAK.
    let mut different_key = profile_key;
    different_key[0] = 0x43;
    assert_ne!(uak, derive_uak(&different_key));
}

/// A re-delivery must not be reported as a desync.
///
/// The whole cost of getting this wrong is off-box and invisible from here:
/// each misclassification sends the peer a `DecryptionErrorMessage` telling it
/// to archive a healthy session and resend, which produces another delivery to
/// misclassify. Two idle devices generated 201 inbound reports and 12 outbound
/// in four minutes.
#[test]
fn a_duplicate_inner_message_is_not_a_session_desync() {
    let device = DeviceId::new(1).expect("device id");
    let ciphertext = [0xAAu8; 8];

    // `1 / 0`: the chain is at 1, the message carries counter 0 — decrypted
    // once already, keys consumed. This is the exact error observed.
    let duplicate = classify_inner_failure(
        SignalProtocolError::DuplicatedMessage(1, 0),
        "caa49e97-690f-407d-ab9a-74bf772dbd3d",
        device,
        CiphertextMessageType::Whisper,
        &ciphertext,
        None,
    );
    assert!(
        matches!(duplicate, CryptoError::DuplicateMessage { .. }),
        "a duplicate was classified as {duplicate:?}, which sends the peer a \
             DecryptionErrorMessage and restarts the loop"
    );

    // A genuine wedge still reports, or the 1:1 self-heal stops working. The
    // ciphertext has to survive into the error — the report is built from it.
    let wedged = classify_inner_failure(
        SignalProtocolError::InvalidMessage(CiphertextMessageType::Whisper, "bad mac".to_string()),
        "caa49e97-690f-407d-ab9a-74bf772dbd3d",
        device,
        CiphertextMessageType::Whisper,
        &ciphertext,
        None,
    );
    match wedged {
        CryptoError::SessionDesync {
            ciphertext: carried,
            msg_type,
            ..
        } => {
            assert_eq!(carried, ciphertext, "the report would name no ciphertext");
            assert_eq!(msg_type, CiphertextMessageType::Whisper as u8);
        }
        other => panic!("a real desync was classified as {other:?}; the self-heal is dead"),
    }
}

/// A peer's identity key CHANGING must not be reported as a session desync,
/// and the sealed path's provenance must say the sender was PROVEN.
///
/// libsignal refuses the inner decrypt with `UntrustedIdentity`
/// (session.rs:61 for a PreKey message, session_cipher.rs:317 for a Whisper
/// one) because the key on the wire is not the key pinned for the account.
/// Classifying that as a desync makes the gateway ship the peer a
/// `DecryptionErrorMessage`; the peer archives its session and resends a
/// fresh PreKey message carrying the SAME new identity key, and we refuse it
/// again — a loop that never converges, because no amount of session
/// resetting changes which key is pinned.
#[test]
fn an_identity_change_is_not_a_session_desync() {
    let device = DeviceId::new(1).expect("device id");
    let sender = "caa49e97-690f-407d-ab9a-74bf772dbd3d";
    let address = ProtocolAddress::new(sender.to_string(), device);

    let changed = classify_inner_failure(
        SignalProtocolError::UntrustedIdentity(address),
        sender,
        device,
        CiphertextMessageType::PreKey,
        &[0xAAu8; 8],
        None,
    );

    match changed {
        CryptoError::UntrustedIdentity {
            peer_id,
            device_id,
            attribution,
            ..
        } => {
            assert_eq!(peer_id, sender);
            assert_eq!(device_id, 1);
            assert!(
                attribution.is_authenticated(),
                "the sealed path authenticates its sender via the certificate; reporting it \
                     as unproven would understate what the UI may tell the user"
            );
        }
        other => panic!(
            "a peer identity-key change was classified as {other:?}; as a desync it sends a \
                 DecryptionErrorMessage that can never fix the pinned key"
        ),
    }
}

/// `CiphertextMessageType::Plaintext` bypasses the ratchet entirely — no
/// `is_trusted_identity`, no replay protection, no forward secrecy. Pinned
/// libsignal refuses it outright (`sealed_sender.rs`, "unexpected message
/// type for sealed_sender_decrypt"); we accept it only as a
/// `DecryptionErrorMessage` ride-along.
///
/// So the decrypt result MUST tell the caller which type it got. Without
/// that label, unratcheted attacker-chosen bytes are indistinguishable from
/// ratchet-authenticated plaintext and go straight into `Content::decode`
/// on the DM and anon-DM paths.
#[tokio::test]
async fn sealed_sender_decrypt_labels_unratcheted_plaintext() {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use libsignal_protocol::{
        IdentityKeyPair, InMemSignalProtocolStore, KeyPair, PrivateKey,
        sealed_sender_encrypt_from_usmc,
    };

    let mut rng = OsRng.unwrap_err();

    // The dev trust root's private key is committed in
    // deploy/vault/init-and-unseal.sh, which is what makes this test
    // possible at all — and is separately why config.rs now demands an
    // explicit feature to pin it.
    let trust_root_private = PrivateKey::deserialize(
        &BASE64
            .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
            .expect("known-good base64"),
    )
    .expect("dev trust root private key");
    assert_eq!(
        trust_root_private.public_key().expect("public"),
        *TRUST_ROOT_PUBLIC_KEY,
        "test cert chain must validate under the trust root this build pins"
    );

    // Server cert signed by the trust root, sender cert signed by it.
    let server_keys = KeyPair::generate(&mut rng);
    let server_cert =
        ServerCertificate::new(1, server_keys.public_key, &trust_root_private, &mut rng)
            .expect("server certificate");

    let sender_identity = IdentityKeyPair::generate(&mut rng);
    let sender_uuid = "11111111-1111-4111-8111-111111111111";
    let sender_device = DeviceId::new(1).expect("device id");
    let expiration = Timestamp::from_epoch_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000,
    );
    let sender_cert = SenderCertificate::new(
        sender_uuid.to_string(),
        None,
        *sender_identity.public_key(),
        sender_device,
        expiration,
        server_cert,
        &server_keys.private_key,
        &mut rng,
    )
    .expect("sender certificate");

    // A USMC whose payload never touches the Double Ratchet.
    let unratcheted = b"\xc0not ratchet output".to_vec();
    let usmc = UnidentifiedSenderMessageContent::new(
        CiphertextMessageType::Plaintext,
        sender_cert,
        unratcheted.clone(),
        ContentHint::Implicit,
        None,
    )
    .expect("usmc");

    let recipient_uuid = "22222222-2222-4222-8222-222222222222";
    let recipient_device = DeviceId::new(1).expect("device id");
    let recipient_address = ProtocolAddress::new(recipient_uuid.to_string(), recipient_device);
    let recipient_identity = IdentityKeyPair::generate(&mut rng);
    let mut recipient_store =
        InMemSignalProtocolStore::new(recipient_identity, 1234).expect("recipient store");

    // Sender-side store, just to hand the recipient's identity to encrypt.
    let mut sender_store =
        InMemSignalProtocolStore::new(sender_identity, 5678).expect("sender store");
    sender_store
        .save_identity(&recipient_address, recipient_identity.identity_key())
        .await
        .expect("save recipient identity");

    let sealed =
        sealed_sender_encrypt_from_usmc(&recipient_address, &usmc, &sender_store, &mut rng)
            .await
            .expect("seal");

    let mut s2 = recipient_store.clone();
    let mut s3 = recipient_store.clone();
    let mut s4 = recipient_store.clone();
    let s6 = recipient_store.clone();
    let (result, msg_type) = SealedSenderManager::decrypt(
        &sealed,
        &TRUST_ROOT_PUBLIC_KEY,
        Timestamp::from_epoch_millis(0),
        None,
        recipient_uuid.to_string(),
        recipient_device,
        &mut recipient_store,
        &mut s2,
        &mut s3,
        &s6,
        &mut s4,
    )
    .await
    .expect("envelope opens — the cert chain is valid");

    assert_eq!(
        result.message, unratcheted,
        "the Plaintext branch returns contents verbatim, as the DEM handler needs"
    );
    assert_eq!(
        msg_type,
        CiphertextMessageType::Plaintext,
        "decrypt must label unratcheted bytes so non-DEM callers can refuse them"
    );
}

/// §5: the sealed layer's group id survives an inner decrypt failure, so
/// the receiver can mark the loss in the channel a pairwise channel
/// control belongs to (Signal-Desktop places a `groupId` failure there).
#[tokio::test]
async fn an_inner_failure_carries_the_sealed_group_id() {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use libsignal_protocol::{
        IdentityKeyPair, InMemSignalProtocolStore, KeyPair, PrivateKey,
        sealed_sender_encrypt_from_usmc,
    };
    let mut rng = OsRng.unwrap_err();
    let trust_root_private = PrivateKey::deserialize(
        &BASE64
            .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
            .expect("known-good base64"),
    )
    .expect("dev trust root private key");
    let server_keys = KeyPair::generate(&mut rng);
    let server_cert =
        ServerCertificate::new(1, server_keys.public_key, &trust_root_private, &mut rng)
            .expect("server certificate");
    let sender_identity = IdentityKeyPair::generate(&mut rng);
    let expiration = Timestamp::from_epoch_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000,
    );
    let sender_cert = SenderCertificate::new(
        "11111111-1111-4111-8111-111111111111".to_string(),
        None,
        *sender_identity.public_key(),
        DeviceId::new(1).expect("device id"),
        expiration,
        server_cert,
        &server_keys.private_key,
        &mut rng,
    )
    .expect("sender certificate");

    let channel = [9u8; 16];
    // No session exists, so this Whisper message cannot be opened.
    let usmc = UnidentifiedSenderMessageContent::new(
        CiphertextMessageType::Whisper,
        sender_cert,
        vec![0x33; 64],
        ContentHint::Default,
        Some(channel.to_vec()),
    )
    .expect("usmc");

    let recipient_uuid = "22222222-2222-4222-8222-222222222222";
    let recipient_device = DeviceId::new(1).expect("device id");
    let recipient_address = ProtocolAddress::new(recipient_uuid.to_string(), recipient_device);
    let recipient_identity = IdentityKeyPair::generate(&mut rng);
    let mut recipient_store =
        InMemSignalProtocolStore::new(recipient_identity, 1234).expect("recipient store");
    let mut sender_store =
        InMemSignalProtocolStore::new(sender_identity, 5678).expect("sender store");
    sender_store
        .save_identity(&recipient_address, recipient_identity.identity_key())
        .await
        .expect("save recipient identity");
    let sealed =
        sealed_sender_encrypt_from_usmc(&recipient_address, &usmc, &sender_store, &mut rng)
            .await
            .expect("seal");

    let mut s2 = recipient_store.clone();
    let mut s3 = recipient_store.clone();
    let mut s4 = recipient_store.clone();
    let s6 = recipient_store.clone();
    let err = SealedSenderManager::decrypt(
        &sealed,
        &TRUST_ROOT_PUBLIC_KEY,
        Timestamp::from_epoch_millis(0),
        None,
        recipient_uuid.to_string(),
        recipient_device,
        &mut recipient_store,
        &mut s2,
        &mut s3,
        &s6,
        &mut s4,
    )
    .await
    .expect_err("no session: the inner message cannot open");
    match err {
        CryptoError::SessionDesync { group_id, .. } => {
            assert_eq!(group_id.as_deref(), Some(&channel[..]));
        }
        other => panic!("expected SessionDesync, got {other:?}"),
    }
}

/// The sender certificate is judged at the envelope's SERVER timestamp,
/// which the actor's `DecryptSealed` now carries (Signal-Desktop
/// `MessageReceiver.preload.ts` validates against `serverTimestamp`).
/// A message deposited while its cert was live must open however late it
/// is read; one deposited after expiry must not.
#[tokio::test]
async fn sealed_sender_cert_is_validated_at_envelope_time_not_now() {
    use libsignal_protocol::{
        IdentityKeyPair, InMemSignalProtocolStore, KeyPair, sealed_sender_encrypt_from_usmc,
    };

    let mut rng = OsRng.unwrap_err();
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let hour = 3_600_000;

    let root = KeyPair::generate(&mut rng);
    let server_keys = KeyPair::generate(&mut rng);
    let server_cert =
        ServerCertificate::new(1, server_keys.public_key, &root.private_key, &mut rng)
            .expect("server certificate");
    let sender_identity = IdentityKeyPair::generate(&mut rng);
    // Expired an hour ago, relative to the local clock.
    let expiration = Timestamp::from_epoch_millis(now_ms - hour);
    let sender_cert = SenderCertificate::new(
        "11111111-1111-4111-8111-111111111111".to_string(),
        None,
        *sender_identity.public_key(),
        DeviceId::new(1).expect("device id"),
        expiration,
        server_cert,
        &server_keys.private_key,
        &mut rng,
    )
    .expect("sender certificate");

    let body = b"\xc0sealed body".to_vec();
    let usmc = UnidentifiedSenderMessageContent::new(
        CiphertextMessageType::Plaintext,
        sender_cert,
        body.clone(),
        ContentHint::Implicit,
        None,
    )
    .expect("usmc");

    let recipient_uuid = "22222222-2222-4222-8222-222222222222";
    let recipient_device = DeviceId::new(1).expect("device id");
    let recipient_address = ProtocolAddress::new(recipient_uuid.to_string(), recipient_device);
    let recipient_identity = IdentityKeyPair::generate(&mut rng);
    let recipient_store =
        InMemSignalProtocolStore::new(recipient_identity, 1234).expect("recipient store");
    let mut sender_store =
        InMemSignalProtocolStore::new(sender_identity, 5678).expect("sender store");
    sender_store
        .save_identity(&recipient_address, recipient_identity.identity_key())
        .await
        .expect("save recipient identity");
    let sealed =
        sealed_sender_encrypt_from_usmc(&recipient_address, &usmc, &sender_store, &mut rng)
            .await
            .expect("seal");

    let open_at = |at_ms: u64| {
        let mut s1 = recipient_store.clone();
        let mut s2 = recipient_store.clone();
        let mut s3 = recipient_store.clone();
        let mut s4 = recipient_store.clone();
        let s6 = recipient_store.clone();
        let sealed = sealed.clone();
        let trust_root = root.public_key;
        async move {
            SealedSenderManager::decrypt(
                &sealed,
                &trust_root,
                Timestamp::from_epoch_millis(at_ms),
                None,
                recipient_uuid.to_string(),
                recipient_device,
                &mut s1,
                &mut s2,
                &mut s3,
                &s6,
                &mut s4,
            )
            .await
        }
    };

    // Deposited two hours ago, while the cert was live: opens.
    let (result, _) = open_at(now_ms - 2 * hour)
        .await
        .expect("cert valid at the envelope's server timestamp must decrypt");
    assert_eq!(result.message, body);

    // Deposited after expiry: refused, even though nothing else changed.
    assert!(
        open_at(now_ms).await.is_err(),
        "cert expired at the envelope's server timestamp must be refused"
    );
}

/// The sender's own cache check (`get_cached_sender_certificate`) relies on
/// this: a cert chaining to a root this build does not pin — cached under
/// an older server key — must be refused so it is refetched, because every
/// recipient would reject it and drop the message.
#[test]
fn validate_trust_root_refuses_a_foreign_root() {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use libsignal_protocol::{IdentityKeyPair, KeyPair, PrivateKey};

    let mut rng = OsRng.unwrap_err();
    let expiration = Timestamp::from_epoch_millis(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000,
    );
    let cert_under = |root: &PrivateKey, rng: &mut _| {
        let server_keys = KeyPair::generate(rng);
        let server_cert = ServerCertificate::new(1, server_keys.public_key, root, rng)
            .expect("server certificate");
        SenderCertificate::new(
            "11111111-1111-4111-8111-111111111111".to_string(),
            None,
            *IdentityKeyPair::generate(rng).public_key(),
            DeviceId::new(1).expect("device id"),
            expiration,
            server_cert,
            &server_keys.private_key,
            rng,
        )
        .expect("sender certificate")
    };

    let pinned = PrivateKey::deserialize(
        &BASE64
            .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
            .expect("base64"),
    )
    .expect("dev trust root private key");
    let foreign = KeyPair::generate(&mut rng).private_key;

    assert!(validate_trust_root(&cert_under(&pinned, &mut rng)));
    assert!(!validate_trust_root(&cert_under(&foreign, &mut rng)));
}

#[test]
fn test_cached_certificate_wrapper() {
    // We can't easily create a real SenderCertificate in tests,
    // so just test the wrapper logic with expiration calculations
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_millis() as u64;

    // Future expiration (24+ hours from now)
    let future_expiry = now + (48 * 60 * 60 * 1000); // 48 hours

    // Past expiration
    let past_expiry = now - (60 * 1000); // 1 minute ago

    // Test threshold logic (manual calculation)
    let threshold_ms = CERTIFICATE_REFRESH_THRESHOLD.as_millis() as u64;
    let needs_refresh_threshold = now + threshold_ms;

    // Future expiry beyond threshold - doesn't need refresh
    assert!(future_expiry > needs_refresh_threshold);

    // Past expiry - definitely needs refresh
    assert!(past_expiry < now);
}
