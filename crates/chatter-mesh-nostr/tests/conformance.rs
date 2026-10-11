//! Conformance suite: drives `vectors/*.json` against the Nostr transport.
//!
//! # Where these numbers come from
//!
//! Every value in `vectors/` is either **transcribed** from bitchat's own source or
//! test suite, or **derived** — computed by executing the documented algorithm with
//! primitives that were themselves validated against published test vectors first.
//! Each vector says which, in its own `source` and `derivation` fields. Nothing here
//! is a guess, and where a value could not be obtained either way there is no vector
//! and the gap is stated in a comment.
//!
//! The derivation toolchain (a pure-python ChaCha20/Poly1305/HChaCha20, secp256k1,
//! HKDF, bech32 and geohash) was validated before emitting anything, against:
//!
//! * RFC 8439 §2.8.2 — AEAD_CHACHA20_POLY1305
//! * draft-irtf-cfrg-xchacha-03 §2.2.1 and §A.3.1 — HChaCha20 and AEAD_XCHACHA20_POLY1305
//! * RFC 5869 §A.1 and §A.3 — HKDF-SHA256, the latter covering the empty-salt path
//! * BIP-340's official test-vector CSV — all 8 signing vectors
//! * NIP-19's `npub`/`nsec` examples — bech32
//! * the canonical geohash `u4pruydqqvj` for (57.64911, 10.40744)
//!
//! # Two things that cannot be asserted, and why
//!
//! **Signatures are not deterministic.** bitchat signs with 32 bytes of fresh random
//! BIP-340 aux, so the same event signs to different bytes every time. Anywhere a
//! signature appears in a vector it is asserted by *verification*, never by
//! comparison. The signatures in `events.json` were produced with aux = 32 zero
//! bytes precisely so that they are reproducible for an auditor, but no correct
//! implementation will reproduce them.
//!
//! **The length errors in `xchacha.json` are unrepresentable here.** bitchat checks
//! key and nonce lengths at runtime because Swift hands it a `Data`; our API takes
//! `[u8; 32]` and `[u8; 24]`, so those failures are compile errors instead. The
//! vectors are kept as a record of the checked lengths; they are not executed.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use chatter_mesh_nostr::types::{EventKind, GeohashLevel, NostrEvent};
use chatter_mesh_nostr::{builders, crypto, event, geohash, identity, pow, wrap};

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn load<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("vectors");
    path.push(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading vector file {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing vector file {}: {e}", path.display()))
}

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap_or_else(|e| panic!("bad hex in vector {s:?}: {e}"))
}

fn unhex_n<const N: usize>(s: &str) -> [u8; N] {
    let bytes = unhex(s);
    assert_eq!(bytes.len(), N, "vector field {s:?} should be {N} bytes");
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    out
}

/// Deterministic RNG for the paths that need one (throwaway wrap keys, nonces).
///
/// SplitMix64. Test-only: it is `CryptoRng` so it satisfies the bound, and it is
/// emphatically not one. Nothing it produces is compared to a vector — it only has
/// to be *some* value so a round trip can run.
struct TestRng(u64);

impl rand_core::TryRng for TestRng {
    type Error = core::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok({ self.next_u64() as u32 })
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok({
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        })
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dest.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}

impl rand_core::TryCryptoRng for TestRng {}

// ---------------------------------------------------------------------------
// XChaCha20-Poly1305
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct XChaChaFile {
    seal: Vec<SealVector>,
}

#[derive(Debug, Deserialize)]
struct SealVector {
    name: String,
    key_hex: String,
    nonce24_hex: String,
    aad_hex: String,
    plaintext_hex: String,
    ciphertext_hex: String,
    tag_hex: String,
}

/// The AEAD itself, through the `"v2:"` framing our `encrypt_with_nonce` produces.
///
/// bitchat always seals with an empty AAD, so this is the path every real DM takes.
#[test]
fn xchacha_seal_vectors_with_empty_aad() {
    let file: XChaChaFile = load("xchacha.json");
    let mut checked = 0;

    for vector in &file.seal {
        if !vector.aad_hex.is_empty() {
            continue;
        }

        let key = unhex_n::<32>(&vector.key_hex);
        let nonce = unhex_n::<24>(&vector.nonce24_hex);
        let plaintext = unhex(&vector.plaintext_hex);

        let payload = crypto::encrypt_with_nonce(&plaintext, &key, &nonce)
            .unwrap_or_else(|e| panic!("{}: encrypt_with_nonce failed: {e}", vector.name));

        let mut expected = nonce.to_vec();
        expected.extend_from_slice(&unhex(&vector.ciphertext_hex));
        expected.extend_from_slice(&unhex(&vector.tag_hex));

        let expected_payload = format!(
            "{}{}",
            chatter_mesh_nostr::CIPHERTEXT_PREFIX,
            base64_url_nopad(&expected)
        );
        assert_eq!(payload, expected_payload, "{}", vector.name);
        checked += 1;
    }

    assert!(
        checked >= 5,
        "expected several empty-AAD vectors, ran {checked}"
    );
}

