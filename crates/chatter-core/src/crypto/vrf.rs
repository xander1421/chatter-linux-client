//! ECVRF-EDWARDS25519-SHA512-TAI verifier — byte-exact port of the Go server's
//! VRF (`internal/services/keytransparency/crypto/vrf.go`, which wraps
//! ProtonMail's `go-ecvrf`, itself implementing draft-irtf-cfrg-vrf-10 with the
//! try-and-increment hash-to-curve). Suite string byte is `0x03`.
//!
//! Only VERIFY is ported (the client never proves). Given the server's VRF
//! PUBLIC key, an input `alpha` (a peer's `user_id`), and an 80-byte proof
//! `gamma(32) || c(16) || s(32)`, [`vrf_verify`] returns the 32-byte VRF index
//! (the first half of the 64-byte VRF output) iff the proof is valid, else
//! `None`. The index is the sparse-Merkle tree leaf key the KT log commits under
//! — so verifying the VRF proof binds the queried `user_id` to the exact leaf a
//! KT inclusion proof is served for, WITHOUT the plaintext `user_id` being the
//! linkable lookup key an auditor of the log can see.
//!
//! Every byte format below mirrors `ecvrf.go`:
//!   * `hash_to_curve_tai`: for ctr in 0..=255, SHA-512(0x03 || 0x01 || pk ||
//!     alpha || ctr || 0x00), take the first 32 bytes, decompress as an Edwards
//!     point; the first non-identity decode, multiplied by the cofactor (8), is H.
//!   * `hash_points`: SHA-512(0x03 || 0x02 || P1 || P2 || ... || 0x00)[..16].
//!   * challenge `c`: the 16 proof bytes zero-extended to a 32-byte little-endian
//!     scalar.
//!   * verify: U = s·B − c·Y, V = s·H − c·Gamma, accept iff
//!     hash_points(H, Gamma, U, V) == proof[32..48].
//!   * output (beta): SHA-512(0x03 || 0x03 || (8·Gamma) || 0x00); index = beta[..32].
//!
//! `curve25519-dalek`'s point compression / `CompressedEdwardsY::decompress`
//! matches `filippo.io/edwards25519`'s `Bytes`/`SetBytes` (both accept the same
//! non-canonical-but-valid encodings via a SqrtRatio decode), and its
//! `mul_by_cofactor` is ×8 — so the curve arithmetic is byte-identical to the Go
//! prover. Interop is proven against a LIVE server proof in the V37 validated
//! test, and against fixed Go-generated vectors in the unit tests below.

use curve25519_dalek::{
    edwards::CompressedEdwardsY, edwards::EdwardsPoint, scalar::Scalar, traits::Identity,
};
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;

/// ECVRF-EDWARDS25519-SHA512-TAI suite identifier octet.
const SUITE: u8 = 0x03;
/// Proof layout: gamma(32) || c(16) || s(32).
const PROOF_SIZE: usize = 80;
const GAMMA_LEN: usize = 32;
const C_LEN: usize = 16;

/// Decode a 32-byte compressed Edwards point, matching `filippo`'s `SetBytes`.
fn decompress(bytes: &[u8]) -> Option<EdwardsPoint> {
    let arr: [u8; 32] = bytes.try_into().ok()?;
    CompressedEdwardsY(arr).decompress()
}

/// `ECVRF_hash_to_curve_try_and_increment` (draft-10 §5.4.1.1) for suite 0x03.
/// Port of `hashToCurveTAI` in ecvrf.go.
fn hash_to_curve_tai(pk: &[u8; 32], alpha: &[u8]) -> Option<EdwardsPoint> {
    for ctr in 0u16..256 {
        let mut h = Sha512::new();
        h.update([SUITE, 0x01]);
        h.update(pk);
        h.update(alpha);
        h.update([ctr as u8, 0x00]);
        let digest = h.finalize();
        if let Some(p) = decompress(&digest[..32]) {
            if p != EdwardsPoint::identity() {
                return Some(p.mul_by_cofactor());
            }
        }
    }
    None
}

/// `ECVRF_hash_points` (draft-10 §5.4.3): SHA-512(0x03 || 0x02 || P.. || 0x00),
/// truncated to 16 bytes. Port of `hashPoints`.
fn hash_points(points: &[&EdwardsPoint]) -> [u8; C_LEN] {
    let mut h = Sha512::new();
    h.update([SUITE, 0x02]);
    for p in points {
        h.update(p.compress().to_bytes());
    }
    h.update([0x00]);
    let digest = h.finalize();
    let mut c = [0u8; C_LEN];
    c.copy_from_slice(&digest[..C_LEN]);
    c
}

/// Zero-extend the 16-byte challenge to a canonical 32-byte little-endian scalar.
/// Port of `cToScalar`. Always < L (value < 2^128), so the decode never fails.
fn c_to_scalar(c: &[u8; C_LEN]) -> Scalar {
    let mut raw = [0u8; 32];
    raw[..C_LEN].copy_from_slice(c);
    // Safe: 128-bit value is always a canonical scalar.
    Option::<Scalar>::from(Scalar::from_canonical_bytes(raw)).expect("16-byte c is always canonical")
}

/// `ECVRF_proof_to_hash` (draft-10 §5.2): the 64-byte VRF output beta.
/// Port of `proofToHash`.
fn proof_to_hash(gamma: &EdwardsPoint) -> [u8; 64] {
    let gamma_c = gamma.mul_by_cofactor();
    let mut h = Sha512::new();
    h.update([SUITE, 0x03]);
    h.update(gamma_c.compress().to_bytes());
    h.update([0x00]);
    h.finalize().into()
}

