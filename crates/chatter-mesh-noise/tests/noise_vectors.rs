//! Drives the canonical Noise specification vectors through this crate.
//!
//! # Provenance — why these bytes can be trusted
//!
//! `vectors/noise_official.json` is not hand-derived. It is a verbatim copy of
//! the community-canonical Noise vector corpus:
//!
//! - Entries 1 and 2 (`Noise_XX_25519_ChaChaPoly_SHA256`) are copied byte for
//!   byte from bitchat's `bitchatTests/Noise/NoiseTestVectors.json`, which
//!   bitchat's own `NoiseProtocolTests.runTestVector` asserts against. Entry 1
//!   is the cacophony fixture (the `"John Galt"` prologue, the
//!   `"Ludwig von Mises"` / `"Murray Rothbard"` payloads); entry 2 is snow's.
//! - Entry 3 (`Noise_X_25519_ChaChaPoly_SHA256`) is the cacophony fixture for
//!   the one-shot `X` pattern that [`chatter_mesh_noise::seal`] is built on.
//!   bitchat's JSON carries no `X` entry, so it was taken from the cacophony
//!   corpus directly — the same upstream file bitchat lifted its `XX` entry out
//!   of.
//!
//! That last claim was checked mechanically when the file was assembled:
//! bitchat's entry 1 and cacophony's `Noise_XX_25519_ChaChaPoly_SHA256` entry
//! compare equal field for field. The two files really are the same corpus, so
//! the `X` entry sits in the same trust domain as the `XX` ones. As a standing
//! check, [`the_corpus_is_the_cacophony_fixture_set`] pins the fixtures'
//! recognisable markers.
//!
//! **Nothing in `vectors/noise_official.json` was computed by this project.**
//! If a value here is wrong, it is wrong upstream, and every Noise
//! implementation in the world is wrong with it.
//!
//! # Why the vectors need a test seam
//!
//! A Noise transcript is only reproducible if the ephemerals are pinned, and
//! these entries also carry their own protocol name and prologue. The
//! production constructors quite rightly expose none of that, so the suite is
//! driven through [`NoiseHandshake::for_vectors`], which `session.rs` gates
//! behind `feature = "vectors"`. **That feature must be enabled for this file
//! to compile:** an integration test is a separate crate, so the `cfg(test)`
//! half of the gate does not apply to it.
//!
//! # Message ordering
//!
//! `XX` is three handshake messages — initiator, responder, initiator — and the
//! corpus then continues the same strict alternation into transport mode, so
//! `messages[3]` is sent by the **responder**. `X` is a one-way pattern: the
//! responder never writes, so every message after the single handshake message
//! is sent by the initiator. Both readings match the upstream harnesses exactly
//! — bitchat's `transportIndex % 2 == 0` for `XX`, and snow's
//! `if is_oneway || i % 2 == 0` over the global message index for both.

use chatter_mesh_noise::identity::MeshIdentity;
use chatter_mesh_noise::session::{HandshakeRole, NONCE_PREFIX_LEN, NoiseHandshake, NoiseSession};
use chatter_mesh_noise::{NOISE_X_PROTOCOL, NOISE_XX_PROTOCOL};
use serde::Deserialize;

const VECTORS_JSON: &str = include_str!("../vectors/noise_official.json");

/// `XX` completes after three messages; everything after is transport.
const XX_HANDSHAKE_MESSAGES: usize = 3;
/// `X` completes after one. It is one-way, so the responder writes nothing.
const X_HANDSHAKE_MESSAGES: usize = 1;

/// The vectors pin only Curve25519 material. The Ed25519 signing key plays no
/// part in a Noise transcript, so any fixed value does — it exists only because
/// [`MeshIdentity`] holds both keys, and it is used here purely to derive
/// public keys from the corpus's private ones.
const SIGNING_FILLER: [u8; 32] = [0x5A; 32];

// ---------------------------------------------------------------------------
// Vector file
// ---------------------------------------------------------------------------

/// One corpus entry.
///
/// Fields absent from a given pattern are `Option`: `X` has no responder
/// ephemeral, `XX` has no `init_remote_static` (nothing is known in advance),
/// and snow's `XX` entry carries no `handshake_hash`. Unknown fields
/// (`init_psks`, `resp_psks`) are ignored — plain `XX` and `X` have no PSKs.
#[derive(Deserialize)]
struct Vector {
    protocol_name: String,
    init_prologue: String,
    init_static: String,
    init_ephemeral: String,
    resp_prologue: String,
    resp_static: String,
    resp_ephemeral: Option<String>,
    /// `X` only: the responder's static *public* key, known to the initiator
    /// before the handshake starts.
    init_remote_static: Option<String>,
    handshake_hash: Option<String>,
    messages: Vec<VectorMessage>,
}

