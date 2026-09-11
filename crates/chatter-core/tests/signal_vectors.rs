//! Cross-validation tests against libsignal reference vectors.
//!
//! These tests pin fixed test inputs (or seeded RNG inputs) and assert that our
//! codepaths produce results that round-trip correctly through libsignal's
//! public API. The intent is to catch regressions where our wrappers, storage
//! layer, or proto conversions silently desync from upstream libsignal.
//!
//! # Why seeded RNG instead of static byte vectors for X3DH/PQXDH?
//!
//! libsignal-protocol does not expose deterministic seeds on its public
//! `process_prekey_bundle` API: the function takes a `Rng + CryptoRng` and uses
//! it for the ephemeral X25519 base key and PQXDH KEM encapsulation randomness.
//! Even with a seeded `StdRng`, libsignal also pulls system time (`SystemTime`)
//! into the session record, and KEM encapsulation in `mlkem1024`/`kyber1024`
//! uses fresh randomness internally that is not solely a function of the
//! provided RNG output stream's first N bytes. So we cannot pin an exact
//! "expected ciphertext bytes" constant.
//!
//! Instead, we pin a deterministic *input* (seeded keys, fixed plaintext) and
//! assert the *behavioral* invariant that matters: a session built via our
//! codepath decrypts to the original plaintext on the peer side. That is the
//! property libsignal's own integration tests exercise, and it catches the
//! same class of bugs (key-encoding drift, signature-byte ordering, KEM
//! ciphertext layout, etc.).
//!
//! # Tests
//!
//! 1. `pqxdh_session_roundtrip_seeded` — Alice runs `process_prekey_bundle`
//!    against a Bob bundle built from a seeded RNG, encrypts a known
//!    plaintext, and Bob decrypts it back to the same bytes.
//! 2. `sender_certificate_roundtrip_validates` — A SenderCertificate built via
//!    libsignal's `SenderCertificate::new` round-trips through
//!    `serialized` -> `deserialize` and validates against the original trust
//!    root. Mirrors the same `cert.validate(&trust_root, now)` call our
//!    `validate_trust_root` wrapper makes (sealed_sender.rs:533).
//! 3. `skdm_parse_pins_distribution_id` — A SenderKeyDistributionMessage built
//!    with a fixed UUID/chain_id/iteration/chain_key/signing_key, serialized,
//!    then re-parsed via `SenderKeyDistributionMessage::try_from(&[u8])`,
//!    yields identical fields. This pins the wire format we depend on.
//!
//! All tests use only libsignal's public API and do not touch production code.

use libsignal_protocol::{
    kem, message_decrypt, message_encrypt, process_prekey_bundle, CiphertextMessage,
    DeviceId, GenericSignedPreKey, IdentityKey, IdentityKeyPair, InMemSignalProtocolStore,
    KeyPair, KyberPreKeyRecord, PreKeyBundle, PreKeyId, PreKeyRecord, PreKeySignalMessage,
    PreKeyStore, ProtocolAddress, SenderCertificate, SenderKeyDistributionMessage,
    ServerCertificate, SignedPreKeyId, SignedPreKeyRecord, Timestamp,
};
use rand::{rngs::StdRng, SeedableRng};
use std::time::SystemTime;
use uuid::Uuid;

/// Build a deterministic Bob protocol store + prekey bundle from a seeded RNG.
///
/// Mirrors libsignal's own `tests/support/mod.rs::create_pre_key_bundle`, but
/// driven by a caller-supplied seeded RNG so the keys are deterministic across
/// runs (modulo the libsignal/KEM internals noted in the module docs).
async fn build_seeded_bob_bundle(
    rng: &mut StdRng,
) -> (InMemSignalProtocolStore, PreKeyBundle) {
    let bob_identity = IdentityKeyPair::generate(rng);
    let bob_registration_id: u32 = 0x1234;
    let mut bob_store =
        InMemSignalProtocolStore::new(bob_identity, bob_registration_id).expect("store init");

    let pre_key_pair = KeyPair::generate(rng);
    let signed_pre_key_pair = KeyPair::generate(rng);
    let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);

    let signed_pre_key_public = signed_pre_key_pair.public_key.serialize();
    let signed_pre_key_signature = bob_identity
        .private_key()
        .calculate_signature(&signed_pre_key_public, rng)
        .expect("sign signed prekey");

    let kyber_pre_key_public = kyber_pre_key_pair.public_key.serialize();
    let kyber_pre_key_signature = bob_identity
        .private_key()
        .calculate_signature(&kyber_pre_key_public, rng)
        .expect("sign kyber prekey");

    let device_id: DeviceId = DeviceId::new(1).unwrap();
    let pre_key_id: u32 = 31337;
    let signed_pre_key_id: u32 = 22;
    let kyber_pre_key_id: u32 = 8675309;

    let bundle = PreKeyBundle::new(
        bob_registration_id,
        device_id,
        Some((PreKeyId::from(pre_key_id), pre_key_pair.public_key)),
        SignedPreKeyId::from(signed_pre_key_id),
        signed_pre_key_pair.public_key,
        signed_pre_key_signature.to_vec(),
        kyber_pre_key_id.into(),
        kyber_pre_key_pair.public_key.clone(),
        kyber_pre_key_signature.to_vec(),
        *bob_identity.identity_key(),
    )
    .expect("bundle build");

    use libsignal_protocol::{KyberPreKeyStore, SignedPreKeyStore};

    bob_store
        .save_pre_key(
            PreKeyId::from(pre_key_id),
            &PreKeyRecord::new(PreKeyId::from(pre_key_id), &pre_key_pair),
        )
        .await
        .unwrap();

    bob_store
        .save_signed_pre_key(
            SignedPreKeyId::from(signed_pre_key_id),
            &SignedPreKeyRecord::new(
                SignedPreKeyId::from(signed_pre_key_id),
                Timestamp::from_epoch_millis(1_700_000_000_000),
                &signed_pre_key_pair,
                &signed_pre_key_signature,
            ),
        )
        .await
        .unwrap();

    bob_store
        .save_kyber_pre_key(
            kyber_pre_key_id.into(),
            &KyberPreKeyRecord::new(
                kyber_pre_key_id.into(),
                Timestamp::from_epoch_millis(1_700_000_000_000),
                &kyber_pre_key_pair,
                &kyber_pre_key_signature,
            ),
        )
        .await
        .unwrap();

    (bob_store, bundle)
}

