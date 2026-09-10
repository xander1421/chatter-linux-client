//! One-shot Noise `X` seals: store-and-forward envelopes that need no round
//! trip with the recipient.
//!
//! Ported from `Services/NoiseEncryptionService.swift` (`sealCourierPayload`,
//! `openCourierPayload`, `sealPrekeyPayload`, `openPrekeyPayload`). This is the
//! genuinely bitchat-specific part of the crate: `snow` supplies the `X`
//! pattern, but the two prologues and the way the prekey ID is bound into one
//! of them are ours to get exactly right.
//!
//! # The shape of an `X` seal
//!
//! `X` is `<- s` as a pre-message, then a single `e, es, s, ss`. The sender
//! writes it and stops; the recipient reads it and stops. The sender's static
//! key travels *inside* the message, encrypted under a key derived from `es`,
//! and the trailing `ss` binds the payload to it. That is why every `open_*`
//! here returns the sender's static key alongside the payload: dropping it on
//! the floor would turn an authenticated envelope into an anonymous one without
//! any visible symptom.
//!
//! # Domain separation
//!
//! Both prologues are hashed into the transcript before anything else, so a
//! courier ciphertext cannot be reinterpreted as a prekey ciphertext, neither
//! can be confused with an interactive `XX` transcript, and a prekey ciphertext
//! cannot be replayed against a different prekey of the same recipient. The
//! prekey ID is a **big-endian** `u32`; a byte-order mistake here fails open in
//! the worst possible way, since our own seals would still round-trip locally
//! while every real peer rejected them.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::error::NoiseError;
use crate::identity::MeshIdentity;
use crate::session::{MAX_HANDSHAKE_OVERHEAD, MAX_MESSAGE_SIZE};
use crate::{COURIER_PROLOGUE, NOISE_X_PROTOCOL, PREKEY_PROLOGUE_PREFIX};

/// Bytes an `X` seal adds to its payload: the ephemeral public key, the
/// encrypted static public key with its tag, and the payload's own tag.
pub const SEAL_OVERHEAD: usize = MAX_HANDSHAKE_OVERHEAD;

/// Seals `payload` to a peer's long-term static key.
///
/// # No forward secrecy
///
/// This is the protocol's main cryptographic trade-off, and it is deliberate.
/// The recipient's static key is the only key protecting the envelope, so
/// anyone who later compromises that key can decrypt every courier envelope
/// they captured in transit. Use an established session whenever the peer is
/// reachable, and [`seal_prekey`] when it is not — that one is forward secret
/// once the prekey is consumed and its grace window lapses.
pub fn seal_courier(
    payload: &[u8],
    recipient_static: &[u8; 32],
    sender: &MeshIdentity,
) -> Result<Vec<u8>, NoiseError> {
    seal(
        payload,
        recipient_static,
        sender.static_secret(),
        COURIER_PROLOGUE,
    )
}

/// Opens a courier envelope addressed to our static key.
///
/// Returns the payload and the sender's **authenticated** static public key.
/// The pairing is the point: the `ss` operation in the `X` pattern binds the
/// sender's identity to the ciphertext, so a caller that discards the key is
/// treating an authenticated message as an anonymous one.
pub fn open_courier(
    ciphertext: &[u8],
    recipient: &MeshIdentity,
) -> Result<(Vec<u8>, [u8; 32]), NoiseError> {
    open(ciphertext, recipient.static_secret(), COURIER_PROLOGUE)
}

/// Seals `payload` to one of the recipient's gossiped one-time prekeys.
///
/// The prekey public key stands in for the recipient's identity key as the `X`
/// responder static, and `prekey_id` is bound into the prologue. Unlike
/// [`seal_courier`] this is forward secret: once the recipient consumes the
/// prekey and deletes its private half, captured ciphertext stays undecryptable
/// even if the recipient's identity key is later compromised. The sender's
/// static still rides inside, so authentication is unchanged.
///
/// Takes the ID and the raw public key rather than a bundle entry so that
/// sealing does not depend on the bundle format.
pub fn seal_prekey(
    payload: &[u8],
    prekey_id: u32,
    prekey_public: &[u8; 32],
    sender: &MeshIdentity,
) -> Result<Vec<u8>, NoiseError> {
    seal(
        payload,
        prekey_public,
        sender.static_secret(),
        &prekey_prologue(prekey_id),
    )
}