#[derive(Deserialize)]
struct VectorMessage {
    payload: String,
    ciphertext: String,
}

fn corpus() -> Vec<Vector> {
    serde_json::from_str(VECTORS_JSON).expect("vectors/noise_official.json must parse")
}

fn bytes(hex_str: &str) -> Vec<u8> {
    hex::decode(hex_str).expect("vector field must be valid hex")
}

fn key32(hex_str: &str) -> [u8; 32] {
    let decoded = bytes(hex_str);
    assert_eq!(decoded.len(), 32, "key fields in the corpus are 32 bytes");
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    out
}

/// The Curve25519 public key for a corpus private key, derived through the
/// crate's own identity type — the same derivation every other module uses.
fn public_of(secret: &[u8; 32]) -> [u8; 32] {
    MeshIdentity::from_keys(*secret, SIGNING_FILLER).static_public()
}

// ---------------------------------------------------------------------------
// XX — the interactive session pattern
// ---------------------------------------------------------------------------

#[test]
fn official_xx_vectors_reproduce_every_ciphertext_byte() {
    let corpus = corpus();
    let mut driven = 0;

    for (index, vector) in corpus.iter().enumerate() {
        if vector.protocol_name != NOISE_XX_PROTOCOL {
            continue;
        }
        run_xx_vector(vector, index);
        driven += 1;
    }

    assert_eq!(
        driven, 2,
        "the corpus carries two XX vectors (cacophony's and snow's); \
         if this count changed, the vector file was edited"
    );
}

fn run_xx_vector(vector: &Vector, index: usize) {
    let label = format!("XX vector {}", index + 1);

    // Both parties mix the same prologue. An entry where they differ would be
    // testing something else entirely.
    assert_eq!(
        vector.init_prologue, vector.resp_prologue,
        "{label}: prologues must agree"
    );
    let prologue = bytes(&vector.init_prologue);

    let init_static = key32(&vector.init_static);
    let resp_static = key32(&vector.resp_static);
    let resp_ephemeral = key32(
        vector
            .resp_ephemeral
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: XX needs a responder ephemeral")),
    );

    let mut initiator = NoiseHandshake::for_vectors(
        NOISE_XX_PROTOCOL,
        HandshakeRole::Initiator,
        Some(&init_static),
        None,
        Some(&key32(&vector.init_ephemeral)),
        &prologue,
    )
    .unwrap_or_else(|_| panic!("{label}: initiator must build"));

    let mut responder = NoiseHandshake::for_vectors(
        NOISE_XX_PROTOCOL,
        HandshakeRole::Responder,
        Some(&resp_static),
        None,
        Some(&resp_ephemeral),
        &prologue,
    )
    .unwrap_or_else(|_| panic!("{label}: responder must build"));

    assert!(
        vector.messages.len() > XX_HANDSHAKE_MESSAGES,
        "{label}: entry must carry transport messages too"
    );

    // -- Message 1: initiator -> responder, `e`. The payload travels in the
    //    clear (no key exists yet), so the ciphertext is `e ‖ payload`.
    exchange(
        &mut initiator,
        &mut responder,
        &vector.messages[0],
        &label,
        1,
    );
    assert!(
        !initiator.is_complete() && !responder.is_complete(),
        "{label}: XX is not done after one message"
    );

    // -- Message 2: responder -> initiator, `e, ee, s, es`.
    exchange(
        &mut responder,
        &mut initiator,
        &vector.messages[1],
        &label,
        2,
    );

    // -- Message 3: initiator -> responder, `s, se`.
    exchange(
        &mut initiator,
        &mut responder,
        &vector.messages[2],
        &label,
        3,
    );

    assert!(
        initiator.is_complete(),
        "{label}: initiator must be complete"
    );
    assert!(
        responder.is_complete(),
        "{label}: responder must be complete"
    );

    // -- Each side learned the other's static key from the transcript. That is
    //    the mutual authentication XX buys, and everything above this crate
    //    (fingerprints, verification, trust decisions) rests on it.
    assert_eq!(
        initiator.remote_static(),
        Some(public_of(&resp_static)),
        "{label}: initiator must have learned the responder's static key"
    );
    assert_eq!(
        responder.remote_static(),
        Some(public_of(&init_static)),
        "{label}: responder must have learned the initiator's static key"
    );

    check_handshake_hash(&initiator, &responder, vector, &label);

    // -- Transport. The corpus continues the alternation over the *global*
    //    message index, so message 4 (index 3) is the responder's: the
    //    initiator sent the last handshake message.
    let mut initiator_session = initiator
        .into_session()
        .unwrap_or_else(|_| panic!("{label}: initiator must enter transport mode"));
    let mut responder_session = responder
        .into_session()
        .unwrap_or_else(|_| panic!("{label}: responder must enter transport mode"));

    assert_eq!(
        initiator_session.remote_static(),
        public_of(&resp_static),
        "{label}: the session must carry the peer's static key forward"
    );
    assert_eq!(
        responder_session.remote_static(),
        public_of(&init_static),
        "{label}: the session must carry the peer's static key forward"
    );

    for (offset, message) in vector.messages[XX_HANDSHAKE_MESSAGES..].iter().enumerate() {
        let number = XX_HANDSHAKE_MESSAGES + offset + 1;
        let (sender, receiver) = if offset % 2 == 0 {
            (&mut responder_session, &mut initiator_session)
        } else {
            (&mut initiator_session, &mut responder_session)
        };
        transport_step(sender, receiver, message, &label, number);
    }
}