/// The two AAD vectors, including the published draft one, go straight at the
/// primitive: our wire path never passes an AAD, so there is no API for it. If this
/// fails, the crate is not using XChaCha20-Poly1305 at all and everything above is
/// coincidence.
#[test]
fn xchacha_seal_vectors_with_aad() {
    use chacha20poly1305::aead::{Aead, Payload};
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    let file: XChaChaFile = load("xchacha.json");
    let mut checked = 0;

    for vector in &file.seal {
        if vector.aad_hex.is_empty() {
            continue;
        }

        let key = unhex_n::<32>(&vector.key_hex);
        let nonce = unhex_n::<24>(&vector.nonce24_hex);
        let cipher = XChaCha20Poly1305::new(&key.into());

        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &unhex(&vector.plaintext_hex),
                    aad: &unhex(&vector.aad_hex),
                },
            )
            .unwrap_or_else(|_| panic!("{}: seal failed", vector.name));

        let split = sealed.len() - 16;
        assert_eq!(
            hex::encode(&sealed[..split]),
            vector.ciphertext_hex,
            "{} ciphertext",
            vector.name
        );
        assert_eq!(
            hex::encode(&sealed[split..]),
            vector.tag_hex,
            "{} tag",
            vector.name
        );
        checked += 1;
    }

    assert!(checked >= 2, "expected the AAD vectors, ran {checked}");
}

/// Same key, same plaintext, different nonce: the ciphertexts must diverge.
#[test]
fn xchacha_nonce_changes_the_ciphertext() {
    let file: XChaChaFile = load("xchacha.json");
    let first = find_seal(&file, "bitchat_same_plaintext_nonce_01");
    let second = find_seal(&file, "bitchat_same_plaintext_nonce_02");

    assert_eq!(first.key_hex, second.key_hex);
    assert_eq!(first.plaintext_hex, second.plaintext_hex);
    assert_ne!(first.nonce24_hex, second.nonce24_hex);
    assert_ne!(first.ciphertext_hex, second.ciphertext_hex);
}

fn find_seal<'a>(file: &'a XChaChaFile, name: &str) -> &'a SealVector {
    file.seal
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no seal vector named {name}"))
}

fn base64_url_nopad(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// ECDH, HKDF, and the "v2:" framing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CryptoFile {
    ecdh: Vec<EcdhVector>,
    parity_retry: Vec<ParityVector>,
    hkdf: Vec<HkdfVector>,
    payload: Vec<PayloadVector>,
    malformed_payloads: Vec<MalformedPayload>,
}

#[derive(Debug, Deserialize)]
struct EcdhVector {
    name: String,
    alice_secret_hex: String,
    bob_secret_hex: String,
    alice_pubkey_xonly_hex: String,
    bob_pubkey_xonly_hex: String,
    shared_secret_hex: String,
    wrong_x_only_shared_secret_hex: String,
    wrong_sha256_shared_secret_hex: String,
}

#[derive(Debug, Deserialize)]
struct ParityVector {
    name: String,
    sender_secret_hex: String,
    sender_pubkey_xonly_hex: String,
    sender_pubkey_compressed_hex: String,
    recipient_secret_hex: String,
    shared_secret_odd_y_hex: String,
    shared_secret_even_y_guess_hex: String,
    conversation_key_odd_y_hex: String,
    conversation_key_even_y_guess_hex: String,
}

#[derive(Debug, Deserialize)]
struct HkdfVector {
    name: String,
    ikm_hex: String,
    conversation_key_hex: String,
    swapped_salt_and_info_key_hex: String,
    xonly_ikm_key_hex: String,
    sha256_ikm_key_hex: String,
}

#[derive(Debug, Deserialize)]
struct PayloadVector {
    name: String,
    conversation_key_hex: String,
    nonce24_hex: String,
    plaintext_utf8: String,
    payload: String,
    sender_secret_hex: String,
    sender_pubkey_xonly_hex: String,
    recipient_secret_hex: String,
    recipient_pubkey_xonly_hex: String,
}

#[derive(Debug, Deserialize)]
struct MalformedPayload {
    name: String,
    payload: String,
}

/// The shared secret is the **33-byte compressed point**, not the x-coordinate and
/// not a hash of either. This is the single fact the whole DM layer rests on: get it
/// wrong and every ciphertext is well-formed and undecryptable.
#[test]
fn ecdh_shared_secret_is_the_compressed_point() {
    let file: CryptoFile = load("crypto.json");

    for vector in &file.ecdh {
        let alice = unhex_n::<32>(&vector.alice_secret_hex);
        let bob = unhex_n::<32>(&vector.bob_secret_hex);

        assert_eq!(
            event::public_key_hex(&alice).unwrap(),
            vector.alice_pubkey_xonly_hex,
            "{}: alice pubkey",
            vector.name
        );
        assert_eq!(
            event::public_key_hex(&bob).unwrap(),
            vector.bob_pubkey_xonly_hex,
            "{}: bob pubkey",
            vector.name
        );

        let forward = crypto::shared_secret(&alice, &vector.bob_pubkey_xonly_hex).unwrap();
        let backward = crypto::shared_secret(&bob, &vector.alice_pubkey_xonly_hex).unwrap();

        assert_eq!(forward, backward, "{}: ECDH is not symmetric", vector.name);
        assert_eq!(forward.len(), crypto::SHARED_SECRET_LEN);
        assert_eq!(
            hex::encode(forward),
            vector.shared_secret_hex,
            "{}",
            vector.name
        );

        // The two plausible wrong answers.
        assert_ne!(hex::encode(forward), vector.wrong_x_only_shared_secret_hex);
        assert_ne!(hex::encode(forward), vector.wrong_sha256_shared_secret_hex);
    }
}

