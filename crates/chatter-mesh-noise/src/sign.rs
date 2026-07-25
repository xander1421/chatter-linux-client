//! Ed25519 signatures: canonical announce bytes, packet signing, raw signing.
//!
//! Ported from `Services/NoiseEncryptionService.swift` —
//! `canonicalAnnounceBytes`, `buildAnnounceSignature`, `verifyAnnounceSignature`,
//! `signPacket`, `verifyPacketSignature`, `signData`, `verifySignature`.
//!
//! Two rules run through this whole module.
//!
//! **Canonicalisation never fails.** Every fixed-width field in the announce
//! preimage is truncated or zero-padded to its width; a short peer ID or an
//! oversized nickname produces *different bytes*, not an error. That is what the
//! Swift does, and a Rust version that returned an error instead would simply be
//! unable to verify announces that real peers happily sign.
//!
//! **Verification returns `bool`, never `Result`.** Every input to a `verify_*`
//! function arrived over a radio from someone we have not authenticated yet. A
//! malformed signature, a public key that is not a valid Edwards point, and an
//! honest mismatch are all the same answer: `false`. Nothing here panics on
//! attacker-supplied bytes.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use chatter_mesh::BitchatPacket;

use crate::ANNOUNCE_CONTEXT;
use crate::error::NoiseError;
use crate::identity::MeshIdentity;

/// Width of the routing peer ID inside the announce preimage.
const PEER_ID_WIDTH: usize = 8;
/// Width of both public keys inside the announce preimage.
const KEY_WIDTH: usize = 32;
/// Longest length-prefixed field, since the prefix is a single byte.
const MAX_PREFIXED_LEN: usize = 255;

/// The exact bytes an announce signature covers.
///
/// Mirrors `canonicalAnnounceBytes`:
///
/// ```text
/// u8(min(len(context), 255)) ‖ context[..255]     context = "bitchat-announce-v1"
/// peer_id                       8 bytes,  truncated or zero-padded on the right
/// noise_static_public_key      32 bytes,  truncated or zero-padded on the right
/// ed25519_public_key           32 bytes,  truncated or zero-padded on the right
/// u8(min(len(nickname), 255)) ‖ nickname_utf8[..255]
/// be_u64(timestamp_ms)
/// ```
///
/// The three key/ID fields take `&[u8]` rather than fixed arrays precisely
/// because the truncate-or-pad behaviour is observable: a peer that announces a
/// 6-byte ID gets it zero-extended, and we must reproduce that to verify them.
///
/// The nickname is truncated at 255 *bytes*, not characters, matching Swift's
/// `Data(nickname.utf8).prefix(255)`. That can split a multi-byte scalar; the
/// result is a byte string, never re-decoded as UTF-8, so that is harmless and
/// deviating from it would break interoperability.
pub fn canonical_announce_bytes(
    peer_id: &[u8],
    noise_key: &[u8],
    ed25519_key: &[u8],
    nickname: &str,
    timestamp_ms: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        1 + ANNOUNCE_CONTEXT.len() + PEER_ID_WIDTH + 2 * KEY_WIDTH + 1 + nickname.len() + 8,
    );

    append_length_prefixed(&mut out, ANNOUNCE_CONTEXT);
    append_fixed_width(&mut out, peer_id, PEER_ID_WIDTH);
    append_fixed_width(&mut out, noise_key, KEY_WIDTH);
    append_fixed_width(&mut out, ed25519_key, KEY_WIDTH);
    append_length_prefixed(&mut out, nickname.as_bytes());
    out.extend_from_slice(&timestamp_ms.to_be_bytes());

    out
}

/// Signs our own announce: the noise and signing keys come from `identity`.
///
/// `peer_id` is passed separately because the routing ID on the wire is the one
/// in the packet header, which a caller may have rotated independently of the
/// key it is derived from.
pub fn sign_announce(
    identity: &MeshIdentity,
    peer_id: &[u8],
    nickname: &str,
    timestamp_ms: u64,
) -> [u8; 64] {
    let message = canonical_announce_bytes(
        peer_id,
        &identity.static_public(),
        &identity.signing_public(),
        nickname,
        timestamp_ms,
    );
    sign_bytes(identity, &message)
}