// ---------------------------------------------------------------------------
// X — the one-shot seal pattern
// ---------------------------------------------------------------------------

#[test]
fn the_official_x_vector_reproduces_every_ciphertext_byte() {
    // `X` is the pattern both `seal_courier` and `seal_prekey` are built on.
    // The seal API deliberately hard-codes bitchat's prologues, so it cannot be
    // pointed at a corpus entry; this drives the same pattern through
    // `for_vectors` instead. It proves the `X` machinery underneath the seals is
    // spec-conformant, and `tests/conformance.rs` separately proves the seals
    // wrap it with the right prologues. Neither test alone would be enough.
    let corpus = corpus();
    let vector = corpus
        .iter()
        .find(|v| v.protocol_name == NOISE_X_PROTOCOL)
        .expect("corpus has the X vector");
    let label = "X vector".to_string();

    assert_eq!(vector.init_prologue, vector.resp_prologue);
    let prologue = bytes(&vector.init_prologue);

    let init_static = key32(&vector.init_static);
    let resp_static = key32(&vector.resp_static);
    let resp_public = key32(
        vector
            .init_remote_static
            .as_deref()
            .expect("the X vector pins the responder's static public key"),
    );

    // The initiator knows the responder's static key up front — that is what
    // makes `X` a one-shot pattern and what makes a courier seal possible
    // without an interactive round trip.
    let mut initiator = NoiseHandshake::for_vectors(
        NOISE_X_PROTOCOL,
        HandshakeRole::Initiator,
        Some(&init_static),
        Some(&resp_public),
        Some(&key32(&vector.init_ephemeral)),
        &prologue,
    )
    .expect("X initiator must build");

    // The responder brings only its own static; it learns the sender's from the
    // encrypted `s` token inside the single message.
    let mut responder = NoiseHandshake::for_vectors(
        NOISE_X_PROTOCOL,
        HandshakeRole::Responder,
        Some(&resp_static),
        None,
        None,
        &prologue,
    )
    .expect("X responder must build");

    assert!(
        vector.resp_ephemeral.is_none(),
        "X is one-way; the responder has no ephemeral to pin"
    );

    exchange(
        &mut initiator,
        &mut responder,
        &vector.messages[0],
        &label,
        1,
    );

    assert!(initiator.is_complete(), "X completes in one message");
    assert!(responder.is_complete(), "X completes in one message");

    assert_eq!(
        responder.remote_static(),
        Some(public_of(&init_static)),
        "the responder must recover the sender's static key — this is the sender \
         authentication that open_courier and open_prekey return to their callers"
    );
    assert_eq!(
        initiator.remote_static(),
        Some(resp_public),
        "the initiator's remote static is the key it sealed to"
    );

    check_handshake_hash(&initiator, &responder, vector, &label);

    // One-way: every transport message flows initiator -> responder.
    let mut initiator_session = initiator.into_session().expect("X initiator splits");
    let mut responder_session = responder.into_session().expect("X responder splits");

    for (offset, message) in vector.messages[X_HANDSHAKE_MESSAGES..].iter().enumerate() {
        let number = X_HANDSHAKE_MESSAGES + offset + 1;
        transport_step(
            &mut initiator_session,
            &mut responder_session,
            message,
            &label,
            number,
        );
    }
}