/// A sender whose public point has odd Y. The x-only pubkey on the wire cannot say
/// so, which is why decryption retries the `0x03` reconstruction.
#[test]
fn ecdh_parity_retry_is_required_for_odd_y_senders() {
    let file: CryptoFile = load("crypto.json");

    for vector in &file.parity_retry {
        let sender = unhex_n::<32>(&vector.sender_secret_hex);
        let recipient = unhex_n::<32>(&vector.recipient_secret_hex);

        assert!(
            vector.sender_pubkey_compressed_hex.starts_with("03"),
            "{}: this vector is only meaningful for an odd-Y sender",
            vector.name
        );

        // From the sender's side there is no ambiguity: it holds the real point.
        let from_sender =
            crypto::shared_secret(&sender, &event::public_key_hex(&recipient).unwrap()).unwrap();
        assert_eq!(
            hex::encode(from_sender),
            vector.shared_secret_odd_y_hex,
            "{}",
            vector.name
        );

        // From the recipient's side, reconstructing with the wrong parity gives a
        // different secret — that is the whole hazard.
        let even_guess =
            crypto::shared_secret(&recipient, &vector.sender_pubkey_xonly_hex).unwrap();
        assert_eq!(
            hex::encode(even_guess),
            vector.shared_secret_even_y_guess_hex,
            "{}: x-only reconstruction should assume even Y first",
            vector.name
        );
        // The two differ in exactly one byte — the parity prefix. The x coordinate
        // is identical because negating a point leaves x alone, which is precisely
        // why the mistake is invisible until decryption fails.
        assert_eq!(
            vector.shared_secret_odd_y_hex[2..],
            vector.shared_secret_even_y_guess_hex[2..],
            "{}: x coordinates should be identical",
            vector.name
        );
        assert_ne!(
            vector.shared_secret_odd_y_hex[..2],
            vector.shared_secret_even_y_guess_hex[..2],
            "{}: parity bytes should differ",
            vector.name
        );
        assert_ne!(
            vector.conversation_key_odd_y_hex, vector.conversation_key_even_y_guess_hex,
            "{}: one byte of ikm gives a completely different key",
            vector.name
        );
        assert_eq!(
            hex::encode(crypto::derive_conversation_key(&unhex_n::<33>(
                &vector.shared_secret_odd_y_hex
            ))),
            vector.conversation_key_odd_y_hex,
            "{}",
            vector.name
        );
    }
}

/// HKDF orientation: salt empty, info `"nip44-v2"`. The published NIP-44 v2 spec is
/// the other way round; the three trap values are what each plausible mistake
/// produces.
#[test]
fn hkdf_orientation_is_salt_empty_info_nip44v2() {
    let file: CryptoFile = load("crypto.json");

    for vector in &file.hkdf {
        let ikm = unhex_n::<33>(&vector.ikm_hex);
        let key = crypto::derive_conversation_key(&ikm);

        assert_eq!(
            hex::encode(key),
            vector.conversation_key_hex,
            "{}",
            vector.name
        );
        assert_ne!(
            hex::encode(key),
            vector.swapped_salt_and_info_key_hex,
            "{}: salt and info are swapped",
            vector.name
        );
        assert_ne!(
            hex::encode(key),
            vector.xonly_ikm_key_hex,
            "{}: the ikm is the compressed point, not the x-coordinate",
            vector.name
        );
        assert_ne!(
            hex::encode(key),
            vector.sha256_ikm_key_hex,
            "{}: the ikm is the raw point, not a hash of it",
            vector.name
        );
        assert_eq!(chatter_mesh_nostr::NIP44_HKDF_INFO, b"nip44-v2");
    }
}

/// `"v2:" + base64url(nonce24 ‖ ciphertext ‖ tag16)`, both directions.
#[test]
fn v2_payload_framing() {
    let file: CryptoFile = load("crypto.json");

    for vector in &file.payload {
        let key = unhex_n::<32>(&vector.conversation_key_hex);
        let nonce = unhex_n::<24>(&vector.nonce24_hex);

        // Encode.
        let payload =
            crypto::encrypt_with_nonce(vector.plaintext_utf8.as_bytes(), &key, &nonce).unwrap();
        assert_eq!(payload, vector.payload, "{}: encode", vector.name);
        assert!(payload.starts_with(chatter_mesh_nostr::CIPHERTEXT_PREFIX));
        assert!(
            !payload.contains('+') && !payload.contains('/') && !payload.contains('='),
            "{}: base64url must be URL-safe and unpadded",
            vector.name
        );

        // Decode, as the recipient, from the sender's x-only pubkey.
        let recipient = unhex_n::<32>(&vector.recipient_secret_hex);
        let opened = crypto::decrypt(&vector.payload, &vector.sender_pubkey_xonly_hex, &recipient)
            .unwrap_or_else(|e| panic!("{}: decrypt failed: {e}", vector.name));
        assert_eq!(opened, vector.plaintext_utf8, "{}: decode", vector.name);

        // And the conversation key really is the one derived from these keys.
        let sender = unhex_n::<32>(&vector.sender_secret_hex);
        let shared = crypto::shared_secret(&sender, &vector.recipient_pubkey_xonly_hex).unwrap();
        assert_eq!(
            hex::encode(crypto::derive_conversation_key(&shared)),
            vector.conversation_key_hex,
            "{}",
            vector.name
        );
    }
}

/// Round trip through the random-nonce path, which is what production calls.
#[test]
fn encrypt_then_decrypt_round_trips() {
    let file: CryptoFile = load("crypto.json");
    let vector = &file.payload[0];

    let sender = unhex_n::<32>(&vector.sender_secret_hex);
    let recipient = unhex_n::<32>(&vector.recipient_secret_hex);
    let mut rng = TestRng(0x5eed);

    let payload = crypto::encrypt(
        "round trip",
        &vector.recipient_pubkey_xonly_hex,
        &sender,
        &mut rng,
    )
    .unwrap();

    let opened = crypto::decrypt(&payload, &vector.sender_pubkey_xonly_hex, &recipient).unwrap();
    assert_eq!(opened, "round trip");

    // A different recipient must not be able to open it.
    let stranger = unhex_n::<32>(&file.ecdh[0].alice_secret_hex);
    assert!(crypto::decrypt(&payload, &vector.sender_pubkey_xonly_hex, &stranger).is_err());
}

