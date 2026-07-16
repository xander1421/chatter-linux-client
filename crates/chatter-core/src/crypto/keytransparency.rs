//! Key Transparency (KT) client-side verification — Phase 2.
//!
//! The server maintains an append-only sparse-Merkle log of user identity keys
//! (see `internal/services/keytransparency` on the server). `GetProof(user_id)`
//! returns the identity key the server has committed for a user together with a
//! Merkle inclusion proof and an Ed25519-signed tree head.
//!
//! Before the client trusts a peer's `bundle.identity_key()` (TOFU), it verifies
//! that the key is the one committed in the log:
//!
//!   1. the proof's tree key equals `H("ChatterKT-leaf-v1" || user_id)` — binds
//!      the proof to the *user_id* we asked about (a server cannot answer with
//!      some other user's leaf),
//!   2. the proof's committed value equals the identity key from the bundle —
//!      binds the log to the *key we are about to trust*,
//!   3. the Merkle inclusion proof recomputes to the tree head's `root_hash`,
//!   4. the tree head signature verifies under the pinned Ed25519 public key
//!      (`config::KT_TREE_HEAD_PUBLIC_KEY_BYTES`).
//!
//! Every byte format below is a direct port of the Go `crypto` package
//! (`internal/services/keytransparency/crypto/{merkle,treehead}.go`) — the leaf
//! and interior domain-separation tags (`0x00` / `0x01`), the tree-head domain
//! separator (`"ChatterKT-v1"`), and the little-endian epoch/tree-size encoding.
//! A mismatch in any of these makes the verifier reject a genuine proof, so
//! interop is proven live against a real server in the V14 validated test.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::config::KT_TREE_HEAD_PUBLIC_KEY_BYTES;
use crate::proto::chat_v1::GetProofResponse;

/// Domain separator for deriving a user's sparse-Merkle tree key.
/// Mirrors `keytransparency.leafKeyDomain` (log.go).
const LEAF_KEY_DOMAIN: &[u8] = b"ChatterKT-leaf-v1";

/// Leaf-node hash prefix. Mirrors `crypto.leafPrefix` (merkle.go).
const LEAF_PREFIX: u8 = 0x00;
/// Interior-node hash prefix. Mirrors `crypto.interiorPrefix` (merkle.go).
const INTERIOR_PREFIX: u8 = 0x01;
/// Tree-head signature domain separator. Mirrors `crypto.treeHeadDomainSeparator`.
const TREE_HEAD_DOMAIN: &[u8] = b"ChatterKT-v1";

/// Why a KT proof was rejected. Any variant means: do NOT trust the peer key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KtError {
    /// The proof or tree head was missing/structurally malformed.
    Malformed(&'static str),
    /// `inclusion_proof.key` != `H(domain || user_id)`: the server answered with
    /// a proof for a *different* leaf than the user we asked about.
    WrongLeafKey,
    /// The key committed in the log differs from the identity key in the bundle
    /// — i.e. a forged/MITM identity key. THIS is the F9 attack being rejected.
    IdentityKeyMismatch,
    /// The Merkle inclusion proof does not recompute to the tree-head root.
    InclusionInvalid,
    /// The tree-head signature does not verify under the pinned public key.
    TreeHeadSignatureInvalid,
}

impl std::fmt::Display for KtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KtError::Malformed(w) => write!(f, "KT proof malformed: {w}"),
            KtError::WrongLeafKey => {
                write!(f, "KT proof is for a different user's leaf (tree-key mismatch)")
            }
            KtError::IdentityKeyMismatch => write!(
                f,
                "KT-committed identity key does not match the bundle's identity key (forged key)"
            ),
            KtError::InclusionInvalid => {
                write!(f, "KT Merkle inclusion proof does not verify against the tree head")
            }
            KtError::TreeHeadSignatureInvalid => {
                write!(f, "KT signed tree head signature is invalid under the pinned key")
            }
        }
    }
}