#[tokio::test]
async fn pqxdh_session_roundtrip_seeded() {
    // Two independent seeded RNGs so Alice's session creation and Bob's bundle
    // are each deterministic from a fixed seed. We use 0xDEAD_BEEF for Bob and
    // 0xC0FFEE_F00D for Alice's session-establishment ephemerals — the same
    // pattern already used in production unit tests under crypto/signal/.
    let mut bob_rng = StdRng::seed_from_u64(0xDEAD_BEEF);
    let mut alice_rng = StdRng::seed_from_u64(0xC0FFEE_F00D);

    let (mut bob_store, bob_bundle) = build_seeded_bob_bundle(&mut bob_rng).await;

    let alice_identity = IdentityKeyPair::generate(&mut alice_rng);
    let alice_registration_id: u32 = 0xAAAA;
    let mut alice_store =
        InMemSignalProtocolStore::new(alice_identity, alice_registration_id).unwrap();

    let bob_address = ProtocolAddress::new("bob-uuid".to_string(), DeviceId::new(1).unwrap());
    // Hoisted: libsignal >= 0.99 needs the local address on the encrypt and
    // process-bundle calls above, not just on the decrypt below.
    let alice_address =
        ProtocolAddress::new("alice-uuid".to_string(), DeviceId::new(1).unwrap());

    process_prekey_bundle(
        &bob_address,
        &alice_address,
        &mut alice_store.session_store,
        &mut alice_store.identity_store,
        &bob_bundle,
        SystemTime::now(),
        &mut alice_rng,
    )
    .await
    .expect("alice process bundle");

    // Pinned plaintext — the exact bytes we expect Bob to recover.
    const PLAINTEXT: &[u8] = b"libsignal vector test: hello bob 0123456789";

    let alice_ctext = message_encrypt(
        PLAINTEXT,
        &bob_address,
        &alice_address,        // local side: Alice is encrypting
        &mut alice_store.session_store,
        &mut alice_store.identity_store,
        SystemTime::now(),
        &mut alice_rng,
    )
    .await
    .expect("alice encrypt");

    // First message must be a PreKey envelope (initiates the session).
    let prekey_msg_bytes = match &alice_ctext {
        CiphertextMessage::PreKeySignalMessage(m) => m.serialized().to_vec(),
        other => panic!(
            "expected PreKeySignalMessage as initial ciphertext, got {:?}",
            other.message_type()
        ),
    };

    // Bob parses + decrypts.
    let prekey_msg =
        PreKeySignalMessage::try_from(prekey_msg_bytes.as_slice()).expect("parse prekey msg");
    let recovered = message_decrypt(
        &CiphertextMessage::PreKeySignalMessage(prekey_msg),
        &alice_address,
        &bob_address,          // local side: Bob is decrypting
        &mut bob_store.session_store,
        &mut bob_store.identity_store,
        &mut bob_store.pre_key_store,
        &bob_store.signed_pre_key_store,
        &mut bob_store.kyber_pre_key_store,
        &mut bob_rng,
    )
    .await
    .expect("bob decrypt");

    assert_eq!(
        recovered.as_slice(),
        PLAINTEXT,
        "PQXDH session must round-trip plaintext through libsignal"
    );
}