#[test]
fn malformed_payloads_are_rejected() {
    let file: CryptoFile = load("crypto.json");
    let recipient = unhex_n::<32>(&file.payload[0].recipient_secret_hex);
    let sender_pubkey = &file.payload[0].sender_pubkey_xonly_hex;

    for vector in &file.malformed_payloads {
        let result = crypto::decrypt(&vector.payload, sender_pubkey, &recipient);
        assert!(
            result.is_err(),
            "{}: expected rejection, got {:?}",
            vector.name,
            result
        );
    }
}

// ---------------------------------------------------------------------------
// Event ids, JSON escaping, signatures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EventFile {
    vectors: Vec<EventVector>,
}

#[derive(Debug, Deserialize)]
struct EventVector {
    name: String,
    pubkey: String,
    created_at: i64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    serialized: String,
    id: String,
    #[serde(default)]
    sig: Option<String>,
}

impl EventVector {
    fn to_event(&self) -> NostrEvent {
        NostrEvent {
            id: self.id.clone(),
            pubkey: self.pubkey.clone(),
            created_at: self.created_at,
            kind: self.kind,
            tags: self.tags.clone(),
            content: self.content.clone(),
            sig: self.sig.clone(),
        }
    }
}

/// The vectors are self-checking: hashing the recorded preimage must give the
/// recorded id. If this fails the vector file is corrupt, not the implementation.
#[test]
fn event_vectors_are_internally_consistent() {
    let file: EventFile = load("events.json");

    for vector in &file.vectors {
        let digest = Sha256::digest(vector.serialized.as_bytes());
        assert_eq!(hex::encode(digest), vector.id, "{}", vector.name);
    }
}

/// The id preimage is `[0, pubkey, created_at, kind, tags, content]`, compact, with
/// `/` unescaped. This is the assertion that pins JSON escaping — a single
/// difference here changes every id and invalidates every signature ever produced.
#[test]
fn event_id_preimage_matches_foundation_escaping() {
    let file: EventFile = load("events.json");

    for vector in &file.vectors {
        let serialized = serde_json::to_string(&(
            0u8,
            &vector.pubkey,
            vector.created_at,
            vector.kind,
            &vector.tags,
            &vector.content,
        ))
        .unwrap();

        assert_eq!(
            serialized, vector.serialized,
            "{}: JSON serialization diverged",
            vector.name
        );
    }
}

#[test]
fn event_ids_match() {
    let file: EventFile = load("events.json");

    for vector in &file.vectors {
        let event = vector.to_event();
        assert_eq!(
            event::event_id_hex(&event).unwrap(),
            vector.id,
            "{}",
            vector.name
        );
    }
}

/// Slashes specifically: `.withoutEscapingSlashes` is set on bitchat's encoder, and
/// serde_json never escapes them. A `\/` anywhere in the preimage is a bug.
#[test]
fn forward_slashes_are_never_escaped() {
    let file: EventFile = load("events.json");
    let vector = file
        .vectors
        .iter()
        .find(|v| v.name == "escaping_forward_slash")
        .expect("escaping_forward_slash vector");

    assert!(vector.serialized.contains("https://example.com/a/b?c=d"));
    assert!(!vector.serialized.contains("\\/"));
    assert_eq!(event::event_id_hex(&vector.to_event()).unwrap(), vector.id);
}

/// Signatures verify; they are never compared byte for byte. See the module docs.
#[test]
fn recorded_signatures_verify() {
    let file: EventFile = load("events.json");
    let mut checked = 0;

    for vector in file.vectors.iter().filter(|v| v.sig.is_some()) {
        let event = vector.to_event();
        assert!(event::verify_event(&event), "{}", vector.name);
        checked += 1;
    }

    assert!(checked >= 2, "expected signed vectors, checked {checked}");
}

#[test]
fn tampering_breaks_verification() {
    let file: EventFile = load("events.json");
    let vector = file
        .vectors
        .iter()
        .find(|v| v.sig.is_some())
        .expect("a signed vector");

    let mut tampered = vector.to_event();
    tampered.content.push('!');
    assert!(
        !event::verify_event(&tampered),
        "content change must break the id, and the id must break the signature"
    );

    let mut wrong_id = vector.to_event();
    wrong_id.id = "deadbeef".to_string();
    assert!(!event::verify_event(&wrong_id));

    let mut no_sig = vector.to_event();
    no_sig.sig = None;
    assert!(!event::verify_event(&no_sig));

    let mut uppercase_id = vector.to_event();
    uppercase_id.id = uppercase_id.id.to_uppercase();
    assert!(
        !event::verify_event(&uppercase_id),
        "the id is a digest, not a case-insensitive name"
    );
}

/// Signing the same event twice gives different bytes and both verify — the reason
/// no vector asserts signature bytes.
#[test]
fn signing_is_not_deterministic_but_the_id_is() {
    let seed = [7u8; 32];
    let id = identity::derive_for_geohash(&seed, "u4pru").unwrap();

    let draft = NostrEvent::unsigned(
        id.public_key_hex().to_string(),
        1_700_000_000,
        EventKind::EphemeralEvent,
        vec![vec!["g".into(), "u4pru".into()]],
        "hello mesh".into(),
    );

    let first = event::sign_event(draft.clone(), id.secret()).unwrap();
    let second = event::sign_event(draft, id.secret()).unwrap();

    assert_eq!(first.id, second.id, "ids are deterministic");
    assert_ne!(first.sig, second.sig, "signatures are not");
    assert!(event::verify_event(&first));
    assert!(event::verify_event(&second));
}