/// Verifies a peer's announce signature against the Ed25519 key it advertises.
///
/// Note what this does and does not prove. It proves that whoever holds
/// `public_key` bound *these* four fields together at *this* timestamp. It does
/// not prove `public_key` belongs to anyone in particular — that is what the
/// fingerprint and the verification flow are for. Callers must check
/// `ed25519_key == public_key` if they intend the announce to be self-signed;
/// passing a different key here is meaningful (a third party attesting to a
/// binding) and is therefore not rejected at this layer.
#[allow(clippy::too_many_arguments)]
pub fn verify_announce(
    signature: &[u8; 64],
    peer_id: &[u8],
    noise_key: &[u8],
    ed25519_key: &[u8],
    nickname: &str,
    timestamp_ms: u64,
    public_key: &[u8; 32],
) -> bool {
    let message =
        canonical_announce_bytes(peer_id, noise_key, ed25519_key, nickname, timestamp_ms);
    verify_bytes(signature, &message, public_key)
}

/// Signs a packet over [`BitchatPacket::signing_preimage`].
///
/// That preimage is the **padded** re-encoding with `ttl = 0`, no signature, and
/// `is_rsr = false` — all three of those mutate in flight, so excluding them is
/// what lets a signature survive relaying. P0 owns and tests the preimage; this
/// function only signs it.
///
/// Returns the bare signature rather than a signed packet: the caller decides
/// where it goes, and `BitchatPacket` is P0's type to construct.
pub fn sign_packet(
    identity: &MeshIdentity,
    packet: &BitchatPacket,
) -> Result<[u8; 64], NoiseError> {
    let preimage = packet.signing_preimage()?;
    Ok(sign_bytes(identity, &preimage))
}

/// Verifies a packet's attached signature.
///
/// `false` for an unsigned packet, for a packet whose preimage cannot be
/// re-encoded (an oversized payload, say), and for a genuine mismatch. A relay
/// that decremented the TTL still verifies, which is the entire reason the TTL
/// is outside the preimage.
pub fn verify_packet(packet: &BitchatPacket, public_key: &[u8; 32]) -> bool {
    let Some(signature) = packet.signature else {
        return false;
    };
    let Ok(preimage) = packet.signing_preimage() else {
        return false;
    };
    verify_bytes(&signature, &preimage, public_key)
}

/// Raw Ed25519 signature over arbitrary bytes. Mirrors `signData`.
///
/// Infallible: `ed25519-dalek` derives a valid keypair from any 32 secret bytes,
/// so unlike Swift's optional-returning version there is no failure to report.
pub fn sign_bytes(identity: &MeshIdentity, data: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(identity.signing_secret())
        .sign(data)
        .to_bytes()
}

/// Raw Ed25519 verification. Mirrors `verifySignature`.
///
/// Uses `verify_strict`, which additionally rejects small-order and
/// non-canonically-encoded public keys. Honest signatures are unaffected —
/// this only narrows the set of adversarial keys that could produce a signature
/// verifying under two different public keys, and the strict direction is the
/// safe one for a protocol where the public key *is* the identity.
pub fn verify_bytes(signature: &[u8; 64], data: &[u8], public_key: &[u8; 32]) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    verifying_key
        .verify_strict(data, &Signature::from_bytes(signature))
        .is_ok()
}

/// `u8(min(len, 255)) ‖ field[..255]`.
fn append_length_prefixed(out: &mut Vec<u8>, field: &[u8]) {
    let len = field.len().min(MAX_PREFIXED_LEN);
    // `len <= 255` by construction, so the cast cannot wrap.
    out.push(len as u8);
    out.extend_from_slice(&field[..len]);
}