impl std::error::Error for KtError {}

/// Deterministically map a `user_id` to its 32-byte sparse-Merkle tree key.
/// Port of `keytransparency.LeafKey`: `SHA-256(domain || user_id)`.
pub fn leaf_key(user_id: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(LEAF_KEY_DOMAIN);
    h.update(user_id.as_bytes());
    h.finalize().into()
}

/// Recompute the leaf hash `SHA-256(0x00 || key || value)`.
/// Port of `crypto.computeLeafHash`.
fn leaf_hash(key: &[u8; 32], value: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([LEAF_PREFIX]);
    h.update(key);
    h.update(value);
    h.finalize().into()
}

/// One interior-node hash step `SHA-256(0x01 || left || right)`.
fn interior_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([INTERIOR_PREFIX]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Verify a Merkle inclusion proof, returning the recomputed root.
/// Port of `crypto.VerifyInclusion` (merkle.go): walk from the leaf toward the
/// root, folding each sibling in on the side indicated by `bits`.
///
/// Returns `None` if the proof is structurally invalid (length mismatch, bad
/// sibling size), otherwise `Some(root)` — the caller compares it to the tree
/// head's `root_hash`.
fn recompute_root(
    key: &[u8; 32],
    value: &[u8],
    sibling_hashes: &[Vec<u8>],
    bits: &[u32],
) -> Option<[u8; 32]> {
    if sibling_hashes.len() != bits.len() {
        return None;
    }
    let mut current = leaf_hash(key, value);
    // Leaf -> root: Go iterates i from len-1 down to 0.
    for i in (0..sibling_hashes.len()).rev() {
        let sib: &[u8; 32] = sibling_hashes[i].as_slice().try_into().ok()?;
        current = if bits[i] == 0 {
            interior_hash(&current, sib)
        } else {
            interior_hash(sib, &current)
        };
    }
    Some(current)
}

/// Verify the Ed25519 tree-head signature under the pinned public key.
/// Port of `crypto.VerifyTreeHead` + `treeHeadPayload`: the signed message is
/// `SHA-256("ChatterKT-v1" || epoch_le64 || root_hash || tree_size_le64)`, and
/// the signature is plain RFC-8032 Ed25519 over that 32-byte digest.
fn verify_tree_head(
    epoch: u64,
    root_hash: &[u8; 32],
    tree_size: u64,
    signature: &[u8],
) -> bool {
    let sig_bytes: [u8; 64] = match signature.try_into() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let vk = match VerifyingKey::from_bytes(&KT_TREE_HEAD_PUBLIC_KEY_BYTES) {
        Ok(vk) => vk,
        Err(_) => return false,
    };

    let mut payload = Vec::with_capacity(TREE_HEAD_DOMAIN.len() + 8 + 32 + 8);
    payload.extend_from_slice(TREE_HEAD_DOMAIN);
    payload.extend_from_slice(&epoch.to_le_bytes());
    payload.extend_from_slice(root_hash);
    payload.extend_from_slice(&tree_size.to_le_bytes());
    let digest: [u8; 32] = Sha256::digest(&payload).into();

    vk.verify(&digest, &Signature::from_bytes(&sig_bytes)).is_ok()
}

/// Full client-side verification of a `GetProof` response.
///
/// On `Ok(())` the caller may trust `bundle_identity_key` as `user_id`'s key: it
/// is the value committed in a validly-signed KT log. Any `Err` means the key is
/// NOT KT-attested and the session MUST be refused (never established).
///
/// `bundle_identity_key` is the 33-byte libsignal serialization
/// (`IdentityKey::serialize()`, 0x05 prefix + 32 bytes) — byte-identical to what
/// the server stored at registration, so the equality check is exact.
pub fn verify_proof(
    user_id: &str,
    bundle_identity_key: &[u8],
    response: &GetProofResponse,
) -> Result<(), KtError> {
    let proof = response
        .inclusion_proof
        .as_ref()
        .ok_or(KtError::Malformed("missing inclusion_proof"))?;
    let sth = response
        .signed_tree_head
        .as_ref()
        .ok_or(KtError::Malformed("missing signed_tree_head"))?;

    // (1) The proof must be for THIS user's leaf.
    let expected_key = leaf_key(user_id);
    let proof_key: &[u8; 32] = proof
        .key
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("inclusion_proof.key is not 32 bytes"))?;
    if proof_key != &expected_key {
        return Err(KtError::WrongLeafKey);
    }

    // (2) The committed value must equal the identity key we are about to trust.
    // (`response.identity_key` is documented to equal `proof.value`; we compare
    // against `proof.value` — the byte string the Merkle root actually commits
    // to — so a server cannot desync the two fields.)
    if proof.value.as_slice() != bundle_identity_key {
        return Err(KtError::IdentityKeyMismatch);
    }

    // (3) The inclusion proof must recompute to the tree-head root.
    let root: &[u8; 32] = sth
        .root_hash
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("signed_tree_head.root_hash is not 32 bytes"))?;
    let recomputed = recompute_root(proof_key, &proof.value, &proof.sibling_hashes, &proof.bits)
        .ok_or(KtError::InclusionInvalid)?;
    if &recomputed != root {
        return Err(KtError::InclusionInvalid);
    }

    // (4) The tree head must be signed by the pinned KT key.
    if !verify_tree_head(sth.epoch, root, sth.tree_size, &sth.signature) {
        return Err(KtError::TreeHeadSignatureInvalid);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-check the leaf-key derivation against a value the Go side would
    // produce for a fixed user_id. Computed with the same
    // SHA-256("ChatterKT-leaf-v1" || user_id) construction.
    #[test]
    fn leaf_key_is_domain_separated_sha256() {
        let uid = "11111111-1111-1111-1111-111111111111";
        let mut h = Sha256::new();
        h.update(b"ChatterKT-leaf-v1");
        h.update(uid.as_bytes());
        let expected: [u8; 32] = h.finalize().into();
        assert_eq!(leaf_key(uid), expected);
    }

    // A single-leaf tree: root == leaf_hash, no siblings. Mirrors the Go tree
    // when exactly one user is in the log.
    #[test]
    fn single_leaf_inclusion_recomputes_to_leaf_hash() {
        let key = leaf_key("00000000-0000-0000-0000-000000000000");
        let value = vec![0x05u8; 33];
        let root = recompute_root(&key, &value, &[], &[]).unwrap();
        assert_eq!(root, leaf_hash(&key, &value));
    }

    // Two-leaf tree, hand-built exactly as Go's split would produce it, to
    // exercise both a left- and right-fold and the bit direction.
    #[test]
    fn two_leaf_inclusion_folds_sibling_correctly() {
        let key = [0xAAu8; 32];
        let value = vec![1u8, 2, 3];
        let sibling = [0xBBu8; 32];
        // bit=1 => leaf is the right child: root = H(0x01 || sibling || leaf)
        let leaf = leaf_hash(&key, &value);
        let expected = interior_hash(&sibling, &leaf);
        let got = recompute_root(&key, &value, &[sibling.to_vec()], &[1]).unwrap();
        assert_eq!(got, expected);
        // Flipping the bit must change the root (direction matters).
        let flipped = recompute_root(&key, &value, &[sibling.to_vec()], &[0]).unwrap();
        assert_ne!(flipped, expected);
    }

    #[test]
    fn recompute_root_rejects_length_mismatch() {
        let key = [0u8; 32];
        assert!(recompute_root(&key, b"v", &[vec![0u8; 32]], &[]).is_none());
    }

    #[test]
    fn pinned_key_parses() {
        assert!(VerifyingKey::from_bytes(&KT_TREE_HEAD_PUBLIC_KEY_BYTES).is_ok());
    }
}