#[test]
fn event_json_round_trips() {
    let file: EventFile = load("events.json");

    for vector in &file.vectors {
        let event = vector.to_event();
        let json = event::to_json(&event).unwrap();
        let parsed = event::from_json(&json).unwrap();
        assert_eq!(parsed, event, "{}", vector.name);
    }
}

// ---------------------------------------------------------------------------
// Geohash
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GeohashFile {
    alphabet: String,
    prefix_chain: PrefixChain,
    invalid_geohashes: Vec<InvalidGeohash>,
    valid_geohashes: Vec<String>,
    presence_precision: PresencePrecision,
    vectors: Vec<GeohashVector>,
}

#[derive(Debug, Deserialize)]
struct PrefixChain {
    lat: f64,
    lon: f64,
    levels: Vec<PrefixLevel>,
}

#[derive(Debug, Deserialize)]
struct PrefixLevel {
    precision: usize,
    geohash: String,
}

#[derive(Debug, Deserialize)]
struct InvalidGeohash {
    value: String,
    why: String,
}

#[derive(Debug, Deserialize)]
struct PresencePrecision {
    max_precision: usize,
    allowed: Vec<String>,
    denied: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GeohashVector {
    name: String,
    lat: f64,
    lon: f64,
    precision: usize,
    geohash: String,
    center_lat: f64,
    center_lon: f64,
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
}

#[test]
fn geohash_encode_vectors() {
    let file: GeohashFile = load("geohash.json");

    for vector in &file.vectors {
        let encoded = geohash::encode(vector.lat, vector.lon, vector.precision)
            .unwrap_or_else(|e| panic!("{}: encode failed: {e}", vector.name));
        assert_eq!(encoded, vector.geohash, "{}", vector.name);
        assert_eq!(encoded.chars().count(), vector.precision, "{}", vector.name);
    }
}

#[test]
fn geohash_decode_vectors() {
    let file: GeohashFile = load("geohash.json");

    for vector in &file.vectors {
        let (lat, lon) = geohash::decode(&vector.geohash).unwrap();
        assert!(
            (lat - vector.center_lat).abs() < 1e-9 && (lon - vector.center_lon).abs() < 1e-9,
            "{}: centre was ({lat}, {lon}), expected ({}, {})",
            vector.name,
            vector.center_lat,
            vector.center_lon
        );

        let (lat_min, lat_max, lon_min, lon_max) = geohash::bounds(&vector.geohash).unwrap();
        assert!(
            (lat_min - vector.lat_min).abs() < 1e-9
                && (lat_max - vector.lat_max).abs() < 1e-9
                && (lon_min - vector.lon_min).abs() < 1e-9
                && (lon_max - vector.lon_max).abs() < 1e-9,
            "{}: bounds mismatch",
            vector.name
        );

        // The original coordinate must fall inside the cell it encoded to.
        let lat = vector.lat.clamp(-90.0, 90.0);
        let lon = vector.lon.clamp(-180.0, 180.0);
        assert!(
            lat >= lat_min && lat <= lat_max && lon >= lon_min && lon <= lon_max,
            "{}: the source coordinate is outside its own cell",
            vector.name
        );
    }
}

#[test]
fn geohash_alphabet_excludes_confusable_letters() {
    let file: GeohashFile = load("geohash.json");

    assert_eq!(file.alphabet, "0123456789bcdefghjkmnpqrstuvwxyz");
    assert_eq!(file.alphabet.chars().count(), 32);
    for c in ['a', 'i', 'l', 'o'] {
        assert!(
            !file.alphabet.contains(c),
            "{c} must not be in the alphabet"
        );
    }

    // And no encoding can ever emit one.
    for vector in &file.vectors {
        assert!(
            vector.geohash.chars().all(|c| file.alphabet.contains(c)),
            "{}: emitted a character outside the alphabet",
            vector.name
        );
    }
}

#[test]
fn geohash_validation_vectors() {
    let file: GeohashFile = load("geohash.json");

    for invalid in &file.invalid_geohashes {
        assert!(
            !geohash::is_valid(&invalid.value),
            "{:?} should be invalid: {}",
            invalid.value,
            invalid.why
        );
        assert!(geohash::bounds(&invalid.value).is_err());
    }

    for valid in &file.valid_geohashes {
        assert!(geohash::is_valid(valid), "{valid:?} should be valid");
    }
}

#[test]
fn geohash_precision_levels_form_a_prefix_chain() {
    let file: GeohashFile = load("geohash.json");
    let chain = &file.prefix_chain;

    for level in &chain.levels {
        assert_eq!(
            geohash::encode(chain.lat, chain.lon, level.precision).unwrap(),
            level.geohash,
            "precision {}",
            level.precision
        );
    }

    for pair in chain.levels.windows(2) {
        let (finer, coarser) = (&pair[0], &pair[1]);
        assert!(
            finer.geohash.starts_with(&coarser.geohash),
            "{} should start with {}",
            finer.geohash,
            coarser.geohash
        );
        assert_eq!(
            geohash::parent(&finer.geohash, coarser.precision).unwrap(),
            coarser.geohash
        );
    }
}

/// The privacy rule, at the level of the type: presence is permitted through
/// precision 5 and refused at 6 and finer.
#[test]
fn presence_precision_rule() {
    let file: GeohashFile = load("geohash.json");
    assert_eq!(
        GeohashLevel::MAX_PRESENCE_PRECISION,
        file.presence_precision.max_precision
    );

    for level in [
        GeohashLevel::Region,
        GeohashLevel::Province,
        GeohashLevel::City,
    ] {
        assert!(level.allows_presence(), "{level:?}");
    }
    for level in [
        GeohashLevel::Neighborhood,
        GeohashLevel::Block,
        GeohashLevel::Building,
    ] {
        assert!(!level.allows_presence(), "{level:?}");
    }
}

// ---------------------------------------------------------------------------
// Derived identities
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IdentityFile {
    device_seed_hex: String,
    bridge_label_prefix: String,
    identities: Vec<DerivedIdentity>,
    nip19: Vec<Nip19Vector>,
    invalid_npub: Vec<InvalidNpub>,
}

#[derive(Debug, Deserialize)]
struct DerivedIdentity {
    label: String,
    secret_hex: String,
    pubkey_hex: String,
    npub: String,
    nsec: String,
}

#[derive(Debug, Deserialize)]
struct Nip19Vector {
    name: String,
    #[serde(default)]
    pubkey_hex: Option<String>,
    #[serde(default)]
    npub: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InvalidNpub {
    value: String,
    why: String,
}

#[test]
fn derived_identity_vectors() {
    let file: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&file.device_seed_hex);

