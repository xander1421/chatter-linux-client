//! Key Transparency (KT) client-side verification — Phase 2.
//!
//! The server maintains an append-only log of user identity keys
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
//!   3. the Merkle inclusion proof recomputes to the served `prefix_root` — the
//!      root of the sparse tree holding each user's CURRENT key,
//!   4. that `prefix_root` is sealed inside the newest leaf of the append-only
//!      LOG tree, proven by an RFC 6962 audit path against the tree head's
//!      `root_hash` (which is the LOG root),
//!   5. the tree head signature verifies under the pinned Ed25519 public key
//!      (`config::KT_TREE_HEAD_PUBLIC_KEY_BYTES`).
//!
//! Step 4 is what makes the current-key map auditable over time. The server
//! keeps two structures, as Signal's key transparency does: an append-only LOG
//! tree over the sequence of updates (consistency proofs live there — an
//! identity-key rotation is just another append) and a PREFIX tree of current
//! values (inclusion proofs live there). Sealing the prefix root into the log
//! leaf ties them together.
//!
//! Every byte format below is a direct port of the Go `crypto` package
//! (`internal/services/keytransparency/crypto/{merkle,treehead}.go`) — the leaf
//! and interior domain-separation tags (`0x00` / `0x01`), the tree-head domain
//! separator (`"ChatterKT-v1"`), and the little-endian epoch/tree-size encoding.
//! A mismatch in any of these makes the verifier reject a genuine proof, so
//! interop is proven live against a real server in the V14 validated test.

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
/// v2: the signed root is the LOG tree's root, not the current-key map's.
const TREE_HEAD_DOMAIN: &[u8] = b"ChatterKT-v2";

/// Log-tree leaf prefix. Mirrors `crypto.logLeafPrefix` (logtree.go).
const LOG_LEAF_PREFIX: u8 = 0x02;
/// Log-tree interior prefix. Mirrors `crypto.logInteriorPrefix` (logtree.go).
const LOG_INTERIOR_PREFIX: u8 = 0x03;

/// Why a KT proof was rejected. Any variant means: do NOT trust the peer key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KtError {
    /// The proof or tree head was missing/structurally malformed.
    Malformed(&'static str),
    /// The local tree-head pin could not be read or decoded. Its absence cannot
    /// be assumed, so no offered head may be accepted.
    PinUnreadable,
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
    /// The RFC 6962 consistency path does not reconstruct both signed roots —
    /// the log of `old_epoch` entries is NOT a prefix of the log of `new_epoch`
    /// entries. History was rewritten, reordered or truncated. THIS is the fork
    /// being rejected.
    ConsistencyLogNotAppendOnly,
    /// The identity key's inclusion proof verifies against a current-key-map
    /// root that is NOT the one the log committed at this epoch, so the map the
    /// server served is unattested. REJECT.
    PrefixRootNotInLog,
    /// The consistency proof could not be obtained at all.
    ///
    /// Treated as a REJECT, not a shrug. The server can rebuild state at any
    /// epoch by replaying its log (`Service.replay`, keytransparency/service.go),
    /// so it
    /// has no honest reason to fail — and a server that could suppress this
    /// check by erroring could fork the log undetectably, which is the same
    /// reasoning that already makes a transport error on `GetProof` a reject
    /// rather than a "no leaf".
    ConsistencyUnavailable,
}

impl KtError {
    /// Constant metadata only: no peer, proof, pin, or error payload is logged.
    pub(crate) fn trace_rejection(&self) {
        match self {
            Self::Malformed(_) => tracing::warn!(target: "kt_reject::malformed", "key transparency rejected"),
            Self::PinUnreadable => tracing::warn!(target: "kt_reject::pin_unreadable", "key transparency rejected"),
            Self::WrongLeafKey => tracing::warn!(target: "kt_reject::wrong_leaf", "key transparency rejected"),
            Self::VrfInvalid => tracing::warn!(target: "kt_reject::vrf_invalid", "key transparency rejected"),
            Self::IdentityKeyMismatch => tracing::warn!(target: "kt_reject::value_mismatch", "key transparency rejected"),
            Self::InclusionInvalid => tracing::warn!(target: "kt_reject::inclusion_invalid", "key transparency rejected"),
            Self::TreeHeadSignatureInvalid => tracing::warn!(target: "kt_reject::tree_head_signature", "key transparency rejected"),
            Self::ConsistencyEpochOrder => tracing::warn!(target: "kt_reject::consistency_epoch_order", "key transparency rejected"),
            Self::ConsistencyRootMismatch => tracing::warn!(target: "kt_reject::consistency_root_mismatch", "key transparency rejected"),
            Self::ConsistencyLogNotAppendOnly => tracing::warn!(target: "kt_reject::not_append_only", "key transparency rejected"),
            Self::PrefixRootNotInLog => tracing::warn!(target: "kt_reject::prefix_not_in_log", "key transparency rejected"),
            Self::ConsistencyUnavailable => tracing::warn!(target: "kt_reject::consistency_unavailable", "key transparency rejected"),
        }
    }
}

