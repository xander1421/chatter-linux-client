//! Conformance tests for the constructions that are bitchat's own.
//!
//! `tests/noise_vectors.rs` proves that our Noise is *the* Noise, against the
//! canonical specification corpus. It cannot say anything about the layer
//! bitchat built on top, because that layer has no published vectors — the two
//! domain-separating prologues, the one-shot `X` seal framing, and the packet
//! signing rule are all bitchat's inventions.
//!
//! This file is where those are pinned. Every expected byte string here was
//! derived by hand from the bitchat source, and the source line is cited at each
//! one so a reviewer can check the derivation rather than trust it:
//!
//! - `Services/NoiseEncryptionService.swift:382` — `courierPrologue`
//! - `Services/NoiseEncryptionService.swift:427` — `prekeyProloguePrefix`
//! - `Services/NoiseEncryptionService.swift:429` — `prekeyPrologue(for:)`
//! - `BitFoundation/BitchatPacket.swift` — `toBinaryDataForSigning()`
//!
//! The hex literals are written out in full rather than recomputed from the
//! crate's own constants wherever that is possible. A test that rebuilds the
//! expected value with the same code path as the implementation proves only that
//! the code is self-consistent; these prove it is *correct*.

use chatter_mesh::BitchatPacket;
use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};
use chatter_mesh_noise::identity::{MeshIdentity, fingerprint_of};
use chatter_mesh_noise::seal::{
    open_courier, open_prekey, prekey_prologue, seal_courier, seal_prekey,
};
use chatter_mesh_noise::sign::{sign_packet, verify_packet};
use chatter_mesh_noise::{ANNOUNCE_CONTEXT, COURIER_PROLOGUE, PREKEY_PROLOGUE_PREFIX};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const SENDER_STATIC: [u8; 32] = [0x11; 32];
const SENDER_SIGNING: [u8; 32] = [0x22; 32];
const RECIPIENT_STATIC: [u8; 32] = [0x33; 32];
const RECIPIENT_SIGNING: [u8; 32] = [0x44; 32];
const MALLORY_STATIC: [u8; 32] = [0x55; 32];
const MALLORY_SIGNING: [u8; 32] = [0x66; 32];
const PREKEY_SECRET: [u8; 32] = [0x77; 32];
const PREKEY_ID: u32 = 0x0102_0304;

const TIMESTAMP: u64 = 1_700_000_000_123;

fn sender() -> MeshIdentity {
    MeshIdentity::from_keys(SENDER_STATIC, SENDER_SIGNING)
}

fn recipient() -> MeshIdentity {
    MeshIdentity::from_keys(RECIPIENT_STATIC, RECIPIENT_SIGNING)
}

fn mallory() -> MeshIdentity {
    MeshIdentity::from_keys(MALLORY_STATIC, MALLORY_SIGNING)
}

/// Curve25519 public key for a raw secret, without reaching past this crate's
/// API: `MeshIdentity` derives exactly the key `snow` would.
fn public_of(secret: [u8; 32]) -> [u8; 32] {
    MeshIdentity::from_keys(secret, [0u8; 32]).static_public()
}

// ---------------------------------------------------------------------------
// Prologues — domain separation
// ---------------------------------------------------------------------------

#[test]
fn courier_prologue_is_exactly_the_bitchat_string() {
    // `Data("bitchat-courier-v1".utf8)` — 18 bytes, no length prefix, no
    // trailing NUL. A UTF-8 string constant in Swift and a Rust byte literal
    // agree here only because the string is pure ASCII; spell the bytes out so
    // that stays true by assertion rather than by luck.
    assert_eq!(COURIER_PROLOGUE, b"bitchat-courier-v1");
    assert_eq!(COURIER_PROLOGUE.len(), 18);
    assert_eq!(
        hex::encode(COURIER_PROLOGUE),
        "626974636861742d636f75726965722d7631"
    );
}