    for vector in &file.identities {
        let derived = identity::derive_for_geohash(&seed, &vector.label)
            .unwrap_or_else(|e| panic!("{:?}: derive failed: {e}", vector.label));

        assert_eq!(
            hex::encode(derived.secret()),
            vector.secret_hex,
            "{:?}: secret",
            vector.label
        );
        assert_eq!(
            derived.public_key_hex(),
            vector.pubkey_hex,
            "{:?}: pubkey",
            vector.label
        );
        assert_eq!(
            derived.npub().unwrap(),
            vector.npub,
            "{:?}: npub",
            vector.label
        );
        assert_eq!(
            derived.nsec().unwrap(),
            vector.nsec,
            "{:?}: nsec",
            vector.label
        );
    }
}

#[test]
fn derivation_is_deterministic_and_cell_separating() {
    let file: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&file.device_seed_hex);

    let once = identity::derive_for_geohash(&seed, "u4pru").unwrap();
    let twice = identity::derive_for_geohash(&seed, "u4pru").unwrap();
    assert_eq!(once.public_key_hex(), twice.public_key_hex());

    let neighbour = identity::derive_for_geohash(&seed, "u4prv").unwrap();
    assert_ne!(once.public_key_hex(), neighbour.public_key_hex());

    let coarser = identity::derive_for_geohash(&seed, "u4").unwrap();
    assert_ne!(once.public_key_hex(), coarser.public_key_hex());

    // A different seed must give a different identity for the same cell.
    let other_seed = [0xaa; 32];
    let other = identity::derive_for_geohash(&other_seed, "u4pru").unwrap();
    assert_ne!(once.public_key_hex(), other.public_key_hex());
}

/// The bridge identity for a cell must not be linkable to the chat identity for the
/// same cell — that is what the `"bridge|"` label buys.
#[test]
fn bridge_identity_differs_from_chat_identity() {
    let file: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&file.device_seed_hex);

    assert_eq!(
        file.bridge_label_prefix,
        chatter_mesh_nostr::BRIDGE_IDENTITY_LABEL
    );

    let chat = identity::derive_for_geohash(&seed, "u4pru").unwrap();
    let bridge = identity::derive_for_bridge(&seed, "u4pru").unwrap();
    assert_ne!(chat.public_key_hex(), bridge.public_key_hex());

    // And it is exactly the labelled derivation, not some other scheme.
    let expected = file
        .identities
        .iter()
        .find(|i| i.label == "bridge|u4pru")
        .expect("bridge|u4pru vector");
    assert_eq!(bridge.public_key_hex(), expected.pubkey_hex);
}

#[test]
fn nip19_bech32_vectors() {
    let file: IdentityFile = load("identity.json");

    for vector in &file.nip19 {
        let (Some(pubkey), Some(npub)) = (&vector.pubkey_hex, &vector.npub) else {
            continue; // the nsec reference is covered by the derived identities
        };
        assert_eq!(
            identity::npub_encode(pubkey).unwrap(),
            *npub,
            "{}",
            vector.name
        );
        assert_eq!(
            identity::npub_decode(npub).unwrap(),
            *pubkey,
            "{}",
            vector.name
        );
    }

    for invalid in &file.invalid_npub {
        assert!(
            identity::npub_decode(&invalid.value).is_err(),
            "{:?} should not decode: {}",
            invalid.value,
            invalid.why
        );
    }
}

// ---------------------------------------------------------------------------
// Builders — tag order is inside the event id
// ---------------------------------------------------------------------------

