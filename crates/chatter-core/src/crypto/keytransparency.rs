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
//!   1. the proof's tree key equals `VRF_sk(user_id)` — the server's ECVRF proof
//!      (verified under the pinned VRF public key) binds the proof to the
//!      *user_id* we asked about (a server cannot answer with some other user's
//!      leaf, nor forge a VRF proof for a different index), while the log's leaf
//!      keys stay unlinkable to usernames for a third-party auditor (Phase 3b),
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

use std::sync::LazyLock;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::config::{KT_TREE_HEAD_PUBLIC_KEY_BYTES, KT_VRF_PUBLIC_KEY_BYTES};
use crate::crypto::vrf::vrf_verify;
use crate::proto::chat_v1::{GetConsistencyProofResponse, GetProofResponse, SignedTreeHead};

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
    /// `inclusion_proof.key` != `VRF_sk(user_id)`: the server answered with a
    /// proof for a *different* leaf than the user we asked about.
    WrongLeafKey,
    /// The query-privacy VRF proof is missing, malformed, or does not verify
    /// under the pinned VRF public key — the server did not prove the served leaf
    /// index really is `VRF_sk(user_id)`. REJECT: the leaf is unauthenticated.
    VrfInvalid,
    /// The key committed in the log differs from the identity key in the bundle
    /// — i.e. a forged/MITM identity key. THIS is the F9 attack being rejected.
    IdentityKeyMismatch,
    /// The Merkle inclusion proof does not recompute to the tree-head root.
    InclusionInvalid,
    /// The tree-head signature does not verify under the pinned public key.
    TreeHeadSignatureInvalid,
    /// A consistency proof's epochs are out of order, or don't match the signed
    /// tree heads.
    ConsistencyEpochOrder,
    /// A consistency proof's old/new root does not match the signed tree head it
    /// claims to be for.
    ConsistencyRootMismatch,
    /// The provided old leaves do not reconstruct to the signed old root — the
    /// server under-reported or altered the old leaf set (an attempt to hide a
    /// rewritten/omitted leaf). REJECT: the log may have been forked.
    ConsistencyOldRootUnreconstructable,
    /// An old leaf is NOT present unchanged in the new tree — the log was
    /// rewritten (not append-only). THIS is the fork being rejected.
    ConsistencyLeafNotPreserved,
}

impl std::fmt::Display for KtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KtError::Malformed(w) => write!(f, "KT proof malformed: {w}"),
            KtError::WrongLeafKey => {
                write!(f, "KT proof is for a different user's leaf (tree-key mismatch)")
            }
            KtError::VrfInvalid => write!(
                f,
                "KT query-privacy VRF proof is missing or invalid under the pinned VRF key"
            ),
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
            KtError::ConsistencyEpochOrder => {
                write!(f, "KT consistency proof epochs are out of order or mismatched")
            }
            KtError::ConsistencyRootMismatch => {
                write!(f, "KT consistency proof root does not match its signed tree head")
            }
            KtError::ConsistencyOldRootUnreconstructable => write!(
                f,
                "KT consistency proof old leaves do not reconstruct the signed old root (log may be forked)"
            ),
            KtError::ConsistencyLeafNotPreserved => write!(
                f,
                "KT consistency proof: an old leaf is not preserved in the new tree (log rewritten/forked)"
            ),
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

    // (1) The proof must be for THIS user's leaf. The leaf key is now the
    // query-privacy VRF index VRF_sk(user_id): verify the server's VRF proof under
    // the PINNED VRF public key and recover the index, then require the inclusion
    // proof's key to equal it. This binds the queried user_id to the served leaf
    // WITHOUT the client having to know (or the log leaking) a username->leaf map,
    // and a malicious server cannot answer with an arbitrary leaf — it cannot
    // forge a VRF proof for a different index under the pinned key.
    let expected_key = vrf_verify(&KT_VRF_PUBLIC_KEY_BYTES, user_id.as_bytes(), &response.vrf_proof)
        .ok_or(KtError::VrfInvalid)?;
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

/// Default hashes for empty subtrees at each depth, port of `crypto.defaultHashes`
/// (merkle.go): `dh[0]` = 32 zero bytes, `dh[d] = SHA-256(0x01 || dh[d-1] || dh[d-1])`.
/// Index runs 0..=256 (TreeDepth = 256).
static DEFAULT_HASHES: LazyLock<[[u8; 32]; 257]> = LazyLock::new(|| {
    let mut dh = [[0u8; 32]; 257];
    for d in 1..=256 {
        dh[d] = interior_hash(&dh[d - 1], &dh[d - 1]);
    }
    dh
});

/// Bit `pos` of `key`, port of `crypto.getBit`: `pos=255` is the MSB of byte 0.
fn get_bit(key: &[u8; 32], pos: usize) -> u8 {
    let byte_idx = (256 - 1 - pos) / 8;
    let bit_idx = 7 - ((256 - 1 - pos) % 8);
    (key[byte_idx] >> bit_idx) & 1
}

