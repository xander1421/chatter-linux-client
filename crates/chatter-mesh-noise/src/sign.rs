//! Ed25519 signatures over mesh packets and raw bytes.
//!
//! Ported from `signPacket`, `verifyPacketSignature`, `signData`, and
//! `verifySignature` in `Services/NoiseEncryptionService.swift`.
//!
//! **Verification returns `bool`, never `Result`.** Every input to a `verify_*`
//! function arrived over a radio from someone we have not authenticated yet. A
//! malformed signature, a public key that is not a valid Edwards point, and an
//! honest mismatch are all the same answer: `false`. Nothing here panics on
//! attacker-supplied bytes.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use chatter_mesh::BitchatPacket;

use crate::error::NoiseError;
use crate::identity::MeshIdentity;


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


#[cfg(test)]
mod tests {
    use super::*;

    use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};

    const STATIC_SECRET: [u8; 32] = [0x11; 32];
    const SIGNING_SECRET: [u8; 32] = [0x22; 32];
    const TIMESTAMP: u64 = 1_700_000_000_123;

    fn identity() -> MeshIdentity {
        MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET)
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