#[test]
fn prekey_prologue_is_the_prefix_followed_by_a_big_endian_id() {
    // `prekeyPrologue(for:)`: `prefix ‖ prekeyID.bigEndian`, 17 + 4 = 21 bytes.
    assert_eq!(PREKEY_PROLOGUE_PREFIX, b"bitchat-prekey-v1");
    assert_eq!(PREKEY_PROLOGUE_PREFIX.len(), 17);
    assert_eq!(
        hex::encode(PREKEY_PROLOGUE_PREFIX),
        "626974636861742d7072656b65792d7631"
    );

    let prologue = prekey_prologue(PREKEY_ID);
    assert_eq!(prologue.len(), 21);
    assert_eq!(
        hex::encode(&prologue),
        "626974636861742d7072656b65792d763101020304",
        "prekey 0x01020304 must serialise most-significant byte first"
    );

    // Small IDs are the ones that hide an endianness bug: `id = 7` is
    // `00000007` big-endian and `07000000` little-endian, and only the first is
    // interoperable. Getting this backwards fails *open* — the seal still
    // decrypts against our own implementation and is rejected by every real
    // peer, which is the worst failure mode available.
    assert_eq!(
        hex::encode(prekey_prologue(7)),
        "626974636861742d7072656b65792d763100000007"
    );
    assert_eq!(
        hex::encode(prekey_prologue(0)),
        "626974636861742d7072656b65792d763100000000"
    );
    assert_eq!(
        hex::encode(prekey_prologue(u32::MAX)),
        "626974636861742d7072656b65792d7631ffffffff"
    );

    // The prefix is a strict prefix of every full prologue, and the four ID
    // bytes are the only thing that varies.
    assert!(prologue.starts_with(PREKEY_PROLOGUE_PREFIX));
    assert_ne!(
        prekey_prologue(7),
        prekey_prologue(7u32.swap_bytes()),
        "byte order must be observable in the prologue"
    );
}