/// Opens an envelope sealed to one of our one-time prekeys.
///
/// `prekey_id` must be the ID the sender used: it is inside the prologue, so
/// opening under any other ID fails exactly as if the ciphertext were forged.
/// `prekey_secret` is the private half of that prekey — the recipient's
/// identity key plays no part in an `X` seal's key agreement.
///
/// Returns the payload and the sender's authenticated static public key, the
/// same contract as [`open_courier`]. Consuming the prekey afterwards is the
/// caller's business; this crate holds no prekey store.
///
/// `recipient` is accepted for symmetry with [`open_courier`] and so callers
/// need not special-case which identity a seal was addressed to. It is
/// deliberately not used in the key agreement — substituting the identity key
/// here would silently destroy the forward secrecy that prekeys exist for.
pub fn open_prekey(
    ciphertext: &[u8],
    prekey_id: u32,
    prekey_secret: &[u8; 32],
    _recipient: &MeshIdentity,
) -> Result<(Vec<u8>, [u8; 32]), NoiseError> {
    open(ciphertext, prekey_secret, &prekey_prologue(prekey_id))
}

/// The prologue for a prekey-sealed envelope: `"bitchat-prekey-v1"` followed by
/// `prekey_id` as a big-endian `u32`.
///
/// Byte-exact by contract with `NoiseEncryptionService.prekeyPrologue(for:)`.
pub fn prekey_prologue(prekey_id: u32) -> Vec<u8> {
    let mut prologue = Vec::with_capacity(PREKEY_PROLOGUE_PREFIX.len() + 4);
    prologue.extend_from_slice(PREKEY_PROLOGUE_PREFIX);
    prologue.extend_from_slice(&prekey_id.to_be_bytes());
    prologue
}

/// Writes the single `X` message.
///
/// Deliberately offers no way to pin the ephemeral: a seal whose ephemeral is
/// predictable is a seal an observer can decrypt. The vector harness drives the
/// `X` pattern through [`crate::session::NoiseHandshake`] instead, which gates
/// its pinned-ephemeral constructors behind a cfg.
fn seal(
    payload: &[u8],
    recipient_static: &[u8; 32],
    local_static: &[u8; 32],
    prologue: &[u8],
) -> Result<Vec<u8>, NoiseError> {
    validate_public_key(recipient_static)?;
    if payload.len() > MAX_MESSAGE_SIZE.saturating_sub(SEAL_OVERHEAD) {
        return Err(NoiseError::Encrypt);
    }

    let mut state: HandshakeState = Builder::new(x_params()?)
        .local_private_key(local_static)
        .map_err(setup_error)?
        .remote_public_key(recipient_static)
        .map_err(setup_error)?
        .prologue(prologue)
        .map_err(setup_error)?
        .build_initiator()
        .map_err(setup_error)?;

    let mut buffer = vec![0u8; payload.len() + SEAL_OVERHEAD];
    let written = state
        .write_message(payload, &mut buffer)
        .map_err(|_| NoiseError::Encrypt)?;
    buffer.truncate(written);
    Ok(buffer)
}

/// Reads the single `X` message. Every failure — wrong recipient, wrong
/// prologue, wrong prekey ID, flipped bit, truncation — is
/// [`NoiseError::Decrypt`] and nothing more, so an attacker probing us with
/// crafted envelopes learns nothing about which part was wrong.
fn open(
    ciphertext: &[u8],
    local_static: &[u8; 32],
    prologue: &[u8],
) -> Result<(Vec<u8>, [u8; 32]), NoiseError> {
    if ciphertext.len() < SEAL_OVERHEAD || ciphertext.len() > MAX_MESSAGE_SIZE {
        return Err(NoiseError::Decrypt);
    }

    let mut state: HandshakeState = Builder::new(x_params()?)
        .local_private_key(local_static)
        .map_err(setup_error)?
        .prologue(prologue)
        .map_err(setup_error)?
        .build_responder()
        .map_err(setup_error)?;

    let mut buffer = vec![0u8; ciphertext.len()];
    let written = state
        .read_message(ciphertext, &mut buffer)
        .map_err(|_| NoiseError::Decrypt)?;
    buffer.truncate(written);

    let sender = state
        .get_remote_static()
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .ok_or(NoiseError::MissingRemoteStatic)?;
    // bitchat validates the decrypted `s` token before accepting it. The
    // envelope has already authenticated at this point, so naming the reason
    // gives an attacker nothing they did not already control.
    validate_public_key(&sender)?;

    Ok((buffer, sender))
}