/// Every geohash-chat tag ordering, driven from the id vectors. If a builder
/// reorders, omits or adds a tag, the id stops matching.
#[test]
fn geohash_message_tag_order() {
    let events: EventFile = load("events.json");
    let ids: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&ids.device_seed_hex);
    let sender = identity::derive_for_geohash(&seed, "u4pru").unwrap();

    let cases = [
        ("geohash_chat_minimal", None, false),
        ("geohash_chat_with_nickname", Some("alice"), false),
        ("geohash_chat_teleported", Some("alice"), true),
        ("geohash_chat_teleported_no_nickname", None, true),
    ];

    for (name, nickname, teleported) in cases {
        let vector = events
            .vectors
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("{name} vector"));

        let built = builders::geohash_message(
            "u4pru",
            &vector.content,
            nickname,
            teleported,
            &sender,
            vector.created_at,
        )
        .unwrap();

        // Every geohash message is mined, so the built event carries a trailing
        // `nonce` tag and its ID commits to that nonce. Our nonce is not
        // bitchat's — they start the search at a random value — so the built
        // ID is not a canonical thing to pin. The two properties the vector
        // exists to prove are pinned separately instead:
        //
        //   1. the base tags, in order, are exactly what the vector says;
        //   2. the NIP-01 ID serialization of those fields still reproduces the
        //      vector's ID, recomputed here from an unmined copy.
        let mined = built.tags.last().expect("a nonce tag");
        assert_eq!(mined[0], "nonce", "{name}: the nonce tag is appended last");
        let base = &built.tags[..built.tags.len() - 1];
        assert_eq!(base, vector.tags, "{name}: tag order");

        let unmined = NostrEvent::unsigned(
            built.pubkey.clone(),
            built.created_at,
            EventKind::EphemeralEvent,
            base.to_vec(),
            built.content.clone(),
        );
        assert_eq!(
            event::event_id_hex(&unmined).expect("id"),
            vector.id,
            "{name}: event id of the unmined fields"
        );

        assert_eq!(built.kind, vector.kind);
        assert!(event::verify_event(&built), "{name}");
        assert_eq!(
            pow::difficulty_of(&built).expect("score"),
            pow::TARGET_BITS,
            "{name}: the shipped event must meet the difficulty it commits to"
        );
    }
}

/// A blank nickname is dropped rather than published as an empty `n` tag.
#[test]
fn blank_nickname_is_omitted() {
    let ids: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&ids.device_seed_hex);
    let sender = identity::derive_for_geohash(&seed, "u4pru").unwrap();

    for blank in [Some(""), Some("   "), None] {
        let event =
            builders::geohash_message("u4pru", "hi", blank, false, &sender, 1_700_000_000).unwrap();
        assert!(
            !event
                .tags
                .iter()
                .any(|t| t.first().map(String::as_str) == Some("n")),
            "{blank:?} should not produce an n tag"
        );
    }
}

/// Presence: kind 20001, `g` tag only, empty content, and refused outright above
/// precision 5. The refusal is the privacy guarantee; it lives in the builder so no
/// call site can skip it.
#[test]
fn presence_builder_enforces_the_precision_cap() {
    let geo: GeohashFile = load("geohash.json");
    let ids: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&ids.device_seed_hex);

    for allowed in &geo.presence_precision.allowed {
        let sender = identity::derive_for_geohash(&seed, allowed).unwrap();
        let event = builders::geohash_presence(allowed, &sender, 1_700_000_000)
            .unwrap_or_else(|e| panic!("{allowed} should be allowed: {e}"));

        assert_eq!(event.kind, EventKind::GeohashPresence.as_u16());
        assert_eq!(event.content, "");
        assert_eq!(event.tags, vec![vec!["g".to_string(), allowed.clone()]]);
        assert!(event::verify_event(&event));
    }

    for denied in &geo.presence_precision.denied {
        let sender = identity::derive_for_geohash(&seed, denied).unwrap();
        let result = builders::geohash_presence(denied, &sender, 1_700_000_000);
        assert!(
            matches!(
                result,
                Err(chatter_mesh_nostr::NostrError::PresencePrecisionTooFine(_))
            ),
            "{denied} must be refused, got {result:?}"
        );
    }
}

/// The presence event for a permitted cell reproduces the recorded id exactly.
#[test]
fn presence_event_matches_its_vector() {
    let events: EventFile = load("events.json");
    let ids: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&ids.device_seed_hex);
    let sender = identity::derive_for_geohash(&seed, "u4pru").unwrap();

    let vector = events
        .vectors
        .iter()
        .find(|v| v.name == "geohash_presence")
        .expect("geohash_presence vector");

    let event = builders::geohash_presence("u4pru", &sender, vector.created_at).unwrap();
    assert_eq!(event.id, vector.id);
    assert_eq!(event.tags, vector.tags);
}

// ---------------------------------------------------------------------------
// Gift wrap
// ---------------------------------------------------------------------------

/// Rumor 14 → seal 13 → wrap 1059, and back. The wrap must carry a throwaway
/// pubkey, tag the recipient, and authenticate the sender on the way out.
#[test]
fn gift_wrap_round_trip() {
    let file: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&file.device_seed_hex);
    let sender = identity::derive_for_geohash(&seed, "u4pru").unwrap();
    let recipient = identity::derive_for_geohash(&seed, "u4prv").unwrap();
    let mut rng = TestRng(0xc0ffee);

    let wrap_event = wrap::gift_wrap(
        "hello over nostr",
        recipient.public_key_hex(),
        &sender,
        1_700_000_000,
        &mut rng,
    )
    .unwrap();

    assert_eq!(wrap_event.kind, EventKind::GiftWrap.as_u16());
    assert!(event::verify_event(&wrap_event), "the wrap signs itself");
    assert_eq!(
        wrap_event.tag_value("p"),
        Some(recipient.public_key_hex()),
        "the recipient is tagged so relays can route it"
    );
    assert_ne!(
        wrap_event.pubkey,
        sender.public_key_hex(),
        "the wrap must be published under a throwaway key, never the sender's"
    );
    assert!(
        wrap_event
            .content
            .starts_with(chatter_mesh_nostr::CIPHERTEXT_PREFIX),
        "wrap content is a v2: payload"
    );

    let (content, authenticated_sender, rumor_created_at) =
        wrap::unwrap_gift(&wrap_event, recipient.secret()).unwrap();
    // The rumor carries the true send time; the wrap's is jittered by ±15 min.
    assert_eq!(rumor_created_at, 1_700_000_000);
    assert_eq!(content, "hello over nostr");
    assert_eq!(
        authenticated_sender,
        sender.public_key_hex(),
        "the sender is the seal's signer, not the wrap's"
    );

    // Nobody else can open it.
    let stranger = identity::derive_for_geohash(&seed, "u4").unwrap();
    assert!(wrap::unwrap_gift(&wrap_event, stranger.secret()).is_err());
}