#[test]
fn the_domain_strings_are_prefix_free_of_one_another() {
    // Domain separation is only sound if no domain's bytes are a prefix of
    // another's — otherwise a prologue for one construction can be extended
    // into a prologue for a different one.
    let domains: [&[u8]; 3] = [COURIER_PROLOGUE, PREKEY_PROLOGUE_PREFIX, ANNOUNCE_CONTEXT];
    for i in 0..domains.len() {
        for j in 0..domains.len() {
            if i == j {
                continue;
            }
            assert!(
                !domains[i].starts_with(domains[j]),
                "{} is a prefix of {}",
                String::from_utf8_lossy(domains[j]),
                String::from_utf8_lossy(domains[i])
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Seals — sender authentication and prologue binding
// ---------------------------------------------------------------------------

#[test]
fn opening_a_courier_seal_returns_the_senders_static_key() {
    // The whole point of the `X` pattern here: the initiator's static key rides
    // inside the ciphertext, encrypted, so a successful open *is* an
    // authentication of the sender. A seal that yields the right payload while
    // losing the sender's identity is a silent security failure, not a bug in
    // an accessor.
    let alice = sender();
    let bob = recipient();
    let payload = b"meet at the usual place".as_slice();

    let ciphertext =
        seal_courier(payload, &bob.static_public(), &alice).expect("courier seal must build");
    assert_ne!(
        ciphertext, payload,
        "the payload must not travel in the clear"
    );

    let (opened, sender_static) = open_courier(&ciphertext, &bob).expect("courier seal must open");
    assert_eq!(opened, payload);
    assert_eq!(
        sender_static,
        alice.static_public(),
        "the recovered sender key must be the real sender's"
    );
    assert_eq!(
        fingerprint_of(&sender_static),
        alice.fingerprint(),
        "and it must fingerprint to the sender's stable identity"
    );
}

#[test]
fn a_courier_seal_does_not_open_for_anyone_else() {
    let alice = sender();
    let bob = recipient();
    let ciphertext =
        seal_courier(b"for bob only", &bob.static_public(), &alice).expect("seal must build");

    assert!(
        open_courier(&ciphertext, &mallory()).is_err(),
        "a seal addressed to bob must not open under mallory's key"
    );

    let mut tampered = ciphertext.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
        open_courier(&tampered, &bob).is_err(),
        "a flipped tag byte must fail authentication"
    );

    let mut truncated = ciphertext;
    truncated.pop();
    assert!(
        open_courier(&truncated, &bob).is_err(),
        "a truncated seal must fail rather than yield a short payload"
    );
}

#[test]
fn opening_a_prekey_seal_returns_the_senders_static_key() {
    let alice = sender();
    let bob = recipient();
    let payload = b"forward-secret mail".as_slice();

    let ciphertext = seal_prekey(payload, PREKEY_ID, &public_of(PREKEY_SECRET), &alice)
        .expect("prekey seal builds");
    let (opened, sender_static) = open_prekey(&ciphertext, PREKEY_ID, &PREKEY_SECRET, &bob)
        .expect("prekey seal must open under its own ID");

    assert_eq!(opened, payload);
    assert_eq!(
        sender_static,
        alice.static_public(),
        "prekey seals authenticate the sender exactly as courier seals do"
    );
}

#[test]
fn a_prekey_seal_only_opens_under_the_id_it_was_sealed_to() {
    // The ID lives in the prologue for exactly this reason. If it were outside
    // the transcript, a ciphertext could be replayed against a different prekey.
    let alice = sender();
    let bob = recipient();
    let ciphertext = seal_prekey(b"once only", PREKEY_ID, &public_of(PREKEY_SECRET), &alice)
        .expect("seal builds");

    assert!(open_prekey(&ciphertext, PREKEY_ID, &PREKEY_SECRET, &bob).is_ok());

    for wrong_id in [
        PREKEY_ID.wrapping_add(1),
        PREKEY_ID.wrapping_sub(1),
        0,
        u32::MAX,
        PREKEY_ID.swap_bytes(),
    ] {
        assert!(
            open_prekey(&ciphertext, wrong_id, &PREKEY_SECRET, &bob).is_err(),
            "prekey ID {wrong_id:#010x} must not open a seal made for {PREKEY_ID:#010x}"
        );
    }
}

#[test]
fn courier_and_prekey_seals_are_domain_separated() {
    // Deliberately give the prekey the *same* key material as bob's identity
    // static. Then the only difference between the two constructions is the
    // prologue, and a cross-open that succeeds proves the prologue is not
    // actually being mixed in.
    let alice = sender();
    let bob = recipient();
    let payload = b"same keys, different domain".as_slice();

    let courier = seal_courier(payload, &bob.static_public(), &alice).expect("seals");
    let prekey_sealed =
        seal_prekey(payload, PREKEY_ID, &bob.static_public(), &alice).expect("seals");

    assert_ne!(
        courier, prekey_sealed,
        "identical keys and payload must still produce different ciphertexts"
    );

    assert!(
        open_prekey(&courier, PREKEY_ID, bob.static_secret(), &bob).is_err(),
        "a courier seal must not open as a prekey seal"
    );
    assert!(
        open_courier(&prekey_sealed, &bob).is_err(),
        "a prekey seal must not open as a courier seal"
    );

    // Each still opens in its own domain, so the failures above are the
    // prologue talking and not broken key material.
    assert!(open_courier(&courier, &bob).is_ok());
    assert!(open_prekey(&prekey_sealed, PREKEY_ID, bob.static_secret(), &bob).is_ok());
}

#[test]
fn an_empty_payload_seals_and_opens() {
    // Seals carry control envelopes as well as messages; a zero-length payload
    // must be a valid ciphertext, not an edge case that panics.
    let alice = sender();
    let bob = recipient();
    let ciphertext = seal_courier(&[], &bob.static_public(), &alice).expect("empty seal builds");
    let (opened, sender_static) = open_courier(&ciphertext, &bob).expect("empty seal opens");
    assert!(opened.is_empty());
    assert_eq!(sender_static, alice.static_public());
}

// ---------------------------------------------------------------------------
// Packet signing — over the padded preimage, with ttl = 0
// ---------------------------------------------------------------------------

fn signed_packet(ttl: u8) -> BitchatPacket {
    BitchatPacket::new(
        MessageType::Message,
        [0xAB; SENDER_ID_LEN],
        Some([0xCD; SENDER_ID_LEN]),
        TIMESTAMP,
        b"hello mesh".to_vec(),
        ttl,
    )
}

#[test]
fn a_packet_signature_survives_relaying() {
    // This is the property the whole preimage rule exists for. Every relay
    // decrements the TTL; if the TTL were inside the signed bytes, a signature
    // would be valid for exactly one hop and the mesh would not work at all.
    let alice = sender();
    let original = signed_packet(7);
    let signature = sign_packet(&alice, &original).expect("preimage must encode");

    let mut origin = original.clone();
    origin.signature = Some(signature);
    assert!(verify_packet(&origin, &alice.signing_public()));

    for relayed_ttl in [6u8, 3, 1, 0] {
        let relayed = BitchatPacket {
            ttl: relayed_ttl,
            signature: Some(signature),
            ..original.clone()
        };
        assert!(
            verify_packet(&relayed, &alice.signing_public()),
            "a relay that decremented the TTL to {relayed_ttl} must still verify"
        );
    }

    // `is_rsr` is a receiver-side annotation and is excluded for the same reason.
    let flagged = BitchatPacket {
        is_rsr: true,
        signature: Some(signature),
        ..original.clone()
    };
    assert!(verify_packet(&flagged, &alice.signing_public()));
}

#[test]
fn the_signature_is_ed25519_over_the_padded_preimage() {
    // Cross-check against `ed25519-dalek` directly, over the exact bytes
    // `signing_preimage()` returns. This pins *what* is signed, not merely that
    // signing and verification agree with each other — the failure mode being
    // guarded against is a self-consistent implementation that signs the
    // unpadded encoding and is rejected by every real peer.
    let alice = sender();
    let packet = signed_packet(7);
    let preimage = packet.signing_preimage().expect("preimage must encode");
    let signature = sign_packet(&alice, &packet).expect("preimage must encode");

    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&alice.signing_public())
        .expect("our own signing key is a valid Edwards point");
    verifying_key
        .verify_strict(&preimage, &ed25519_dalek::Signature::from_bytes(&signature))
        .expect("sign_packet must sign exactly signing_preimage()");

    // And the preimage really is the padded encoding: a ten-byte payload does
    // not produce a ten-byte-ish preimage, it produces a full padding block.
    assert_eq!(
        preimage.len(),
        256,
        "the preimage is the padded encoding, not the raw one"
    );
    // The TTL is neutralised inside it, so two TTLs share one preimage.
    assert_eq!(
        preimage,
        signed_packet(0)
            .signing_preimage()
            .expect("preimage must encode"),
        "packets differing only in TTL must share a preimage"
    );
}

#[test]
fn a_packet_signature_binds_everything_the_ttl_does_not() {
    let alice = sender();
    let original = signed_packet(7);
    let signature = sign_packet(&alice, &original).expect("preimage must encode");

    let tampered_payload = BitchatPacket {
        payload: b"hello mesh!".to_vec(),
        signature: Some(signature),
        ..original.clone()
    };
    assert!(!verify_packet(&tampered_payload, &alice.signing_public()));

    let tampered_sender = BitchatPacket {
        sender_id: [0xAC; SENDER_ID_LEN],
        signature: Some(signature),
        ..original.clone()
    };
    assert!(!verify_packet(&tampered_sender, &alice.signing_public()));

    let tampered_recipient = BitchatPacket {
        recipient_id: Some([0xCE; SENDER_ID_LEN]),
        signature: Some(signature),
        ..original.clone()
    };
    assert!(!verify_packet(&tampered_recipient, &alice.signing_public()));

    let tampered_timestamp = BitchatPacket {
        timestamp: TIMESTAMP + 1,
        signature: Some(signature),
        ..original.clone()
    };
    assert!(!verify_packet(&tampered_timestamp, &alice.signing_public()));

    // Another identity's signature, and no signature at all.
    let forged = BitchatPacket {
        signature: Some(sign_packet(&mallory(), &original).expect("preimage must encode")),
        ..original.clone()
    };
    assert!(!verify_packet(&forged, &alice.signing_public()));
    assert!(verify_packet(&forged, &mallory().signing_public()));

    assert!(original.signature.is_none());
    assert!(
        !verify_packet(&original, &alice.signing_public()),
        "an unsigned packet must never verify"
    );
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

#[test]
fn the_fingerprint_is_lowercase_hex_sha256_of_the_static_key() {
    // Golden value, derived by hashing a public key that an outside authority
    // pinned: `31e0303f…` is `init_remote_static` in the cacophony
    // `Noise_X_25519_ChaChaPoly_SHA256` vector, i.e. the X25519 public key for
    // the private key `4a3acbfd…`. Its SHA-256 was computed independently of
    // this crate.
    const PUBLIC_KEY: [u8; 32] = [
        0x31, 0xe0, 0x30, 0x3f, 0xd6, 0x41, 0x8d, 0x2f, 0x8c, 0x0e, 0x78, 0xb9, 0x1f, 0x22, 0xe8,
        0xca, 0xed, 0x0f, 0xbe, 0x48, 0x65, 0x6d, 0xcf, 0x47, 0x67, 0xe4, 0x83, 0x4f, 0x70, 0x1b,
        0x8f, 0x62,
    ];
    const EXPECTED: &str = "9c5643f1fd1a1cad0767bbc42575b2080ff9027c989156198b98ec78ab956038";

    assert_eq!(fingerprint_of(&PUBLIC_KEY), EXPECTED);
    assert_eq!(EXPECTED.len(), 64);

    // Same key material, reached through an identity: the two must agree, or a
    // peer's fingerprint would depend on which side computed it.
    let bob = recipient();
    assert_eq!(bob.fingerprint(), fingerprint_of(&bob.static_public()));
    assert_eq!(bob.fingerprint().len(), 64);
    assert!(
        bob.fingerprint()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "fingerprints are lowercase hex; an uppercase variant would compare unequal \
         against every peer that renders it the other way"
    );

    // Distinct keys give distinct fingerprints, and a one-bit change is not a
    // near-miss anyone could confuse.
    assert_ne!(bob.fingerprint(), sender().fingerprint());
    let mut flipped = PUBLIC_KEY;
    flipped[0] ^= 0x01;
    assert_ne!(fingerprint_of(&flipped), EXPECTED);
}

#[test]
fn a_seal_recovers_a_sender_key_that_fingerprints_to_the_sender() {
    // The join between the two halves of this file: seals return a raw static
    // key, the UI and the verification flow speak fingerprints, and the mapping
    // between them has to be the identity function or trust decisions are made
    // about the wrong peer.
    let alice = sender();
    let bob = recipient();
    let ciphertext = seal_courier(b"attributed", &bob.static_public(), &alice).expect("seals");
    let (_, recovered) = open_courier(&ciphertext, &bob).expect("opens");

    assert_eq!(fingerprint_of(&recovered), alice.fingerprint());
    assert_ne!(fingerprint_of(&recovered), bob.fingerprint());
    assert_ne!(fingerprint_of(&recovered), mallory().fingerprint());
}