// ---------------------------------------------------------------------------
// Shared steps
// ---------------------------------------------------------------------------

/// Writes one handshake message from `writer`, asserts it byte for byte against
/// the corpus, then reads it into `reader` and checks the payload survived.
fn exchange(
    writer: &mut NoiseHandshake,
    reader: &mut NoiseHandshake,
    message: &VectorMessage,
    label: &str,
    number: usize,
) {
    let payload = bytes(&message.payload);

    let produced = writer
        .write_message(&payload)
        .unwrap_or_else(|_| panic!("{label}: handshake message {number} must write"));
    assert_eq!(
        hex::encode(&produced),
        message.ciphertext,
        "{label}: handshake message {number} ciphertext diverges from the specification"
    );

    let opened = reader
        .read_message(&produced)
        .unwrap_or_else(|_| panic!("{label}: handshake message {number} must read"));
    assert_eq!(
        opened, payload,
        "{label}: handshake message {number} payload did not survive the round trip"
    );
}

/// Encrypts one transport message, asserts it byte for byte, then decrypts it.
fn transport_step(
    sender: &mut NoiseSession,
    receiver: &mut NoiseSession,
    message: &VectorMessage,
    label: &str,
    number: usize,
) {
    let payload = bytes(&message.payload);

    // Each direction has its own counter, so it is read off the sender rather
    // than derived from the position in the vector.
    let expected_counter = u32::try_from(sender.messages_sent()).expect("few messages");
    let produced = sender
        .encrypt(&payload)
        .unwrap_or_else(|_| panic!("{label}: transport message {number} must encrypt"));

    // bitchat's transport framing is deliberately *not* the Noise
    // specification's: it prefixes each frame with the big-endian message
    // counter (`useExtractedNonce: true`). The corpus is the specification's, so
    // the prefix is asserted separately and then stripped, leaving the bare
    // Noise transport message the vectors actually pin. Everything under the
    // prefix — key schedule, nonce construction, AEAD — must still match byte
    // for byte, which is the property this file exists to guard.
    let (prefix, spec_bytes) = produced.split_at(NONCE_PREFIX_LEN);
    assert_eq!(
        prefix,
        expected_counter.to_be_bytes(),
        "{label}: transport message {number} must carry its counter, big-endian"
    );
    assert_eq!(
        hex::encode(spec_bytes),
        message.ciphertext,
        "{label}: transport message {number} ciphertext diverges from the specification \
         once bitchat's counter prefix is removed. The prefix is expected; a divergence \
         *under* it means the key schedule or the AEAD is wrong."
    );

    let opened = receiver
        .decrypt(&produced)
        .unwrap_or_else(|_| panic!("{label}: transport message {number} must decrypt"));
    assert_eq!(
        opened, payload,
        "{label}: transport message {number} round-trip lost the payload"
    );
}

/// Both sides must agree on the handshake hash, and where the corpus pins one
/// they must both equal it. This is the strongest single assertion available:
/// the hash commits to the whole transcript, prologue included.
fn check_handshake_hash(
    initiator: &NoiseHandshake,
    responder: &NoiseHandshake,
    vector: &Vector,
    label: &str,
) {
    let initiator_hash = initiator
        .handshake_hash()
        .unwrap_or_else(|| panic!("{label}: a complete handshake has a hash"));
    let responder_hash = responder
        .handshake_hash()
        .unwrap_or_else(|| panic!("{label}: a complete handshake has a hash"));

    assert_eq!(
        initiator_hash, responder_hash,
        "{label}: the two sides disagree on the handshake hash"
    );

    if let Some(expected) = vector.handshake_hash.as_deref() {
        assert_eq!(
            hex::encode(initiator_hash),
            expected,
            "{label}: handshake hash does not match the specification"
        );
    }
}

// ---------------------------------------------------------------------------
// Negative: the prologue is genuinely mixed in
// ---------------------------------------------------------------------------