/// Verify an ECVRF proof. Returns the 32-byte VRF index (first half of the VRF
/// output) iff the proof is valid under `pk` for input `alpha`, else `None`.
///
/// Byte-exact port of `(*PublicKey).Verify` in ecvrf.go (draft-10 §5.3).
pub fn vrf_verify(pk: &[u8; 32], alpha: &[u8], proof: &[u8]) -> Option<[u8; 32]> {
    if proof.len() != PROOF_SIZE {
        return None;
    }

    let y = decompress(pk)?;
    let gamma = decompress(&proof[..GAMMA_LEN])?;
    let mut c_bytes = [0u8; C_LEN];
    c_bytes.copy_from_slice(&proof[GAMMA_LEN..GAMMA_LEN + C_LEN]);
    let c = c_to_scalar(&c_bytes);
    let s_bytes: [u8; 32] = proof[GAMMA_LEN + C_LEN..].try_into().ok()?;
    // s must be a canonical scalar (< L), matching filippo's SetCanonicalBytes.
    let s: Scalar = Option::from(Scalar::from_canonical_bytes(s_bytes))?;

    let h = hash_to_curve_tai(pk, alpha)?;

    // U = s·B − c·Y
    let u = EdwardsPoint::mul_base(&s) - (&c * &y);
    // V = s·H − c·Gamma
    let v = (&s * &h) - (&c * &gamma);

    // Accept iff the recomputed challenge equals the proof's challenge octets.
    let c_prime = hash_points(&[&h, &gamma, &u, &v]);
    if c_prime.ct_eq(&c_bytes).unwrap_u8() != 1 {
        return None;
    }

    let beta = proof_to_hash(&gamma);
    let mut index = [0u8; 32];
    index.copy_from_slice(&beta[..32]);
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed vectors generated by the Go server's crypto.VRFProve under the
    // well-known dev VRF seed (SHA-256("chatter-dev-kt-vrf-v1")), for
    // alpha = ASCII("11111111-1111-1111-1111-111111111111"). Proving byte-exact
    // Go<->Rust interop OFFLINE (no server needed): if any byte format diverged
    // (suite tag, hash-to-curve, challenge encoding, cofactor), verify would
    // reject this known-good proof.
    const PK: &str = "48af975feaba33ea110d4ab0c8f7a38c5c7122db185ff9499b81e5e74da6ec47";
    const ALPHA: &str = "31313131313131312d313131312d313131312d313131312d313131313131313131313131";
    const PROOF: &str = "fe86b3cbf108bb19e19a776fdd8b48dfd4d8cd2507aa747809598bd61506321822726b480154206185629fc947f86d117b1659f8da1a8f544f795c485f3b750aae9ae1b1c1b5bc641ed8e31f12b07c0d";
    const OUTPUT: &str = "cfc93a22b1b48eb9f4d51092865f60290d27ce1b80b63991abdc50cfdd2898672f93bbeb62d1f5c9a9fa896dc1c4713293890c7208e494fc8335d145116c9487";
    const INDEX: &str = "cfc93a22b1b48eb9f4d51092865f60290d27ce1b80b63991abdc50cfdd289867";

    fn pk() -> [u8; 32] {
        hex::decode(PK).unwrap().try_into().unwrap()
    }

    #[test]
    fn verifies_go_generated_proof_and_recovers_index() {
        let alpha = hex::decode(ALPHA).unwrap();
        let proof = hex::decode(PROOF).unwrap();
        let idx = vrf_verify(&pk(), &alpha, &proof).expect("Go-generated proof must verify");
        assert_eq!(hex::encode(idx), INDEX, "recovered index must match Go's");
        // Index is the first 32 bytes of the full 64-byte VRF output.
        assert_eq!(&hex::decode(OUTPUT).unwrap()[..32], idx.as_slice());
    }

    #[test]
    fn rejects_tampered_proof_gamma() {
        let alpha = hex::decode(ALPHA).unwrap();
        let mut proof = hex::decode(PROOF).unwrap();
        proof[0] ^= 0x01; // corrupt gamma
        assert!(vrf_verify(&pk(), &alpha, &proof).is_none());
    }

    #[test]
    fn rejects_tampered_proof_challenge() {
        let alpha = hex::decode(ALPHA).unwrap();
        let mut proof = hex::decode(PROOF).unwrap();
        proof[40] ^= 0x80; // corrupt c
        assert!(vrf_verify(&pk(), &alpha, &proof).is_none());
    }

    #[test]
    fn rejects_tampered_proof_scalar() {
        let alpha = hex::decode(ALPHA).unwrap();
        let mut proof = hex::decode(PROOF).unwrap();
        proof[70] ^= 0x01; // corrupt s
        assert!(vrf_verify(&pk(), &alpha, &proof).is_none());
    }

    #[test]
    fn rejects_wrong_alpha() {
        let mut alpha = hex::decode(ALPHA).unwrap();
        alpha[0] ^= 0x01;
        let proof = hex::decode(PROOF).unwrap();
        assert!(vrf_verify(&pk(), &alpha, &proof).is_none());
    }

    #[test]
    fn rejects_wrong_public_key() {
        let alpha = hex::decode(ALPHA).unwrap();
        let proof = hex::decode(PROOF).unwrap();
        let mut bad = pk();
        bad[0] ^= 0x01;
        // Wrong pk either fails to decompress or yields a different challenge.
        assert!(vrf_verify(&bad, &alpha, &proof).is_none());
    }

    #[test]
    fn rejects_bad_length() {
        assert!(vrf_verify(&pk(), b"x", &[0u8; 79]).is_none());
        assert!(vrf_verify(&pk(), b"x", &[0u8; 81]).is_none());
    }
}