impl std::fmt::Display for KtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KtError::Malformed(w) => write!(f, "KT proof malformed: {w}"),
            KtError::PinUnreadable => write!(
                f,
                "KT pinned tree head could not be read or decoded; refusing to trust the offered head"
            ),
            KtError::WrongLeafKey => {
                write!(
                    f,
                    "KT proof is for a different user's leaf (tree-key mismatch)"
                )
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
                write!(
                    f,
                    "KT Merkle inclusion proof does not verify against the tree head"
                )
            }
            KtError::TreeHeadSignatureInvalid => {
                write!(
                    f,
                    "KT signed tree head signature is invalid under the pinned key"
                )
            }
            KtError::ConsistencyEpochOrder => {
                write!(
                    f,
                    "KT consistency proof epochs are out of order or mismatched"
                )
            }
            KtError::ConsistencyRootMismatch => {
                write!(
                    f,
                    "KT consistency proof root does not match its signed tree head"
                )
            }
            KtError::ConsistencyLogNotAppendOnly => write!(
                f,
                "KT consistency proof: the log at the old epoch is not a prefix of the log at the \
                 new epoch (history rewritten, reordered or truncated)"
            ),
            KtError::PrefixRootNotInLog => write!(
                f,
                "KT proof: the current-key tree root is not the one the append-only log committed \
                 at this epoch"
            ),
            KtError::ConsistencyUnavailable => write!(
                f,
                "KT consistency proof could not be obtained — refusing to advance the pinned \
                 tree head (a server that can suppress this check can fork the log undetected)"
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

/// Depth of the sparse prefix tree: 256-bit keys, one level per bit.
/// Mirrors `crypto.TreeDepth` (merkle.go).
const TREE_DEPTH: usize = 256;

/// The direction taken at `level` on the root-to-leaf path for `key`.
/// Port of `crypto.getBit`: level 255 is the most significant bit, used at the
/// root. This is the ONLY source of direction in the verifier — see
/// `recompute_root`.
fn path_bit(key: &[u8; 32], level: usize) -> u8 {
    let n = TREE_DEPTH - 1 - level; // 0 = most significant bit
    (key[n / 8] >> (7 - (n % 8))) & 1
}

/// Verify a Merkle inclusion proof, returning the recomputed root.
/// Port of `crypto.VerifyInclusion` (merkle.go).
///
/// CRYPTO-1: the side each sibling is folded in on is derived from `key` and
/// NOTHING ELSE. The path bits are deliberately not a parameter (the wire no
/// longer carries them): they are a function of the key, so a server that got to state them could
/// place a leaf at a position the key does not determine — and then commit two
/// leaves for one VRF index at two positions in one tree, serving different
/// identity keys to different clients under one validly signed root. Signal's
/// verifier does the same thing (`libsignal/rust/keytrans/src/prefix.rs`
/// `evaluate_proof`: "Read n^th bit of key", and a hard `proof.len() != 8 *
/// KEY_LENGTH` check).
///
/// The proof must span the full tree. A short proof used to need no hashing at
/// all: an empty one made this function return the leaf hash, so a server
/// publishing `prefix_root = leaf_hash(K, attacker_key)` passed.
///
/// Returns `None` if the proof is structurally invalid (wrong number of levels,
/// bad sibling size), otherwise `Some(root)` — the caller compares it to the
/// served `prefix_root`.
fn recompute_root(key: &[u8; 32], value: &[u8], sibling_hashes: &[Vec<u8>]) -> Option<[u8; 32]> {
    if sibling_hashes.len() != TREE_DEPTH {
        return None;
    }
    let mut current = leaf_hash(key, value);
    // Leaf -> root. `sibling_hashes` is indexed from the root level down, so
    // entry i belongs to level TREE_DEPTH-1-i.
    for i in (0..TREE_DEPTH).rev() {
        let sib: &[u8; 32] = sibling_hashes[i].as_slice().try_into().ok()?;
        current = if path_bit(key, TREE_DEPTH - 1 - i) == 0 {
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
fn verify_tree_head(epoch: u64, root_hash: &[u8; 32], tree_size: u64, signature: &[u8]) -> bool {
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

    vk.verify(&digest, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// One log-tree interior hash `SHA-256(0x03 || left || right)`.
/// Port of `crypto.logInterior` (logtree.go).
fn log_interior(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([LOG_INTERIOR_PREFIX]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The log leaf for one update: `SHA-256(0x02 || prefix_root || update_hash)`.
/// Port of `crypto.LogLeafHash` (logtree.go).
///
/// This is the binding between the two trees. The append-only log commits to the
/// current-key map's root at every epoch, so the succession of maps is itself
/// covered by the log's consistency proof — a server cannot later claim a
/// different current-key map for an epoch it already committed.
fn log_leaf_hash(prefix_root: &[u8; 32], update_hash: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([LOG_LEAF_PREFIX]);
    h.update(prefix_root);
    h.update(update_hash);
    h.finalize().into()
}

/// Verify an RFC 6962 audit path: that `leaf` sits at `index` in a log of `size`
/// leaves whose Merkle Tree Hash is `root`. Port of `crypto.VerifyLogInclusion`
/// (logtree.go), which follows RFC 6962-bis §2.1.3.2 verbatim.
fn verify_log_inclusion(
    leaf: &[u8; 32],
    index: u64,
    size: u64,
    path: &[[u8; 32]],
    root: &[u8; 32],
) -> bool {
    if index >= size {
        return false;
    }
    let (mut fnv, mut sn) = (index, size - 1);
    let mut r = *leaf;
    for p in path {
        if sn == 0 {
            return false;
        }
        if fnv & 1 == 1 || fnv == sn {
            r = log_interior(p, &r);
            while fnv & 1 == 0 && fnv != 0 {
                fnv >>= 1;
                sn >>= 1;
            }
        } else {
            r = log_interior(&r, p);
        }
        fnv >>= 1;
        sn >>= 1;
    }
    sn == 0 && r == *root
}

/// Verify an RFC 6962 consistency proof: that a log of `old_size` leaves with
/// Merkle Tree Hash `old_root` was EXTENDED — not rewritten — into a log of
/// `new_size` leaves with hash `new_root`. Port of `crypto.VerifyLogConsistency`
/// (logtree.go), which follows RFC 6962-bis §2.1.4.2 verbatim.
///
/// BOTH roots are reconstructed from the path, which is what makes every way of
/// breaking history detectable: change a committed value, drop an entry, or
/// reorder entries and the reconstructed old root stops matching the one the
/// caller supplied.
fn verify_log_consistency(
    old_size: u64,
    new_size: u64,
    old_root: &[u8; 32],
    new_root: &[u8; 32],
    path: &[[u8; 32]],
) -> bool {
    if old_size == 0 || old_size >= new_size {
        return false;
    }

    // When the old size is a power of two the old root IS a node of the new
    // tree, so the server omits it from the path.
    let mut p: Vec<[u8; 32]> = Vec::with_capacity(path.len() + 1);
    if old_size & (old_size - 1) == 0 {
        p.push(*old_root);
    }
    p.extend_from_slice(path);
    if p.is_empty() {
        return false;
    }

    let (mut fnv, mut sn) = (old_size - 1, new_size - 1);
    while fnv & 1 == 1 {
        fnv >>= 1;
        sn >>= 1;
    }
    let (mut fr, mut sr) = (p[0], p[0]);

    for c in &p[1..] {
        if sn == 0 {
            return false;
        }
        if fnv & 1 == 1 || fnv == sn {
            fr = log_interior(c, &fr);
            sr = log_interior(c, &sr);
            while fnv & 1 == 0 && fnv != 0 {
                fnv >>= 1;
                sn >>= 1;
            }
        } else {
            sr = log_interior(&sr, c);
        }
        fnv >>= 1;
        sn >>= 1;
    }

    sn == 0 && fr == *old_root && sr == *new_root
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
    let expected_key = vrf_verify(
        &KT_VRF_PUBLIC_KEY_BYTES,
        user_id.as_bytes(),
        &response.vrf_proof,
    )
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

    // (3) The inclusion proof must recompute to the CURRENT-KEY map's root.
    let prefix_root: &[u8; 32] = response
        .prefix_root
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("prefix_root is not 32 bytes"))?;
    let recomputed = recompute_root(proof_key, &proof.value, &proof.sibling_hashes)
        .ok_or(KtError::InclusionInvalid)?;
    if &recomputed != prefix_root {
        return Err(KtError::InclusionInvalid);
    }

    // (4) That map must be the one the APPEND-ONLY LOG committed at this epoch.
    // Without this the prefix root is just a number the server sent us, and a
    // server could serve any current-key map it liked for an epoch it had
    // already committed to.
    let log_root: &[u8; 32] = sth
        .root_hash
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("signed_tree_head.root_hash is not 32 bytes"))?;
    let update_hash: &[u8; 32] = response
        .log_update_hash
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("log_update_hash is not 32 bytes"))?;
    let log_path = hashes32(
        &response.log_inclusion_path,
        "log_inclusion_path entry is not 32 bytes",
    )?;
    if sth.tree_size == 0 {
        return Err(KtError::Malformed("signed tree head claims an empty log"));
    }
    // The newest log leaf is the one sealing the map we just verified against,
    // so its index is the last one.
    if !verify_log_inclusion(
        &log_leaf_hash(prefix_root, update_hash),
        sth.tree_size - 1,
        sth.tree_size,
        &log_path,
        log_root,
    ) {
        return Err(KtError::PrefixRootNotInLog);
    }

    // (5) The tree head must be signed by the pinned KT key.
    if !verify_tree_head(sth.epoch, log_root, sth.tree_size, &sth.signature) {
        return Err(KtError::TreeHeadSignatureInvalid);
    }

    Ok(())
}

/// Verify a `SignedTreeHead` proto message under the pinned key. Returns the
/// 32-byte LOG root on success.
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

/// Collect a repeated proto `bytes` field into 32-byte hashes.
fn hashes32(raw: &[Vec<u8>], what: &'static str) -> Result<Vec<[u8; 32]>, KtError> {
    raw.iter()
        .map(|h| {
            h.as_slice()
                .try_into()
                .map_err(|_| KtError::Malformed(what))
        })
        .collect()
}

/// Verify a consistency proof between two epochs. On `Ok(())` the client has
/// cryptographic evidence that the KT LOG is APPEND-ONLY between `old_epoch` and
/// `new_epoch`: the log of `old_epoch` entries is a PREFIX of the log of
/// `new_epoch` entries, under two tree heads both signed by the pinned key.
///
/// Checks (all must hold):
///   1. both tree-head signatures verify under the pinned key,
///   2. epochs are ordered and match the signed heads (old < new),
///   3. `proof.old_root` / `proof.new_root` equal the signed roots,
///   4. the RFC 6962 consistency path reconstructs BOTH roots.
///
/// What this does NOT do, deliberately: decide whether `old_root` is a head this
/// device ever trusted. A server can always produce an internally valid proof
/// between two epochs of a log it just rewrote — reconstructing the old root
/// from its own altered data. Catching that is
/// [`crate::services::signal::kt_anchor::accept_advance`]'s job: it compares
/// `old_root` against the PINNED head. Both halves are required; neither alone
/// detects a fork.
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
        tracing::warn!(target: "kt_proof::epoch_binding_mismatch", "KT continuity rejected");
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

    // (4) APPEND-ONLY: the path must reconstruct both roots. A value changed
    // retroactively, an entry dropped, or entries reordered all change the
    // reconstructed old root.
    let path = hashes32(&proof.path, "consistency path entry is not 32 bytes")?;
    if !verify_log_consistency(
        old_sth.tree_size,
        new_sth.tree_size,
        &old_root,
        &new_root,
        &path,
    ) {
        return Err(KtError::ConsistencyLogNotAppendOnly);
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

    // ── CRYPTO-1: the prefix-tree path must be bound to the key ─────────────
    //
    // `recompute_root` used to fold each sibling on the side the SERVER's `bits`
    // array named, and never checked it against the key. A proof therefore said
    // only "a leaf committing to (key, value) hashes up to this root somewhere".
    // The helpers below rebuild that old verifier so the exploits can be shown
    // to be accepted by it and rejected by the current one.

    /// The empty-subtree hash ladder: `d[0]` is an empty slot, `d[k+1] =
    /// H(0x01 || d[k] || d[k])`. Port of Go's `defaultHashes`.
    fn default_hashes() -> Vec<[u8; 32]> {
        let mut d = vec![[0u8; 32]];
        for k in 1..=TREE_DEPTH {
            d.push(interior_hash(&d[k - 1], &d[k - 1]));
        }
        d
    }

    /// The pre-fix verifier, verbatim: fold on the side `bits` says.
    fn legacy_recompute_root(
        key: &[u8; 32],
        value: &[u8],
        sibling_hashes: &[Vec<u8>],
        bits: &[u32],
    ) -> Option<[u8; 32]> {
        if sibling_hashes.len() != bits.len() {
            return None;
        }
        let mut current = leaf_hash(key, value);
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

    /// A full-depth proof for `key` in a tree that holds only `key`: every
    /// sibling is the empty subtree for its level. This is what the Go prover
    /// emits for a one-leaf tree.
    fn lone_leaf_proof() -> Vec<Vec<u8>> {
        let d = default_hashes();
        (0..TREE_DEPTH)
            .map(|i| d[TREE_DEPTH - 1 - i].to_vec())
            .collect()
    }

    #[test]
    fn a_proof_must_span_the_whole_tree() {
        let key = leaf_key("00000000-0000-0000-0000-000000000000");
        let value = vec![0x05u8; 33];

        // The degenerate exploit: no siblings at all. The old verifier returned
        // the bare leaf hash, so a server publishing that as `prefix_root`
        // passed with an empty proof and an attacker-chosen value.
        assert_eq!(
            legacy_recompute_root(&key, &value, &[], &[]),
            Some(leaf_hash(&key, &value)),
            "the empty-proof exploit did not reproduce against the legacy verifier"
        );
        assert!(recompute_root(&key, &value, &[]).is_none());

        let full = lone_leaf_proof();
        assert!(recompute_root(&key, &value, &full).is_some());
        assert!(recompute_root(&key, &value, &full[..TREE_DEPTH - 1]).is_none());
        assert!(recompute_root(&key, &value, &full[..1]).is_none());
        let mut too_long = full.clone();
        too_long.push(vec![0u8; 32]);
        assert!(recompute_root(&key, &value, &too_long).is_none());
        // A sibling that is not 32 bytes is malformed, not "close enough".
        let mut bad = full.clone();
        bad[7] = vec![0u8; 31];
        assert!(recompute_root(&key, &value, &bad).is_none());
    }

    #[test]
    fn a_path_that_contradicts_the_key_is_rejected() {
        let key = leaf_key("11111111-1111-1111-1111-111111111111");
        let value = vec![0x05u8; 33];
        let siblings = lone_leaf_proof();
        let honest = recompute_root(&key, &value, &siblings).expect("full-depth proof");

        // The key's own bits reproduce the honest root, so the only thing that
        // changes below is the path.
        let key_bits: Vec<u32> = (0..TREE_DEPTH)
            .map(|i| path_bit(&key, TREE_DEPTH - 1 - i) as u32)
            .collect();
        assert_eq!(
            legacy_recompute_root(&key, &value, &siblings, &key_bits),
            Some(honest)
        );

        for level in [0usize, 1, 128, TREE_DEPTH - 1] {
            let mut forged_bits = key_bits.clone();
            forged_bits[TREE_DEPTH - 1 - level] ^= 1;
            let attacker = vec![0x42u8; 33];
            let forged_root =
                legacy_recompute_root(&key, &attacker, &siblings, &forged_bits).unwrap();
            // The old verifier accepted this: it folded where it was told.
            assert_eq!(
                legacy_recompute_root(&key, &attacker, &siblings, &forged_bits),
                Some(forged_root),
                "level {level}: the forged-path exploit did not reproduce"
            );
            // The current one derives the path from the key, so it does not.
            assert_ne!(
                recompute_root(&key, &attacker, &siblings).unwrap(),
                forged_root,
                "level {level}: a path contradicting the key still verified"
            );
            assert_ne!(
                recompute_root(&key, &value, &siblings).unwrap(),
                forged_root,
                "level {level}: honest leaf verified against the forged root"
            );
        }
    }

    #[test]
    fn one_root_admits_one_value_per_key() {
        let key = leaf_key("22222222-2222-2222-2222-222222222222");
        let siblings = lone_leaf_proof();
        let root = recompute_root(&key, &vec![0x05u8; 33], &siblings).unwrap();
        assert_ne!(
            recompute_root(&key, &vec![0x42u8; 33], &siblings).unwrap(),
            root
        );
    }

    #[test]
    fn path_bit_matches_go_get_bit() {
        // Go: getBit(key, 255) is the MSB of key[0]; getBit(key, 0) is the LSB
        // of key[31].
        let mut key = [0u8; 32];
        key[0] = 0b1000_0000;
        assert_eq!(path_bit(&key, 255), 1);
        assert_eq!(path_bit(&key, 254), 0);
        key = [0u8; 32];
        key[31] = 1;
        assert_eq!(path_bit(&key, 0), 1);
        assert_eq!(path_bit(&key, 1), 0);
    }

    /// Cross-implementation vector for the PREFIX tree, produced by the Go
    /// prover (`SparseMerkleTree.ProveInclusion`) over a 5-leaf tree whose leaf
    /// i is (SHA-256([i]), "identity-key-i"). Every sibling is the empty-subtree
    /// default for its level except the two listed, so the whole 8 KiB proof is
    /// reconstructed here rather than pasted. If the two sides ever disagree on
    /// the leaf/interior tags, the fold order, the bit convention or the
    /// full-depth padding, this fails — which would otherwise show up in
    /// production as "every client rejects every honest proof".
    #[test]
    fn go_prefix_tree_inclusion_vector() {
        let key = unhex("dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986");
        let value = b"identity-key-2";
        let expected_root =
            unhex("7bdd6d14a174a33ac610c1fbd6219a0cfd57d23993faa282ae4f1be5cf03dc8b");

        let mut siblings = lone_leaf_proof();
        siblings[0] =
            unhex("54a7d0680bcb2a192723a932178cb2efdb56aa1f2ac6b03a4f2f9b038596194e").to_vec();
        siblings[2] =
            unhex("5562603069ddc9a5402b15bbb4238464b3af2f1a3d6518ad8eba5caecd984db3").to_vec();

        assert_eq!(siblings.len(), TREE_DEPTH);
        assert_eq!(recompute_root(&key, value, &siblings), Some(expected_root));
    }

    // ── Cross-language interop with the Go log tree ──────────────────────────
    //
    // Every vector below was produced by the Go implementation
    // (internal/services/keytransparency/crypto/logtree.go) over a 16-leaf log
    // whose leaf i is LogLeafHash([i; 32], [i + 128; 32]). If the two sides ever
    // disagree on a domain-separation byte, a hash order, or the RFC 6962 index
    // arithmetic, these fail — which is the failure mode that would otherwise
    // show up as "every client rejects every honest proof".

    fn vec_leaf(i: u8) -> [u8; 32] {
        log_leaf_hash(&[i; 32], &[i.wrapping_add(128); 32])
    }

    fn unhex(h: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).expect("hex");
        }
        out
    }

    fn unhex_list(hs: &[&str]) -> Vec<[u8; 32]> {
        hs.iter().map(|h| unhex(h)).collect()
    }

    /// Go's roots for log sizes 0..=16.
    const GO_ROOTS: [&str; 17] = [
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "ff294c2759f7cd422e8a5b839e4b7300a1da3c417c5aeade37ade51dd02f62ee",
        "2344eaa8fe15e8aed52902eaa5dd1ac6869c228d3c5fd801efe7dd565fd5e6bd",
        "7f218f4c6a5bf1f4ec901ca3ade55da66b0051df18985b270ebd6cd6d38de8db",
        "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
        "2584a54291ccad8f3a7381165b277198685d001b5481675fa5ae90031b118e95",
        "eb69cea77a87ec5d1289107f5019850b7534aba5f0b02da8db84b25c13d4656e",
        "478aa4cf7e5b00677fd96d475efa2babc7182bd90b7df4fac609aa0766bcaa04",
        "69871fc3acd1180a538e9594cd749a6e308f37a4f0247bad393f28975f987d33",
        "4f8320fe77b70a70e2fb87f616b11f0d1f29e9763d9b6ce7060b1e82608cf1ef",
        "caa0c5b37ab7e5e309253d394309afc95377caf44f9cdae95849ad8e407a05af",
        "b474caa14cd75b2bbabc91ddf6c686c6f81998fd694eec9a68918ea8a504d2b0",
        "199476e04cdf99717c300e0130aeebbe9cf84c1b1748d393bbcb3be8b53b512a",
        "1c9c803f09cb9c15c54d845c8821228625c480624c3c801dfe17b58c8512018b",
        "bc8282501c4dd224c3de740fb00580b3e42557e372ff391c570488cc7ea0c4d3",
        "4c321500a068be2c2edb486d2fbbaaf083382f0346a8732c98dfd9da3454afd9",
        "e8b3fcfd6ecc821592773e6054d3a662b3a16172df964122891becb8ef498f51",
    ];

    /// RFC 6962 defines the empty tree's hash as SHA-256 of the empty string;
    /// Go agrees, and so must we.
    #[test]
    fn empty_log_root_matches_rfc6962() {
        assert_eq!(unhex(GO_ROOTS[0]), <[u8; 32]>::from(Sha256::digest([])));
    }

    #[test]
    fn log_inclusion_verifies_against_go_vectors() {
        // (leaf index, log size, audit path)
        let cases: &[(u64, u64, &[&str])] = &[
            (0, 1, &[]),
            (
                2,
                5,
                &[
                    "9da80170723ae9e44c40a54a60cc112881227e421f252e545074aff9547b92a9",
                    "2344eaa8fe15e8aed52902eaa5dd1ac6869c228d3c5fd801efe7dd565fd5e6bd",
                    "1ac483f08581ebbb526cbb11891ac61417aee138eacbb15a40b6dc0cbe5a69c8",
                ],
            ),
            (
                6,
                7,
                &[
                    "e0fdbaaa5767a6798d08c44142b1e08a64ff21f44050aeadd6dc0b60dafe59d2",
                    "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
                ],
            ),
            (
                0,
                16,
                &[
                    "4576f3ef1d5ae23a4ad5b6c28f52e87c9b77d42c14c0b64cc0c22f646c383a13",
                    "361233c5ad54405c1725ccdbf427050c1f2af1a25d92e08ca18f3f6ef318caf8",
                    "95423c2ef941d804a6ffed1bed47bd339c2350651173703e0ea667013f2cfac4",
                    "fc52510871fd8cc7ba5aaf99e47b991b7f47073f4ee935f3458dbdc856c0ad6d",
                ],
            ),
            (
                15,
                16,
                &[
                    "d1654966fd7f1f25c26401986d36206b34e4a92c86555557ce645efbbdfe5035",
                    "5ab30febf0bc24e653fb5420c8fe1f81e799edfef66a57ee1ce0b4152847f14a",
                    "2988fc3ac205d5c7c88e3ff9a0b6e25cc9dbb1068ba531cb6825c61d8e280191",
                    "69871fc3acd1180a538e9594cd749a6e308f37a4f0247bad393f28975f987d33",
                ],
            ),
            (
                9,
                13,
                &[
                    "2a212c95aa87cbfc4700725f6027a5d90b411f4a3229745f8d2cf0f1ba7c4eb7",
                    "3992ed97d045e067babbeca03bdfaf42c62f2b6abc19a63c62d46f5999ec1f5c",
                    "bb215edff47683785ac67f086d872b4d7c4e795da62917fd5128b3ac0c190da8",
                    "69871fc3acd1180a538e9594cd749a6e308f37a4f0247bad393f28975f987d33",
                ],
            ),
        ];

        for &(index, size, path) in cases {
            let root = unhex(GO_ROOTS[size as usize]);
            let path = unhex_list(path);
            let leaf = vec_leaf(index as u8);
            assert!(
                verify_log_inclusion(&leaf, index, size, &path, &root),
                "Go's audit path for leaf {index} of {size} must verify here"
            );
            // A forged leaf at the same position must not.
            assert!(
                !verify_log_inclusion(&vec_leaf(200), index, size, &path, &root),
                "a forged leaf verified at {index} of {size}"
            );
        }
    }

    #[test]
    fn log_consistency_verifies_against_go_vectors() {
        // (old size, new size, path)
        let cases: &[(u64, u64, &[&str])] = &[
            (
                1,
                2,
                &["4576f3ef1d5ae23a4ad5b6c28f52e87c9b77d42c14c0b64cc0c22f646c383a13"],
            ),
            (
                3,
                7,
                &[
                    "13f89c99b70ec2e84a507f143c6bb766fe21e1531013f46cf5797a583c5ece73",
                    "9da80170723ae9e44c40a54a60cc112881227e421f252e545074aff9547b92a9",
                    "2344eaa8fe15e8aed52902eaa5dd1ac6869c228d3c5fd801efe7dd565fd5e6bd",
                    "826066dcfdadeb7fc16e022158923c72f10274dc72392b71653cb57a0335f01b",
                ],
            ),
            (
                4,
                8,
                &["95423c2ef941d804a6ffed1bed47bd339c2350651173703e0ea667013f2cfac4"],
            ),
            (
                6,
                13,
                &[
                    "e0fdbaaa5767a6798d08c44142b1e08a64ff21f44050aeadd6dc0b60dafe59d2",
                    "1cca1b58c9a0c16478344cac5f523e63497b6ed266d72d47d6a4a0d0e7f61923",
                    "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
                    "f27cde603e98dcae8240a20464111ed5e53fe5bd595e6f0fddb5b660458b8e99",
                ],
            ),
            (
                8,
                16,
                &["fc52510871fd8cc7ba5aaf99e47b991b7f47073f4ee935f3458dbdc856c0ad6d"],
            ),
            (
                5,
                16,
                &[
                    "1ac483f08581ebbb526cbb11891ac61417aee138eacbb15a40b6dc0cbe5a69c8",
                    "425d188a138c27f842ae2b4f7ae92bbd58d5ed0e373fed26d0fd4fb679479535",
                    "1cca1b58c9a0c16478344cac5f523e63497b6ed266d72d47d6a4a0d0e7f61923",
                    "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
                    "fc52510871fd8cc7ba5aaf99e47b991b7f47073f4ee935f3458dbdc856c0ad6d",
                ],
            ),
        ];

        for &(old, new, path) in cases {
            let old_root = unhex(GO_ROOTS[old as usize]);
            let new_root = unhex(GO_ROOTS[new as usize]);
            let path = unhex_list(path);
            assert!(
                verify_log_consistency(old, new, &old_root, &new_root, &path),
                "Go's consistency proof {old}->{new} must verify here"
            );
        }
    }

    /// THE DEFECT, at the level the client can see it. A rotation grows the log
    /// by one entry and rewrites nothing, so consistency across it holds. This
    /// is the case the old map-based proof rejected, wedging every pinned
    /// client out of every new session.
    #[test]
    fn a_rotation_is_just_another_append_and_stays_consistent() {
        // Sizes 5 -> 16 spans eleven further appends, any of which may be a
        // rotation: the log tree cannot tell, and does not need to.
        let path = unhex_list(&[
            "1ac483f08581ebbb526cbb11891ac61417aee138eacbb15a40b6dc0cbe5a69c8",
            "425d188a138c27f842ae2b4f7ae92bbd58d5ed0e373fed26d0fd4fb679479535",
            "1cca1b58c9a0c16478344cac5f523e63497b6ed266d72d47d6a4a0d0e7f61923",
            "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
            "fc52510871fd8cc7ba5aaf99e47b991b7f47073f4ee935f3458dbdc856c0ad6d",
        ]);
        assert!(verify_log_consistency(
            5,
            16,
            &unhex(GO_ROOTS[5]),
            &unhex(GO_ROOTS[16]),
            &path
        ));
    }

    /// ...and the negative control that keeps it honest: the same proof must
    /// NOT verify against a root the client never pinned, nor to a different
    /// new root. This is how a rewritten history is caught.
    #[test]
    fn a_consistency_proof_to_an_unpinned_root_is_rejected() {
        let path = unhex_list(&[
            "1ac483f08581ebbb526cbb11891ac61417aee138eacbb15a40b6dc0cbe5a69c8",
            "425d188a138c27f842ae2b4f7ae92bbd58d5ed0e373fed26d0fd4fb679479535",
            "1cca1b58c9a0c16478344cac5f523e63497b6ed266d72d47d6a4a0d0e7f61923",
            "d1284b3d2aa9b337437e6317a939826cbb5bab2be986304a8374711fa038db12",
            "fc52510871fd8cc7ba5aaf99e47b991b7f47073f4ee935f3458dbdc856c0ad6d",
        ]);
        // Old root the device never saw (a rewritten history reconstructs to a
        // different old root, which is exactly this shape).
        assert!(!verify_log_consistency(
            5,
            16,
            &unhex(GO_ROOTS[6]),
            &unhex(GO_ROOTS[16]),
            &path
        ));
        // New root that is not the head being offered.
        assert!(!verify_log_consistency(
            5,
            16,
            &unhex(GO_ROOTS[5]),
            &unhex(GO_ROOTS[15]),
            &path
        ));
        // Truncation / reordering shows up as a size that does not fit the path.
        assert!(!verify_log_consistency(
            5,
            15,
            &unhex(GO_ROOTS[5]),
            &unhex(GO_ROOTS[15]),
            &path
        ));
    }

    #[test]
    fn log_consistency_rejects_degenerate_ranges() {
        let r = unhex(GO_ROOTS[4]);
        assert!(!verify_log_consistency(0, 4, &r, &r, &[]));
        assert!(!verify_log_consistency(4, 4, &r, &r, &[]));
        assert!(!verify_log_consistency(5, 4, &r, &r, &[]));
    }

    #[test]
    fn pinned_key_parses() {
        assert!(VerifyingKey::from_bytes(&KT_TREE_HEAD_PUBLIC_KEY_BYTES).is_ok());
    }
}