fn x_params() -> Result<NoiseParams, NoiseError> {
    NOISE_X_PROTOCOL
        .parse()
        .map_err(|_| NoiseError::Setup(format!("unsupported protocol name {NOISE_X_PROTOCOL}")))
}

fn setup_error(err: snow::Error) -> NoiseError {
    NoiseError::Setup(format!("{err:?}"))
}

/// Rejects Curve25519 public keys that would make key agreement degenerate.
///
/// Ported verbatim from `NoiseHandshakeState.validatePublicKey`, including the
/// exact point list — these are the all-zero point, small-order points, and
/// non-canonical encodings that reduce to them, all of which drive the DH
/// output to a value an attacker knows in advance. `snow` performs no such
/// check, so it has to happen here.
///
/// The comparison is constant time and never short-circuits: it accumulates
/// across the whole list before deciding, so timing does not reveal *which*
/// point was matched.
pub(crate) fn validate_public_key(key: &[u8; 32]) -> Result<(), NoiseError> {
    let mut matched = false;
    for point in &LOW_ORDER_POINTS {
        matched |= constant_time_eq(key, point);
    }
    if matched {
        return Err(NoiseError::InvalidPublicKey);
    }
    Ok(())
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

/// The rejection list.
///
/// Started as a verbatim port of `NoiseProtocol.swift`. Checking that port by
/// arithmetic rather than by citation found it wrong in both directions
/// (OPEN-ITEMS §102):
///
///   * It **omitted** the canonical `p-1`, `p` and `p+1`, and the non-canonical
///     `2p-1`. Each drives the X25519 shared secret to all zeros, so each was a
///     real hole; they are added here.
///   * It **contained two points that are not degenerate at all**:
///     `[0u8; 31] ++ [0x01]`, commented "u = 2^248" and almost certainly `u = 1`
///     written big-endian by mistake, and `[0xff; 32]`, which reduces to 18 mod
///     p — an ordinary point. Neither produces a zero shared secret. Rejecting a
///     valid key can only turn away an honest peer, so both are gone.
///
/// What remains is exactly libsodium's `has_small_order` set: the twelve
/// encodings of the points of order 1, 2, 4 and 8, canonical and not. Seven are
/// degenerate to this build directly; the five with bit 255 set are non-canonical
/// encodings that RFC 7748 masking defuses here but that an implementation
/// skipping the mask would honour, and no honest peer sends one.
///
/// `every_listed_point_is_degenerate_or_a_non_canonical_encoding` is what found
/// them, on its first run.
const LOW_ORDER_POINTS: [[u8; 32]; 12] = [
    // Point at infinity / all-zero key.
    [0x00; 32],
    // u = 1, order 4.
    [
        0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0,
    ],
    // Order 8.
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0x00,
    ],
    // Order 8.
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0x57,
    ],
    // Non-canonical encoding congruent to 0 mod p.
    [
        0xda, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff,
    ],
    // The order-8 point above, plus p: the same point, non-canonically encoded.
    [
        0xcd, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0xff,
    ],
    // The other order-8 point plus p.
    [
        0x4c, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0xd7,
    ],
    // p - 1: the order-2 point. Canonical, and absent from the Swift list.
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // p: reduces to zero. Canonical, and absent from the Swift list.
    [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // p + 1: reduces to one. Canonical, and absent from the Swift list.
    [
        0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // 2p - 1, the non-canonical sibling of p - 1.
    [
        0xd9, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff,
    ],
    // Non-canonical encoding congruent to 1 mod p.
    [
        0xdb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff,
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> MeshIdentity {
        MeshIdentity::from_keys([1u8; 32], [11u8; 32])
    }

    fn recipient() -> MeshIdentity {
        MeshIdentity::from_keys([2u8; 32], [22u8; 32])
    }

    fn stranger() -> MeshIdentity {
        MeshIdentity::from_keys([3u8; 32], [33u8; 32])
    }

    /// A one-time prekey: any Curve25519 keypair, unrelated to the identity.
    fn prekey(seed: u8) -> ([u8; 32], [u8; 32]) {
        let secret = [seed; 32];
        let public =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret)).to_bytes();
        (secret, public)
    }

    // -- prologues ---------------------------------------------------------

    #[test]
    fn the_courier_prologue_is_the_exact_bitchat_string() {
        assert_eq!(COURIER_PROLOGUE, b"bitchat-courier-v1");
        assert_eq!(COURIER_PROLOGUE.len(), 18);
    }

    #[test]
    fn the_prekey_prologue_appends_the_id_big_endian() {
        assert_eq!(PREKEY_PROLOGUE_PREFIX, b"bitchat-prekey-v1");
        assert_eq!(PREKEY_PROLOGUE_PREFIX.len(), 17);

        let prologue = prekey_prologue(0x0102_0304);
        assert_eq!(prologue.len(), 21);
        assert_eq!(&prologue[..17], b"bitchat-prekey-v1");
        // Big-endian: most significant byte first. Little-endian would be
        // [04, 03, 02, 01] and would round-trip locally while failing against
        // every real peer.
        assert_eq!(&prologue[17..], &[0x01, 0x02, 0x03, 0x04]);

        assert_eq!(&prekey_prologue(0)[17..], &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&prekey_prologue(1)[17..], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&prekey_prologue(u32::MAX)[17..], &[0xff, 0xff, 0xff, 0xff]);
        // A byte-swapped ID must not collide with the original.
        assert_ne!(prekey_prologue(1), prekey_prologue(0x0100_0000));
    }

    // -- courier seals -----------------------------------------------------

    #[test]
    fn a_courier_seal_round_trips_and_names_its_sender() {
        let (from, to) = (sender(), recipient());
        let sealed = seal_courier(b"meet at the docks", &to.static_public(), &from).unwrap();

        let (payload, sender_key) = open_courier(&sealed, &to).unwrap();
        assert_eq!(payload.as_slice(), b"meet at the docks");
        // The whole point of the X pattern: the recipient learns who sent it.
        assert_eq!(sender_key, from.static_public());
        assert_ne!(sender_key, to.static_public());
    }

    #[test]
    fn a_courier_seal_costs_exactly_the_x_pattern_overhead() {
        let (from, to) = (sender(), recipient());
        let sealed = seal_courier(b"", &to.static_public(), &from).unwrap();
        assert_eq!(sealed.len(), SEAL_OVERHEAD);
        assert_eq!(SEAL_OVERHEAD, 96);

        let sealed = seal_courier(&[7u8; 200], &to.static_public(), &from).unwrap();
        assert_eq!(sealed.len(), 200 + SEAL_OVERHEAD);
    }

    #[test]
    fn an_empty_courier_payload_round_trips() {
        let (from, to) = (sender(), recipient());
        let sealed = seal_courier(b"", &to.static_public(), &from).unwrap();
        let (payload, sender_key) = open_courier(&sealed, &to).unwrap();
        assert!(payload.is_empty());
        assert_eq!(sender_key, from.static_public());
    }

    #[test]
    fn two_seals_of_the_same_payload_differ() {
        let (from, to) = (sender(), recipient());
        let first = seal_courier(b"same text", &to.static_public(), &from).unwrap();
        let second = seal_courier(b"same text", &to.static_public(), &from).unwrap();
        // Fresh ephemeral per seal; identical ciphertexts would mean the
        // ephemeral was not fresh.
        assert_ne!(first, second);
        assert_ne!(first[..32], second[..32]);
    }

    #[test]
    fn the_wrong_recipient_cannot_open_a_courier_seal() {
        let (from, to) = (sender(), recipient());
        let sealed = seal_courier(b"private", &to.static_public(), &from).unwrap();
        assert_eq!(open_courier(&sealed, &stranger()), Err(NoiseError::Decrypt));
        // Not even the sender can reopen it: X gives the writer no read side.
        assert_eq!(open_courier(&sealed, &from), Err(NoiseError::Decrypt));
    }

    #[test]
    fn a_tampered_courier_seal_is_rejected() {
        let (from, to) = (sender(), recipient());
        let original = seal_courier(b"authentic", &to.static_public(), &from).unwrap();

        // Every byte matters: the ephemeral, the encrypted static, the payload
        // and the tag are all covered.
        for index in [0usize, 31, 32, 79, 80, original.len() - 1] {
            let mut tampered = original.clone();
            tampered[index] ^= 0x01;
            assert_eq!(
                open_courier(&tampered, &recipient()),
                Err(NoiseError::Decrypt),
                "tampering at byte {index} was not detected"
            );
        }
    }

    #[test]
    fn a_truncated_courier_seal_is_rejected_without_panicking() {
        let (from, to) = (sender(), recipient());
        let sealed = seal_courier(b"authentic", &to.static_public(), &from).unwrap();
        for len in 0..sealed.len() {
            assert_eq!(
                open_courier(&sealed[..len], &recipient()),
                Err(NoiseError::Decrypt)
            );
        }
    }

    // -- prekey seals ------------------------------------------------------

    #[test]
    fn a_prekey_seal_round_trips_and_names_its_sender() {
        let (from, to) = (sender(), recipient());
        let (secret, public) = prekey(5);

        let sealed = seal_prekey(b"offline mail", 7, &public, &from).unwrap();
        let (payload, sender_key) = open_prekey(&sealed, 7, &secret, &to).unwrap();

        assert_eq!(payload.as_slice(), b"offline mail");
        assert_eq!(sender_key, from.static_public());
    }

    #[test]
    fn a_prekey_seal_opened_under_the_wrong_id_fails() {
        let (from, to) = (sender(), recipient());
        let (secret, public) = prekey(5);
        let sealed = seal_prekey(b"bound to prekey 7", 7, &public, &from).unwrap();

        // Right key, right ciphertext, wrong ID. Only the prologue differs, so
        // this failing is exactly the proof that the ID is bound into it.
        for wrong_id in [0u32, 6, 8, 0x0700_0000, u32::MAX] {
            assert_eq!(
                open_prekey(&sealed, wrong_id, &secret, &to),
                Err(NoiseError::Decrypt),
                "prekey id {wrong_id} should not have opened a seal bound to 7"
            );
        }
        // And the correct ID still works, so the failures above are about the
        // binding rather than a broken seal.
        assert!(open_prekey(&sealed, 7, &secret, &to).is_ok());
    }

    #[test]
    fn a_prekey_seal_opened_with_the_wrong_prekey_fails() {
        let (from, to) = (sender(), recipient());
        let (_, public) = prekey(5);
        let (other_secret, _) = prekey(6);
        let sealed = seal_prekey(b"offline mail", 7, &public, &from).unwrap();

        assert_eq!(
            open_prekey(&sealed, 7, &other_secret, &to),
            Err(NoiseError::Decrypt)
        );
        // The recipient's identity key is not the prekey and must not open it.
        assert_eq!(
            open_prekey(&sealed, 7, to.static_secret(), &to),
            Err(NoiseError::Decrypt)
        );
    }

    #[test]
    fn a_tampered_prekey_seal_is_rejected() {
        let (from, to) = (sender(), recipient());
        let (secret, public) = prekey(5);
        let original = seal_prekey(b"authentic", 7, &public, &from).unwrap();

        for index in [0usize, 32, original.len() - 1] {
            let mut tampered = original.clone();
            tampered[index] ^= 0x80;
            assert_eq!(
                open_prekey(&tampered, 7, &secret, &to),
                Err(NoiseError::Decrypt)
            );
        }
    }

    // -- domain separation between the two seal types ----------------------

    #[test]
    fn the_two_seal_types_are_not_interchangeable() {
        let (from, to) = (sender(), recipient());
        let (secret, public) = prekey(5);

        // A courier seal is addressed to the identity key; opening it as a
        // prekey seal fails on the key *and* on the prologue.
        let courier = seal_courier(b"courier", &to.static_public(), &from).unwrap();
        assert_eq!(
            open_prekey(&courier, 7, to.static_secret(), &to),
            Err(NoiseError::Decrypt)
        );

        // A prekey seal opened as a courier envelope fails on the prologue even
        // when the reader holds the right private key.
        let prekey_sealed = seal_prekey(b"prekey", 7, &public, &from).unwrap();
        let holder = MeshIdentity::from_keys(secret, [0x44; 32]);
        assert_eq!(
            open_courier(&prekey_sealed, &holder),
            Err(NoiseError::Decrypt)
        );

        // Sanity: that identity really does hold the prekey's private half, so
        // the failure above is the prologue and not the key.
        assert_eq!(holder.static_public(), public);
        assert!(open_prekey(&prekey_sealed, 7, &secret, &to).is_ok());
    }

    // -- key validation ----------------------------------------------------

    #[test]
    fn sealing_to_a_degenerate_key_is_refused() {
        let from = sender();
        for bad in &LOW_ORDER_POINTS {
            assert_eq!(
                seal_courier(b"payload", bad, &from),
                Err(NoiseError::InvalidPublicKey)
            );
            assert_eq!(
                seal_prekey(b"payload", 1, bad, &from),
                Err(NoiseError::InvalidPublicKey)
            );
        }
    }

    /// §102 — the list is justified by arithmetic, not by citation.
    ///
    /// An entry earns its place one of two ways, and this test insists on one of
    /// them:
    ///
    ///   1. **It is degenerate as this implementation sees it.** X25519 masks bit
    ///      255 of the u-coordinate before doing anything (RFC 7748 §5), so the
    ///      test simply computes the shared secret: a point of order 1, 2, 4 or 8
    ///      drives it to all zeros whatever the scalar.
    ///   2. **It has bit 255 set.** Then it is a non-canonical encoding, harmless
    ///      *here* because the masking makes it something else — but not harmless
    ///      everywhere, since an implementation that skips the mask sees the small
    ///      order point it encodes. No honest peer ever sends one: a real
    ///      u-coordinate is less than p, so its top bit is clear. Rejecting these
    ///      costs nothing and covers the peer who is not running dalek.
    ///
    /// The first clause is what caught the two entries the Swift port had wrong:
    /// `[0u8; 31] ++ [0x01]` ("u = 2^248", almost certainly `u = 1` written
    /// big-endian) and `[0xff; 32]` (reduces to 18) are both perfectly ordinary
    /// points with bit 255 clear, and rejecting a valid key can only turn away an
    /// honest peer.
    #[test]
    fn every_listed_point_is_degenerate_or_a_non_canonical_encoding() {
        use x25519_dalek::{PublicKey, StaticSecret};

        for (index, point) in LOW_ORDER_POINTS.iter().enumerate() {
            let non_canonical = point[31] & 0x80 != 0;

            // Any scalar will do — the point of a degenerate point is that the
            // output does not depend on it.
            let degenerate = [1u8, 42, 200].iter().all(|seed| {
                let secret = StaticSecret::from([*seed; 32]);
                secret.diffie_hellman(&PublicKey::from(*point)).as_bytes() == &[0u8; 32]
            });

            assert!(
                degenerate || non_canonical,
                "LOW_ORDER_POINTS[{index}] is rejected but is neither degenerate nor a \
                 non-canonical encoding: it is an ordinary point, and refusing it turns \
                 away an honest peer"
            );
        }
    }

    /// The canonical half of the list must be degenerate outright — no appeal to
    /// other implementations. These are the ones that matter to this build.
    #[test]
    fn every_canonical_listed_point_zeroes_the_shared_secret() {
        use x25519_dalek::{PublicKey, StaticSecret};

        let secret = StaticSecret::from([9u8; 32]);
        let mut canonical = 0;
        for point in LOW_ORDER_POINTS.iter().filter(|p| p[31] & 0x80 == 0) {
            canonical += 1;
            assert_eq!(
                secret.diffie_hellman(&PublicKey::from(*point)).as_bytes(),
                &[0u8; 32]
            );
        }
        assert_eq!(
            canonical, 7,
            "the canonical small-order points are 0, 1, the two of order 8, p-1, p and p+1"
        );
    }

    /// The other direction: an honest key must survive. A rejection list that
    /// over-reaches is a denial of service against real peers.
    #[test]
    fn honest_keys_pass_validation() {
        assert!(validate_public_key(&sender().static_public()).is_ok());
        assert!(validate_public_key(&recipient().static_public()).is_ok());
        assert!(validate_public_key(&prekey(5).1).is_ok());
    }

    #[test]
    fn constant_time_eq_agrees_with_ordinary_equality() {
        let a = [7u8; 32];
        let mut b = [7u8; 32];
        assert!(constant_time_eq(&a, &b));
        b[31] ^= 0x01;
        assert!(!constant_time_eq(&a, &b));
        b[31] ^= 0x01;
        b[0] ^= 0x80;
        assert!(!constant_time_eq(&a, &b));
    }

    // -- size limits -------------------------------------------------------

    #[test]
    fn an_oversized_payload_is_refused_before_allocating() {
        let (from, to) = (sender(), recipient());
        let huge = vec![0u8; MAX_MESSAGE_SIZE];
        assert_eq!(
            seal_courier(&huge, &to.static_public(), &from),
            Err(NoiseError::Encrypt)
        );

        let (_, public) = prekey(5);
        assert_eq!(
            seal_prekey(&huge, 1, &public, &from),
            Err(NoiseError::Encrypt)
        );
    }

    #[test]
    fn the_largest_payload_that_fits_still_round_trips() {
        let (from, to) = (sender(), recipient());
        let payload = vec![0xa5u8; MAX_MESSAGE_SIZE - SEAL_OVERHEAD];
        let sealed = seal_courier(&payload, &to.static_public(), &from).unwrap();
        assert_eq!(sealed.len(), MAX_MESSAGE_SIZE);

        let (opened, sender_key) = open_courier(&sealed, &to).unwrap();
        assert_eq!(opened, payload);
        assert_eq!(sender_key, from.static_public());
    }
}