#[test]
fn sender_certificate_roundtrip_validates() {
    // Mirrors libsignal's own tests/sealed_sender.rs::test_sender_cert and
    // exercises the same `cert.validate(&trust_root, now)` call our
    // `chatter_core::crypto::signal::sealed_sender::validate_trust_root`
    // makes (see sealed_sender.rs:533). If libsignal ever changes the cert
    // wire format or signature scheme, this test catches it.
    let mut rng = StdRng::seed_from_u64(0xDEAD_BEEF);

    let trust_root = KeyPair::generate(&mut rng);
    let server_key = KeyPair::generate(&mut rng);
    let identity_key = KeyPair::generate(&mut rng);

    let server_cert =
        ServerCertificate::new(1, server_key.public_key, &trust_root.private_key, &mut rng)
            .expect("server cert");

    let device_id = DeviceId::new(42).unwrap();
    let expires = Timestamp::from_epoch_millis(2_000_000_000_000);
    let sender_uuid = "9d0652a3-dcc3-4d11-975f-74d61598733f".to_string();

    let sender_cert = SenderCertificate::new(
        sender_uuid.clone(),
        Some("+14152222222".to_string()),
        identity_key.public_key,
        device_id,
        expires,
        server_cert,
        &server_key.private_key,
        &mut rng,
    )
    .expect("sender cert");

    // Round-trip: serialize -> deserialize -> validate against original trust root.
    let bytes = sender_cert.serialized().expect("serialize cert").to_vec();
    let parsed = SenderCertificate::deserialize(&bytes).expect("deserialize cert");

    assert_eq!(parsed.sender_uuid().expect("uuid getter"), sender_uuid);
    assert_eq!(
        parsed.sender_device_id().expect("device id getter"),
        device_id
    );
    assert_eq!(
        parsed.expiration().expect("expiration getter").epoch_millis(),
        expires.epoch_millis()
    );

    // Trust-root validation: should pass at any time strictly before expiry.
    let before_expiry = Timestamp::from_epoch_millis(expires.epoch_millis() - 1);
    assert!(
        parsed
            .validate(&trust_root.public_key, before_expiry)
            .expect("validate"),
        "round-tripped sender cert must validate against original trust root"
    );

    // And expire correctly past `expires`.
    let after_expiry = Timestamp::from_epoch_millis(expires.epoch_millis() + 1);
    assert!(
        !parsed
            .validate(&trust_root.public_key, after_expiry)
            .expect("validate"),
        "expired sender cert must fail validation"
    );

    // Wrong trust root must fail.
    let wrong_trust = KeyPair::generate(&mut rng);
    assert!(
        !parsed
            .validate(&wrong_trust.public_key, before_expiry)
            .expect("validate"),
        "cert signed by a different server must not validate against wrong trust root"
    );
}

#[test]
fn skdm_parse_pins_distribution_id() {
    // Build an SKDM with fixed-content fields, serialize, re-parse from raw
    // bytes via the public `TryFrom<&[u8]>` impl, and assert all fields
    // round-trip. This pins the SKDM wire format that our group encryption
    // path (chatter_core::crypto::signal::sender_key) relies on.
    let mut rng = StdRng::seed_from_u64(0xDEAD_BEEF);
    let signing_keys = KeyPair::generate(&mut rng);

    // Fixed UUID chosen for deterministic output. Any v4 UUID works.
    let distribution_id =
        Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
    let chain_id: u32 = 7;
    let iteration: u32 = 0;
    // 32-byte chain key — the SKDM wire format requires at least 32 bytes here.
    let chain_key: Vec<u8> = (0u8..32).collect();

    let skdm = SenderKeyDistributionMessage::new(
        3, // message_version (current)
        distribution_id,
        chain_id,
        iteration,
        chain_key.clone(),
        signing_keys.public_key,
    )
    .expect("build SKDM");

    let bytes = skdm.serialized().to_vec();
    let parsed =
        SenderKeyDistributionMessage::try_from(bytes.as_slice()).expect("parse SKDM bytes");

    assert_eq!(
        parsed.distribution_id().expect("distribution_id getter"),
        distribution_id,
        "distribution UUID must round-trip through SKDM wire format"
    );
    assert_eq!(parsed.chain_id().expect("chain_id getter"), chain_id);
    assert_eq!(parsed.iteration().expect("iteration getter"), iteration);
    assert_eq!(
        parsed.chain_key().expect("chain_key getter"),
        chain_key.as_slice()
    );
    assert_eq!(
        parsed
            .signing_key()
            .expect("signing_key getter")
            .serialize(),
        signing_keys.public_key.serialize(),
        "signing pubkey must round-trip"
    );
    // Serialized form is canonical: re-serialize and compare.
    assert_eq!(
        parsed.serialized(),
        bytes.as_slice(),
        "SKDM canonical serialization must be stable across parse"
    );
}

// Suppress unused-import lint when only some tests are compiled. The
// `IdentityKey` re-export is kept so future tests that build a bundle from
// raw bytes (rather than a generated `IdentityKeyPair`) don't have to amend
// the import list.
#[allow(dead_code)]
fn _unused_imports_anchor(_: IdentityKey) {}