#[test]
fn a_mismatched_prologue_breaks_the_transcript() {
    // Domain separation is only real if the prologue actually reaches the
    // symmetric state. Run the first vector with the responder on a different
    // prologue: message 1 still reads (in XX it is `e` plus a cleartext,
    // unauthenticated payload), but message 2 must not. If this test ever
    // passes trivially, both bitchat prologues are decorative and the courier /
    // prekey domain separation in `conformance.rs` is void.
    let corpus = corpus();
    let vector = corpus
        .iter()
        .find(|v| v.protocol_name == NOISE_XX_PROTOCOL)
        .expect("corpus has an XX vector");

    let prologue = bytes(&vector.init_prologue);
    let mut wrong_prologue = prologue.clone();
    wrong_prologue.push(0x00);

    let init_static = key32(&vector.init_static);
    let resp_static = key32(&vector.resp_static);
    let resp_ephemeral = key32(
        vector
            .resp_ephemeral
            .as_deref()
            .expect("XX vector has a responder ephemeral"),
    );

    let mut initiator = NoiseHandshake::for_vectors(
        NOISE_XX_PROTOCOL,
        HandshakeRole::Initiator,
        Some(&init_static),
        None,
        Some(&key32(&vector.init_ephemeral)),
        &prologue,
    )
    .expect("initiator builds");

    let mut responder = NoiseHandshake::for_vectors(
        NOISE_XX_PROTOCOL,
        HandshakeRole::Responder,
        Some(&resp_static),
        None,
        Some(&resp_ephemeral),
        &wrong_prologue,
    )
    .expect("responder builds");

    let message1 = initiator.write_message(&[]).expect("message 1 writes");
    responder.read_message(&message1).expect("message 1 reads");

    let message2 = responder.write_message(&[]).expect("message 2 writes");
    assert!(
        initiator.read_message(&message2).is_err(),
        "a divergent prologue must fail the handshake"
    );
}

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

#[test]
fn x_vector_pins_the_static_public_key_we_derive() {
    // The `X` entry is the only place in this project where a Curve25519
    // private key and its public key are both pinned by an outside authority.
    // It is therefore the only golden check available on our key derivation,
    // and it is worth having on its own: a clamping or endianness mistake in
    // `MeshIdentity` would put a wrong static key into every announce, every
    // fingerprint and every seal, while every round-trip test still passed.
    let corpus = corpus();
    let vector = corpus
        .iter()
        .find(|v| v.protocol_name == NOISE_X_PROTOCOL)
        .expect("corpus has the X vector");

    let expected = vector
        .init_remote_static
        .as_deref()
        .expect("the X vector pins the responder's static public key");

    assert_eq!(
        hex::encode(public_of(&key32(&vector.resp_static))),
        expected,
        "X25519 public key derivation diverges from the specification vector"
    );

    // The same responder private static appears in the cacophony XX entry, so
    // this pins that entry's key material too.
    let xx = corpus
        .iter()
        .find(|v| v.protocol_name == NOISE_XX_PROTOCOL)
        .expect("corpus has an XX vector");
    assert_eq!(
        xx.resp_static, vector.resp_static,
        "the cacophony XX and X entries share a responder static; if that stopped \
         being true, the corpus was edited"
    );
}

// ---------------------------------------------------------------------------
// Corpus integrity
// ---------------------------------------------------------------------------

#[test]
fn every_vector_targets_a_protocol_this_crate_speaks() {
    // Mirrors bitchat's own guard. A vector for a pattern we do not implement
    // would pass vacuously and quietly reduce coverage to nothing.
    for vector in corpus() {
        assert!(
            vector.protocol_name == NOISE_XX_PROTOCOL || vector.protocol_name == NOISE_X_PROTOCOL,
            "unexpected protocol in the corpus: {}",
            vector.protocol_name
        );
    }
}

#[test]
fn the_corpus_is_the_cacophony_fixture_set() {
    // Tripwire on the copy. The `"John Galt"` prologue and the
    // `"Ludwig von Mises"` first payload are the fixture set's signatures; if a
    // future edit ever replaces them with locally generated values, this suite
    // stops being a conformance suite and nothing else would notice.
    let corpus = corpus();
    assert_eq!(corpus.len(), 3, "two XX entries and one X entry");

    let cacophony: Vec<&Vector> = corpus
        .iter()
        .filter(|v| v.init_prologue == hex::encode(b"John Galt"))
        .collect();
    assert_eq!(
        cacophony.len(),
        2,
        "the two cacophony entries (XX and X) carry the \"John Galt\" prologue"
    );

    for vector in cacophony {
        assert_eq!(
            vector.messages[0].payload,
            hex::encode("Ludwig von Mises"),
            "cacophony's first payload is \"Ludwig von Mises\""
        );
        assert!(
            vector.handshake_hash.is_some(),
            "the cacophony entries pin a handshake hash"
        );
    }
}