/// Recompute a sparse-Merkle subtree root from the leaves that fall inside it,
/// splitting on bit `depth`. Byte-exact port of the Go tree's root computation
/// (merkle.go insert/split + nodeHash/defaultHashes):
///   - empty subtree  -> DEFAULT_HASHES[depth + 1]
///   - single leaf    -> its leaf hash (a lone leaf collapses, never padded down)
///   - otherwise      -> H(0x01 || left_root || right_root), partitioning leaves
///     by their bit at `depth`.
///
/// `leaves` are (tree_key, leaf_hash) pairs. Used to independently reconstruct
/// the OLD root from a consistency proof's old leaves, so the server cannot
/// omit/alter an old leaf without the reconstructed root diverging from the
/// signed old root.
fn sparse_subtree_root(leaves: &[([u8; 32], [u8; 32])], depth: usize) -> [u8; 32] {
    match leaves.len() {
        0 => DEFAULT_HASHES[depth + 1],
        1 => leaves[0].1,
        _ => {
            let (mut left, mut right): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
            for &lf in leaves {
                if get_bit(&lf.0, depth) == 0 {
                    left.push(lf);
                } else {
                    right.push(lf);
                }
            }
            // depth == 0 with >1 leaf would mean two identical keys; the Go tree
            // cannot represent that (keys are unique), so recursion always makes
            // progress before underflowing.
            let lh = sparse_subtree_root(&left, depth.wrapping_sub(1));
            let rh = sparse_subtree_root(&right, depth.wrapping_sub(1));
            interior_hash(&lh, &rh)
        }
    }
}

/// Reconstruct the whole-tree root from a complete set of (key, value) leaves.
/// Port of `SparseMerkleTree.Root()` for the leaves collected via `Leaves()`.
fn sparse_root(kv_leaves: &[(&[u8; 32], &[u8])]) -> [u8; 32] {
    if kv_leaves.is_empty() {
        return DEFAULT_HASHES[256];
    }
    let leaves: Vec<([u8; 32], [u8; 32])> =
        kv_leaves.iter().map(|(k, v)| (**k, leaf_hash(k, v))).collect();
    sparse_subtree_root(&leaves, 255)
}

/// Verify a `SignedTreeHead` proto message under the pinned key. Returns the
/// 32-byte root on success.
fn verify_sth(sth: &SignedTreeHead) -> Result<[u8; 32], KtError> {
    let root: [u8; 32] = sth
        .root_hash
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("signed_tree_head.root_hash is not 32 bytes"))?;
    if !verify_tree_head(sth.epoch, &root, sth.tree_size, &sth.signature) {
        return Err(KtError::TreeHeadSignatureInvalid);
    }
    Ok(root)
}

/// Verify a consistency proof between two epochs. On `Ok(())` the client has
/// cryptographic evidence that the KT log is APPEND-ONLY between `old_epoch` and
/// `new_epoch`: every leaf committed in the old tree still exists, unchanged, in
/// the new tree, under two tree heads both signed by the pinned key. Any `Err`
/// means the log may have been FORKED/rewritten and the client must alarm.
///
/// Checks (all must hold):
///   1. both tree-head signatures verify under the pinned key,
///   2. epochs are ordered and match the signed heads (old < new),
///   3. `proof.old_root` / `proof.new_root` equal the signed roots,
///   4. the old leaves reconstruct EXACTLY to the signed old root (completeness —
///      the server cannot omit or alter an old leaf),
///   5. every old leaf has a valid inclusion proof against the new root
///      (persistence — port of `crypto.VerifyConsistency`).
pub fn verify_consistency_proof(response: &GetConsistencyProofResponse) -> Result<(), KtError> {
    let proof = response
        .proof
        .as_ref()
        .ok_or(KtError::Malformed("missing consistency proof"))?;
    let old_sth = response
        .old_signed_tree_head
        .as_ref()
        .ok_or(KtError::Malformed("missing old_signed_tree_head"))?;
    let new_sth = response
        .new_signed_tree_head
        .as_ref()
        .ok_or(KtError::Malformed("missing new_signed_tree_head"))?;

    // (1) both heads signed by the pinned key.
    let old_root = verify_sth(old_sth)?;
    let new_root = verify_sth(new_sth)?;

    // (2) epoch ordering + binding to the signed heads.
    if proof.old_epoch >= proof.new_epoch
        || proof.old_epoch != old_sth.epoch
        || proof.new_epoch != new_sth.epoch
    {
        return Err(KtError::ConsistencyEpochOrder);
    }

    // (3) proof roots must equal the signed roots.
    let proof_old_root: [u8; 32] = proof
        .old_root
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("consistency old_root is not 32 bytes"))?;
    let proof_new_root: [u8; 32] = proof
        .new_root
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("consistency new_root is not 32 bytes"))?;
    if proof_old_root != old_root || proof_new_root != new_root {
        return Err(KtError::ConsistencyRootMismatch);
    }

    // (4) COMPLETENESS: reconstruct the old root from the provided old leaves.
    // Every old_leaf_proof carries the old (key, value); rebuilding the sparse
    // tree from them must yield exactly the signed old root, so the server cannot
    // silently drop or rewrite an old leaf and still pass (5).
    let kv: Vec<(&[u8; 32], &[u8])> = {
        let mut v = Vec::with_capacity(proof.old_leaf_proofs.len());
        for lp in &proof.old_leaf_proofs {
            let key: &[u8; 32] = lp
                .key
                .as_slice()
                .try_into()
                .map_err(|_| KtError::Malformed("consistency old leaf key is not 32 bytes"))?;
            v.push((key, lp.value.as_slice()));
        }
        v
    };
    if sparse_root(&kv) != old_root {
        return Err(KtError::ConsistencyOldRootUnreconstructable);
    }

    // (5) PERSISTENCE: each old leaf must be included in the new tree with the
    // same value (port of crypto.VerifyConsistency).
    for lp in &proof.old_leaf_proofs {
        let key: &[u8; 32] = lp
            .key
            .as_slice()
            .try_into()
            .map_err(|_| KtError::Malformed("consistency old leaf key is not 32 bytes"))?;
        let recomputed = recompute_root(key, &lp.value, &lp.sibling_hashes, &lp.bits)
            .ok_or(KtError::ConsistencyLeafNotPreserved)?;
        if recomputed != new_root {
            return Err(KtError::ConsistencyLeafNotPreserved);
        }
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