/// Truncates to `width` or zero-pads on the right. Never fails — see the module
/// docs for why that matters.
fn append_fixed_width(out: &mut Vec<u8>, field: &[u8], width: usize) {
    let taken = field.len().min(width);
    out.extend_from_slice(&field[..taken]);
    out.resize(out.len() + (width - taken), 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};

    const STATIC_SECRET: [u8; 32] = [0x11; 32];
    const SIGNING_SECRET: [u8; 32] = [0x22; 32];
    const PEER_ID: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
    const TIMESTAMP: u64 = 1_700_000_000_123;

    fn identity() -> MeshIdentity {
        MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET)
    }

    // MARK: canonical bytes — layout

    #[test]
    fn canonical_bytes_have_the_documented_layout() {
        let noise = [0x33u8; 32];
        let ed = [0x44u8; 32];
        let bytes = canonical_announce_bytes(&PEER_ID, &noise, &ed, "alice", TIMESTAMP);

        let context_len = ANNOUNCE_CONTEXT.len();
        assert_eq!(context_len, 19, "\"bitchat-announce-v1\" is 19 bytes");
        assert_eq!(bytes.len(), 1 + context_len + 8 + 32 + 32 + 1 + 5 + 8);

        let mut cursor = 0;
        assert_eq!(bytes[cursor], context_len as u8);
        cursor += 1;
        assert_eq!(&bytes[cursor..cursor + context_len], ANNOUNCE_CONTEXT);
        cursor += context_len;
        assert_eq!(&bytes[cursor..cursor + 8], &PEER_ID);
        cursor += 8;
        assert_eq!(&bytes[cursor..cursor + 32], &noise);
        cursor += 32;
        assert_eq!(&bytes[cursor..cursor + 32], &ed);
        cursor += 32;
        assert_eq!(bytes[cursor], 5);
        cursor += 1;
        assert_eq!(&bytes[cursor..cursor + 5], b"alice");
        cursor += 5;
        assert_eq!(&bytes[cursor..], &TIMESTAMP.to_be_bytes());
    }

    #[test]
    fn short_fields_are_zero_padded_on_the_right() {
        // 3-byte peer ID, 1-byte noise key, empty ed key.
        let bytes = canonical_announce_bytes(&[1, 2, 3], &[0xFF], &[], "", TIMESTAMP);
        let offset = 1 + ANNOUNCE_CONTEXT.len();

        assert_eq!(&bytes[offset..offset + 8], &[1, 2, 3, 0, 0, 0, 0, 0]);

        let noise_at = offset + 8;
        let mut expected_noise = [0u8; 32];
        expected_noise[0] = 0xFF;
        assert_eq!(&bytes[noise_at..noise_at + 32], &expected_noise);

        let ed_at = noise_at + 32;
        assert_eq!(&bytes[ed_at..ed_at + 32], &[0u8; 32]);

        // Empty nickname is a zero-length prefix, not an omitted field.
        assert_eq!(bytes[ed_at + 32], 0);
        assert_eq!(bytes.len(), offset + 8 + 32 + 32 + 1 + 8);
    }

    #[test]
    fn over_long_fields_are_truncated() {
        let long_peer = [0x77u8; 20];
        let long_key = [0x88u8; 40];
        let bytes = canonical_announce_bytes(&long_peer, &long_key, &long_key, "x", TIMESTAMP);
        let offset = 1 + ANNOUNCE_CONTEXT.len();

        assert_eq!(&bytes[offset..offset + 8], &[0x77u8; 8]);
        assert_eq!(&bytes[offset + 8..offset + 40], &[0x88u8; 32]);
        assert_eq!(&bytes[offset + 40..offset + 72], &[0x88u8; 32]);
        assert_eq!(bytes.len(), offset + 8 + 32 + 32 + 1 + 1 + 8);
    }

    #[test]
    fn nickname_is_truncated_to_255_bytes() {
        let long_nick = "n".repeat(300);
        let bytes = canonical_announce_bytes(&PEER_ID, &[0; 32], &[0; 32], &long_nick, TIMESTAMP);
        let nick_len_at = 1 + ANNOUNCE_CONTEXT.len() + 8 + 32 + 32;

        assert_eq!(bytes[nick_len_at], 255);
        assert_eq!(&bytes[nick_len_at + 1..nick_len_at + 256], &[b'n'; 255]);
        assert_eq!(bytes.len(), nick_len_at + 1 + 255 + 8);
    }

    #[test]
    fn nickname_truncation_counts_bytes_not_characters() {
        // 128 two-byte scalars = 256 bytes, one over the limit. Swift truncates
        // the Data, so the 128th scalar is cut in half; we must do the same.
        let nick = "é".repeat(128);
        assert_eq!(nick.len(), 256);
        let bytes = canonical_announce_bytes(&PEER_ID, &[0; 32], &[0; 32], &nick, TIMESTAMP);
        let nick_len_at = 1 + ANNOUNCE_CONTEXT.len() + 8 + 32 + 32;

        assert_eq!(bytes[nick_len_at], 255);
        // Last emitted byte is the lead byte of a split scalar.
        assert_eq!(bytes[nick_len_at + 255], 0xC3);
    }

    #[test]
    fn a_padded_short_field_is_indistinguishable_from_the_padded_form() {
        // This is the property that makes truncate-or-pad safe to rely on: it is
        // a total function, so a peer sending 6 bytes and a peer sending the same
        // 6 bytes zero-extended sign identical preimages.
        let short = canonical_announce_bytes(&[9; 6], &[0; 32], &[0; 32], "n", 1);
        let padded = canonical_announce_bytes(&[9, 9, 9, 9, 9, 9, 0, 0], &[0; 32], &[0; 32], "n", 1);
        assert_eq!(short, padded);
    }

    // MARK: announce sign/verify

    #[test]
    fn announce_signature_round_trips() {
        let id = identity();
        let signature = sign_announce(&id, &PEER_ID, "alice", TIMESTAMP);
        assert!(verify_announce(
            &signature,
            &PEER_ID,
            &id.static_public(),
            &id.signing_public(),
            "alice",
            TIMESTAMP,
            &id.signing_public(),
        ));
    }

    #[test]
    fn announce_verification_fails_on_each_mutated_field() {
        let id = identity();
        let noise = id.static_public();
        let ed = id.signing_public();
        let signature = sign_announce(&id, &PEER_ID, "alice", TIMESTAMP);

        let mut other_peer = PEER_ID;
        other_peer[7] ^= 0x01;
        assert!(
            !verify_announce(&signature, &other_peer, &noise, &ed, "alice", TIMESTAMP, &ed),
            "peer ID must be bound"
        );

        let mut other_noise = noise;
        other_noise[0] ^= 0x01;
        assert!(
            !verify_announce(
                &signature,
                &PEER_ID,
                &other_noise,
                &ed,
                "alice",
                TIMESTAMP,
                &ed
            ),
            "noise key must be bound"
        );

        let mut other_ed = ed;
        other_ed[31] ^= 0x01;
        assert!(
            !verify_announce(
                &signature,
                &PEER_ID,
                &noise,
                &other_ed,
                "alice",
                TIMESTAMP,
                &ed
            ),
            "advertised ed25519 key must be bound"
        );

        assert!(
            !verify_announce(&signature, &PEER_ID, &noise, &ed, "alicf", TIMESTAMP, &ed),
            "nickname must be bound"
        );

        assert!(
            !verify_announce(&signature, &PEER_ID, &noise, &ed, "alice", TIMESTAMP + 1, &ed),
            "timestamp must be bound"
        );

        let mut forged = signature;
        forged[0] ^= 0x01;
        assert!(!verify_announce(
            &forged, &PEER_ID, &noise, &ed, "alice", TIMESTAMP, &ed
        ));

        let other = MeshIdentity::from_keys(STATIC_SECRET, [0x99; 32]);
        assert!(
            !verify_announce(
                &signature,
                &PEER_ID,
                &noise,
                &ed,
                "alice",
                TIMESTAMP,
                &other.signing_public()
            ),
            "another peer's key must not verify"
        );
    }

    #[test]
    fn announce_nickname_length_prefix_prevents_field_sliding() {
        // Without the length prefix, ("ab", ts) and ("a", "b" ‖ ts) could collide.
        // They must not.
        let a = canonical_announce_bytes(&PEER_ID, &[0; 32], &[0; 32], "ab", 1);
        let b = canonical_announce_bytes(&PEER_ID, &[0; 32], &[0; 32], "a", 1);
        assert_ne!(a, b);
        assert_ne!(a.len(), b.len());
    }

    // MARK: raw bytes

    #[test]
    fn raw_signature_round_trips_and_rejects_tampering() {
        let id = identity();
        let signature = sign_bytes(&id, b"attack at dawn");
        assert!(verify_bytes(&signature, b"attack at dawn", &id.signing_public()));
        assert!(!verify_bytes(&signature, b"attack at dusk", &id.signing_public()));
    }

    #[test]
    fn verification_never_panics_on_hostile_input() {
        let id = identity();
        let signature = sign_bytes(&id, b"data");

        // Not a valid Edwards point: every byte set.
        assert!(!verify_bytes(&signature, b"data", &[0xFF; 32]));
        // The all-zero encoding is a small-order point; `verify_strict` rejects it.
        assert!(!verify_bytes(&signature, b"data", &[0x00; 32]));
        // Garbage signature bytes.
        assert!(!verify_bytes(&[0xFF; 64], b"data", &id.signing_public()));
        assert!(!verify_bytes(&[0x00; 64], b"data", &id.signing_public()));
        // Empty message with a signature over something else.
        assert!(!verify_bytes(&signature, b"", &id.signing_public()));
    }

    #[test]
    fn empty_message_signs_and_verifies() {
        let id = identity();
        let signature = sign_bytes(&id, b"");
        assert!(verify_bytes(&signature, b"", &id.signing_public()));
    }

    // MARK: packets

    fn packet(ttl: u8) -> BitchatPacket {
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
    fn packet_signature_round_trips() {
        let id = identity();
        let mut p = packet(7);
        let signature = sign_packet(&id, &p).expect("preimage encodes");
        p.signature = Some(signature);
        assert!(verify_packet(&p, &id.signing_public()));
    }

    #[test]
    fn packet_signature_survives_a_ttl_decrement() {
        // The whole reason the TTL is outside the preimage: every relay
        // decrements it, and the originator's signature must still verify.
        let id = identity();
        let original = packet(7);
        let signature = sign_packet(&id, &original).expect("preimage encodes");

        let relayed = BitchatPacket {
            ttl: 3,
            signature: Some(signature),
            ..original.clone()
        };
        assert!(verify_packet(&relayed, &id.signing_public()));

        let exhausted = BitchatPacket {
            ttl: 0,
            signature: Some(signature),
            ..original
        };
        assert!(verify_packet(&exhausted, &id.signing_public()));
    }

    #[test]
    fn packet_signature_ignores_is_rsr_but_not_the_payload() {
        let id = identity();
        let original = packet(7);
        let signature = sign_packet(&id, &original).expect("preimage encodes");

        // `is_rsr` is a receiver-side annotation; it is excluded from the preimage.
        let flagged = BitchatPacket {
            is_rsr: true,
            signature: Some(signature),
            ..original.clone()
        };
        assert!(verify_packet(&flagged, &id.signing_public()));

        let tampered = BitchatPacket {
            payload: b"hello mesh!".to_vec(),
            signature: Some(signature),
            ..original.clone()
        };
        assert!(!verify_packet(&tampered, &id.signing_public()));

        let resender = BitchatPacket {
            sender_id: [0xAC; SENDER_ID_LEN],
            signature: Some(signature),
            ..original.clone()
        };
        assert!(!verify_packet(&resender, &id.signing_public()));

        let retimed = BitchatPacket {
            timestamp: TIMESTAMP + 1,
            signature: Some(signature),
            ..original
        };
        assert!(!verify_packet(&retimed, &id.signing_public()));
    }

    #[test]
    fn unsigned_packet_never_verifies() {
        let id = identity();
        let p = packet(7);
        assert!(p.signature.is_none());
        assert!(!verify_packet(&p, &id.signing_public()));
    }

    #[test]
    fn packet_signature_from_another_identity_fails() {
        let id = identity();
        let mallory = MeshIdentity::from_keys([0x55; 32], [0x66; 32]);
        let mut p = packet(7);
        p.signature = Some(sign_packet(&mallory, &p).expect("preimage encodes"));
        assert!(!verify_packet(&p, &id.signing_public()));
        assert!(verify_packet(&p, &mallory.signing_public()));
    }

    #[test]
    fn packet_preimage_is_the_padded_encoding() {
        // Guards the single most likely interop mistake: signing the unpadded
        // bytes verifies against ourselves and fails against every real peer.
        let p = packet(7);
        let preimage = p.signing_preimage().expect("preimage encodes");
        assert_eq!(
            preimage.len(),
            256,
            "a short packet pads up to the first block size"
        );
    }
}