/// Two wraps of the same message must not share a pubkey — a fresh throwaway key
/// per wrap is what stops a relay linking a conversation.
#[test]
fn each_gift_wrap_uses_a_fresh_key() {
    let file: IdentityFile = load("identity.json");
    let seed = unhex_n::<32>(&file.device_seed_hex);
    let sender = identity::derive_for_geohash(&seed, "u4pru").unwrap();
    let recipient = identity::derive_for_geohash(&seed, "u4prv").unwrap();
    let mut rng = TestRng(0xfeed);

    let first = wrap::gift_wrap("same", recipient.public_key_hex(), &sender, 1, &mut rng).unwrap();
    let second = wrap::gift_wrap("same", recipient.public_key_hex(), &sender, 1, &mut rng).unwrap();

    assert_ne!(first.pubkey, second.pubkey);
    assert_ne!(first.content, second.content);
    assert_ne!(first.id, second.id);
}

// ---------------------------------------------------------------------------
// Relay frames
// ---------------------------------------------------------------------------

#[cfg(feature = "client")]
mod relay_frames {
    use super::*;
    use chatter_mesh_nostr::relay::{self, RelayMessage};

    #[derive(Debug, Deserialize)]
    struct RelayFile {
        well_formed: Vec<WellFormedFrame>,
        malformed: Vec<MalformedFrame>,
        relay_urls: RelayUrls,
    }

    #[derive(Debug, Deserialize)]
    struct WellFormedFrame {
        name: String,
        kind: String,
        frame: String,
        #[serde(default)]
        subscription_id: Option<String>,
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default)]
        accepted: Option<bool>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        tolerant: bool,
    }

    #[derive(Debug, Deserialize)]
    struct MalformedFrame {
        name: String,
        frame: String,
        why: String,
    }

    #[derive(Debug, Deserialize)]
    struct RelayUrls {
        normalize: Vec<NormalizeCase>,
        directory_address: Vec<DirectoryCase>,
    }

    #[derive(Debug, Deserialize)]
    struct NormalizeCase {
        raw: String,
        default_scheme: Option<String>,
        expected: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct DirectoryCase {
        raw: String,
        expected: Option<String>,
    }

    #[test]
    fn well_formed_frames_parse() {
        let file: RelayFile = load("relay.json");

        for vector in &file.well_formed {
            let parsed = relay::parse_relay_message(&vector.frame);

            if vector.tolerant && parsed.is_err() {
                // bitchat itself drops this frame type; either behaviour is fine so
                // long as it is not a panic or a misclassification.
                continue;
            }

            let parsed =
                parsed.unwrap_or_else(|e| panic!("{}: expected a parse, got {e}", vector.name));

            match (vector.kind.as_str(), &parsed) {
                (
                    "event",
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
                ) => {
                    assert_eq!(Some(subscription_id), vector.subscription_id.as_ref());
                    assert_eq!(Some(&event.id), vector.event_id.as_ref());
                    assert!(
                        event::verify_event(event),
                        "{}: the embedded event must verify",
                        vector.name
                    );
                }
                ("eose", RelayMessage::Eose { subscription_id }) => {
                    assert_eq!(Some(subscription_id), vector.subscription_id.as_ref());
                }
                (
                    "ok",
                    RelayMessage::Ok {
                        event_id,
                        accepted,
                        message,
                    },
                ) => {
                    assert_eq!(Some(event_id), vector.event_id.as_ref());
                    assert_eq!(Some(*accepted), vector.accepted);
                    match &vector.message {
                        Some(expected) => assert_eq!(message, expected, "{}", vector.name),
                        // A missing reason is filled in, never left empty.
                        None => assert!(!message.is_empty(), "{}", vector.name),
                    }
                }
                ("notice", RelayMessage::Notice(message)) => {
                    assert_eq!(Some(message), vector.message.as_ref());
                }
                (
                    "closed",
                    RelayMessage::Closed {
                        subscription_id, ..
                    },
                ) => {
                    assert_eq!(Some(subscription_id), vector.subscription_id.as_ref());
                }
                (expected, actual) => {
                    panic!(
                        "{}: expected a {expected} frame, got {actual:?}",
                        vector.name
                    )
                }
            }
        }
    }

    #[test]
    fn malformed_frames_are_rejected_without_panicking() {
        let file: RelayFile = load("relay.json");

        for vector in &file.malformed {
            let parsed = relay::parse_relay_message(&vector.frame);
            assert!(
                parsed.is_err(),
                "{} should be rejected ({}), got {parsed:?}",
                vector.name,
                vector.why
            );
        }
    }

    /// Relay URL normalisation. The `:443` cases decide whether a CSV row written
    /// `host:443` is the same relay as one written `host` — if it is not, the client
    /// opens two connections to the same relay and dedups nothing.
    #[test]
    fn relay_url_normalisation() {
        let file: RelayFile = load("relay.json");

        for case in &file.relay_urls.normalize {
            let actual = relay::normalize_relay_url(&case.raw, case.default_scheme.as_deref());
            assert_eq!(
                actual.as_deref(),
                case.expected.as_deref(),
                "normalize({:?}, {:?})",
                case.raw,
                case.default_scheme
            );
        }

        for case in &file.relay_urls.directory_address {
            let actual = relay::directory_address(&case.raw);
            assert_eq!(
                actual.as_deref(),
                case.expected.as_deref(),
                "directory_address({:?})",
                case.raw
            );
        }
    }
}

#[cfg(test)]
use rand_core::Rng;
