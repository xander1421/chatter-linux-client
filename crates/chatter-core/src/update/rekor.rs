//! Offline verification that a release artifact is in Sigstore's Rekor log.
//!
//! The release-update Ed25519 signature says the operator authorized an
//! artifact; this says the authorization was made PUBLIC. A stolen release key
//! can still sign a targeted malicious build, but no client installs it unless
//! its hash is also in an append-only public log under the pinned
//! release-transparency key, where a monitor sees it
//! (`rekor-cli search --public-key`).
//!
//! What the publisher puts beside the signed manifest is Rekor v1's own
//! `LogEntry` JSON (`GET /api/v1/log/entries/{uuid}`), verbatim. It is checked
//! the way sigstore-go checks a v1 bundle, entirely offline:
//!
//! 1. `logID` is SHA-256 of the pinned log key's SPKI;
//! 2. the signed entry timestamp (ECDSA over canonical
//!    `{body,integratedTime,logID,logIndex}`) verifies under the log key;
//! 3. the checkpoint (a C2SP signed note) verifies under the log key and names
//!    the proof's tree size and root;
//! 4. `SHA-256(0x00 || body)` is included at the proof's index (RFC 6962);
//! 5. the body is a `hashedrekord` whose SHA-256 is the manifest's, whose key
//!    is the pinned release-transparency key, and whose ECDSA signature over
//!    that digest verifies.
//!
//! Formats were checked against live rekor.sigstore.dev responses on
//! 2026-11-13 (`tests/fixtures/rekor/`), not only against documentation.
//!
//! Consistency: the client keeps the newest checkpoint it accepted and, when
//! a release names a larger tree, requires an RFC 6962 consistency proof from
//! that size (Rekor serves them at `GET /api/v1/log/proof?firstSize=&lastSize=
//! &treeID=`; the publisher copies them into `consistency.json`). Ceiling: a
//! checkpoint for a DIFFERENT tree (Rekor rotates shards; the origin line
//! changes) is accepted fresh, because no proof can span shards.

use base64::Engine;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::signature::hazmat::PrehashVerifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// DER prefix of a P-256 SubjectPublicKeyInfo; the 65-byte point follows.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

pub const MAX_CONSISTENCY_BYTES: usize = 1024 * 1024;

/// `{"uuid": ..., "entry": <Rekor LogEntry>}` beside `manifest`/`signature`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RekorBundle {
    pub uuid: String,
    pub entry: LogEntry,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LogEntry {
    pub body: String,
    pub integrated_time: i64,
    #[serde(rename = "logID")]
    pub log_id: String,
    pub log_index: u64,
    pub verification: EntryVerification,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EntryVerification {
    pub inclusion_proof: InclusionProof,
    pub signed_entry_timestamp: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InclusionProof {
    pub checkpoint: String,
    pub hashes: Vec<String>,
    pub log_index: u64,
    pub root_hash: String,
    pub tree_size: u64,
}

/// A verified signed tree head. `origin` is the note's first line
/// (`rekor.sigstore.dev - <treeID>`), which identifies the shard.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub origin: String,
    pub size: u64,
    pub root: [u8; 32],
}

/// `consistency.json`: proofs from every earlier release checkpoint of this
/// platform/channel to `to_size` (the current release's checkpoint).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConsistencyFile {
    pub to_size: u64,
    pub proofs: Vec<ConsistencyProof>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConsistencyProof {
    pub from_size: u64,
    pub hashes: Vec<String>,
}

fn spki(point: &[u8; 65]) -> Vec<u8> {
    [&P256_SPKI_PREFIX[..], point].concat()
}

fn key(point: &[u8; 65]) -> Result<VerifyingKey, String> {
    VerifyingKey::from_sec1_bytes(point).map_err(|_| "pinned P-256 key is invalid".to_string())
}

fn rfc6962_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0x01]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

fn rfc6962_leaf(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(data);
    h.finalize().into()
}

fn hex32(text: &str, what: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(text).map_err(|_| format!("Rekor {what} is malformed"))?;
    if text != text.to_ascii_lowercase() {
        return Err(format!("Rekor {what} is malformed"));
    }
    bytes
        .try_into()
        .map_err(|_| format!("Rekor {what} is malformed"))
}

fn hashes32(raw: &[String]) -> Result<Vec<[u8; 32]>, String> {
    raw.iter().map(|h| hex32(h, "proof hash")).collect()
}

fn b64(text: &str, what: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| format!("Rekor {what} is malformed"))
}

fn der_signature(bytes: &[u8], what: &str) -> Result<Signature, String> {
    Signature::from_der(bytes).map_err(|_| format!("Rekor {what} is malformed"))
}

/// Verify a C2SP signed note from the pinned log key.
pub fn verify_checkpoint(note: &str, log_key: &[u8; 65]) -> Result<Checkpoint, String> {
    let split = note.find("\n\n").ok_or("Rekor checkpoint is malformed")?;
    let (text, signatures) = (&note[..split + 1], &note[split + 2..]);
    let mut lines = text.lines();
    let (Some(origin), Some(size), Some(root)) = (lines.next(), lines.next(), lines.next()) else {
        return Err("Rekor checkpoint is malformed".into());
    };
    if origin.is_empty() || size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Rekor checkpoint is malformed".into());
    }
    let size: u64 = size.parse().map_err(|_| "Rekor checkpoint is malformed")?;
    let root: [u8; 32] = b64(root, "checkpoint root")?
        .try_into()
        .map_err(|_| "Rekor checkpoint root is malformed")?;
    if !note.ends_with('\n') {
        return Err("Rekor checkpoint is malformed".into());
    }
    let hint = &Sha256::digest(spki(log_key))[..4];
    let verifier = key(log_key)?;
    let signed = signatures.lines().any(|line| {
        let Some(rest) = line.strip_prefix("\u{2014} ") else {
            return false;
        };
        let Some((_name, sig)) = rest.rsplit_once(' ') else {
            return false;
        };
        let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(sig) else {
            return false;
        };
        raw.len() > 4
            && &raw[..4] == hint
            && Signature::from_der(&raw[4..])
                .is_ok_and(|s| verifier.verify(text.as_bytes(), &s).is_ok())
    });
    if !signed {
        return Err("Rekor checkpoint is not signed by the pinned log key".into());
    }
    Ok(Checkpoint {
        origin: origin.to_string(),
        size,
        root,
    })
}

/// Steps 1-4: the entry is in the log the pinned key speaks for. Returns the
/// decoded body and the verified checkpoint.
pub fn verify_log_entry(
    entry: &LogEntry,
    log_key: &[u8; 65],
) -> Result<(Vec<u8>, Checkpoint), String> {
    let log_id = hex::encode(Sha256::digest(spki(log_key)));
    if entry.log_id != log_id {
        return Err("Rekor entry is from another log".into());
    }
    let body = b64(&entry.body, "entry body")?;
    // Canonical JSON (sorted keys, compact) exactly as Rekor signs it. `body`
    // is validated base64 and `logID` equals our hex, so neither needs escaping.
    let set_payload = format!(
        r#"{{"body":"{}","integratedTime":{},"logID":"{}","logIndex":{}}}"#,
        entry.body, entry.integrated_time, entry.log_id, entry.log_index
    );
    let set = der_signature(
        &b64(
            &entry.verification.signed_entry_timestamp,
            "signed entry timestamp",
        )?,
        "signed entry timestamp",
    )?;
    key(log_key)?
        .verify(set_payload.as_bytes(), &set)
        .map_err(|_| "Rekor signed entry timestamp is invalid")?;

    let proof = &entry.verification.inclusion_proof;
    let checkpoint = verify_checkpoint(&proof.checkpoint, log_key)?;
    if checkpoint.size != proof.tree_size
        || checkpoint.root != hex32(&proof.root_hash, "root hash")?
    {
        return Err("Rekor inclusion proof does not match its checkpoint".into());
    }
    if !verify_log_inclusion(
        &rfc6962_leaf(&body),
        proof.log_index,
        proof.tree_size,
        &hashes32(&proof.hashes)?,
        &checkpoint.root,
    ) {
        return Err("Rekor inclusion proof is invalid".into());
    }
    Ok((body, checkpoint))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HashedRekord {
    api_version: String,
    kind: String,
    spec: HashedRekordSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashedRekordSpec {
    data: HashedRekordData,
    signature: HashedRekordSignature,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashedRekordData {
    hash: HashedRekordHash,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashedRekordHash {
    algorithm: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HashedRekordSignature {
    content: String,
    public_key: HashedRekordKey,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashedRekordKey {
    content: String,
}

/// All five steps: `sha256_hex` (the signed manifest's digest) is logged by
/// the pinned transparency key in the pinned log. Returns the checkpoint for
/// the caller's consistency check.
pub fn verify_release_entry(
    bundle: &RekorBundle,
    log_key: &[u8; 65],
    transparency_key: &[u8; 65],
    sha256_hex: &str,
) -> Result<Checkpoint, String> {
    let (body, checkpoint) = verify_log_entry(&bundle.entry, log_key)?;
    let record: HashedRekord =
        serde_json::from_slice(&body).map_err(|_| "Rekor entry is not a hashedrekord")?;
    if record.api_version != "0.0.1"
        || record.kind != "hashedrekord"
        || record.spec.data.hash.algorithm != "sha256"
    {
        return Err("Rekor entry is not a sha256 hashedrekord".into());
    }
    let digest = hex32(&record.spec.data.hash.value, "artifact digest")?;
    if record.spec.data.hash.value != sha256_hex {
        return Err("Rekor entry logs a different artifact".into());
    }
    let pem = String::from_utf8(b64(&record.spec.signature.public_key.content, "entry key")?)
        .map_err(|_| "Rekor entry key is malformed")?;
    let der = pem
        .trim_end()
        .strip_prefix("-----BEGIN PUBLIC KEY-----\n")
        .and_then(|rest| rest.strip_suffix("-----END PUBLIC KEY-----"))
        .map(|body| body.split_whitespace().collect::<String>())
        .ok_or("Rekor entry key is not a PEM public key")?;
    if b64(&der, "entry key")? != spki(transparency_key) {
        return Err("Rekor entry is not signed by the pinned release-transparency key".into());
    }
    let signature = der_signature(
        &b64(&record.spec.signature.content, "entry signature")?,
        "entry signature",
    )?;
    key(transparency_key)?
        .verify_prehash(&digest, &signature)
        .map_err(|_| "Rekor entry signature is invalid")?;
    Ok(checkpoint)
}

/// Whether accepting `new` needs a consistency proof from `stored`.
pub fn needs_consistency_proof(stored: Option<&Checkpoint>, new: &Checkpoint) -> bool {
    stored.is_some_and(|s| s.origin == new.origin && s.size < new.size)
}

/// Refuse a checkpoint that rolls back, forks, or extends the tree this device
/// last accepted without a proof that it is an extension.
pub fn check_consistency(
    stored: Option<&Checkpoint>,
    new: &Checkpoint,
    proofs: Option<&ConsistencyFile>,
) -> Result<(), String> {
    let Some(stored) = stored else {
        return Ok(());
    };
    if stored.origin != new.origin {
        tracing::warn!(old = %stored.origin, new = %new.origin, "Rekor tree changed; pinning the new shard");
        return Ok(());
    }
    if new.size < stored.size {
        return Err("Rekor checkpoint is older than one already seen".into());
    }
    if new.size == stored.size {
        return if new.root == stored.root {
            Ok(())
        } else {
            Err("Rekor checkpoint forks from one already seen".into())
        };
    }
    let file = proofs.ok_or("Rekor consistency proof is missing")?;
    if file.to_size != new.size {
        return Err("Rekor consistency proofs target another checkpoint".into());
    }
    let proof = file
        .proofs
        .iter()
        .find(|p| p.from_size == stored.size)
        .ok_or("Rekor consistency proof is missing")?;
    if !verify_log_consistency(
        stored.size,
        new.size,
        &stored.root,
        &new.root,
        &hashes32(&proof.hashes)?,
    ) {
        return Err("Rekor log is not an extension of the checkpoint already seen".into());
    }
    Ok(())
}

/// Evaluate the RFC 6962 audit path against the checkpoint root.
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
            r = rfc6962_node(p, &r);
            while fnv & 1 == 0 && fnv != 0 {
                fnv >>= 1;
                sn >>= 1;
            }
        } else {
            r = rfc6962_node(&r, p);
        }
        fnv >>= 1;
        sn >>= 1;
    }
    sn == 0 && r == *root
}

/// Check that both Rekor checkpoint roots are reconstructed by the RFC 6962 path.
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
            fr = rfc6962_node(c, &fr);
            sr = rfc6962_node(c, &sr);
            while fnv & 1 == 0 && fnv != 0 {
                fnv >>= 1;
                sn >>= 1;
            }
        } else {
            sr = rfc6962_node(&sr, c);
        }
        fnv >>= 1;
        sn >>= 1;
    }

    sn == 0 && fr == *old_root && sr == *new_root
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::signature::hazmat::PrehashSigner;

    const PROD_ENTRY: &str = include_str!("../../tests/fixtures/rekor/entry-3721594000.json");
    const PROD_CONSISTENCY: &str =
        include_str!("../../tests/fixtures/rekor/consistency-3721597586-3721608161.json");

    fn prod_entry() -> (String, LogEntry) {
        let map: std::collections::BTreeMap<String, LogEntry> =
            serde_json::from_str(PROD_ENTRY).unwrap();
        map.into_iter().next().unwrap()
    }

    fn signer(label: &[u8]) -> SigningKey {
        SigningKey::from_slice(&Sha256::digest(label)).unwrap()
    }

    fn point(key: &SigningKey) -> [u8; 65] {
        key.verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap()
    }

    // ── recorded rekor.sigstore.dev data ────────────────────────────────

    #[test]
    fn recorded_rekor_entry_verifies_offline() {
        let (uuid, entry) = prod_entry();
        let (body, checkpoint) =
            verify_log_entry(&entry, &crate::config::PUBLIC_GOOD_REKOR_LOG_PUBLIC_KEY).unwrap();
        assert!(uuid.ends_with(&hex::encode(rfc6962_leaf(&body))));
        assert_eq!(
            checkpoint.origin,
            "rekor.sigstore.dev - 1193050959916656506"
        );
        assert_eq!(checkpoint.size, 3721597586);
    }

    #[test]
    fn recorded_rekor_entry_rejections() {
        let prod = crate::config::PUBLIC_GOOD_REKOR_LOG_PUBLIC_KEY;
        let (_, entry) = prod_entry();
        let reject = |e: &LogEntry, k: &[u8; 65]| verify_log_entry(e, k).unwrap_err();

        // Another log's key: logID mismatch.
        assert!(reject(&entry, &crate::config::DEV_REKOR_LOG_PUBLIC_KEY).contains("another log"));

        // Body swapped for another (valid base64) body: the SET no longer covers it.
        let mut e = entry.clone();
        e.body = base64::engine::general_purpose::STANDARD.encode(b"{}");
        assert!(reject(&e, &prod).contains("timestamp"));

        // Global index altered: SET fails.
        let mut e = entry.clone();
        e.log_index += 1;
        assert!(reject(&e, &prod).contains("timestamp"));

        // Inclusion path altered.
        let mut e = entry.clone();
        e.verification.inclusion_proof.hashes[3] = "00".repeat(32);
        assert!(reject(&e, &prod).contains("inclusion proof is invalid"));

        // Proof index altered.
        let mut e = entry.clone();
        e.verification.inclusion_proof.log_index -= 1;
        assert!(reject(&e, &prod).contains("inclusion proof is invalid"));

        // Proof claims a root its checkpoint does not sign.
        let mut e = entry.clone();
        e.verification.inclusion_proof.root_hash = "11".repeat(32);
        assert!(reject(&e, &prod).contains("does not match its checkpoint"));

        // Checkpoint text altered under its signature.
        let mut e = entry.clone();
        e.verification.inclusion_proof.checkpoint = e
            .verification
            .inclusion_proof
            .checkpoint
            .replacen("3721597586", "3721597587", 1);
        e.verification.inclusion_proof.tree_size += 1;
        assert!(reject(&e, &prod).contains("not signed by the pinned log key"));

        // Unsigned checkpoint.
        let mut e = entry.clone();
        let cp = &e.verification.inclusion_proof.checkpoint;
        e.verification.inclusion_proof.checkpoint = cp[..cp.find("\n\n").unwrap() + 2].to_string();
        assert!(reject(&e, &prod).contains("not signed"));

        // A real entry is someone else's artifact and key.
        let bundle = RekorBundle {
            uuid: String::new(),
            entry,
        };
        let digest = "3411426703e2906be52dc1ba39e6935e52de285c91d2238995fb20b7e9c1ced5";
        let err = verify_release_entry(
            &bundle,
            &prod,
            &crate::config::DEV_REKOR_LOG_PUBLIC_KEY,
            digest,
        )
        .unwrap_err();
        assert!(
            err.contains("PEM public key") || err.contains("transparency key"),
            "{err}"
        );
    }

    #[test]
    fn recorded_rekor_consistency_proof() {
        let prod = crate::config::PUBLIC_GOOD_REKOR_LOG_PUBLIC_KEY;
        let (_, entry) = prod_entry();
        let (_, old) = verify_log_entry(&entry, &prod).unwrap();
        #[derive(Deserialize)]
        struct Recorded {
            to_checkpoint: String,
            from_size: u64,
            hashes: Vec<String>,
        }
        let rec: Recorded = serde_json::from_str(PROD_CONSISTENCY).unwrap();
        let new = verify_checkpoint(&rec.to_checkpoint, &prod).unwrap();
        assert_eq!(rec.from_size, old.size);
        let file = ConsistencyFile {
            to_size: new.size,
            proofs: vec![ConsistencyProof {
                from_size: rec.from_size,
                hashes: rec.hashes,
            }],
        };
        assert!(needs_consistency_proof(Some(&old), &new));
        check_consistency(Some(&old), &new, Some(&file)).unwrap();

        // A forged old root (a device that saw a different history) fails.
        let mut forked = old.clone();
        forked.root[0] ^= 1;
        assert!(check_consistency(Some(&forked), &new, Some(&file)).is_err());
        // A tampered path fails.
        let mut bad = file.clone();
        bad.proofs[0].hashes[0] = "22".repeat(32);
        assert!(check_consistency(Some(&old), &new, Some(&bad)).is_err());
        // No proof at all fails.
        assert!(check_consistency(Some(&old), &new, None).is_err());
        // Going back to the older checkpoint is a rollback.
        assert!(
            check_consistency(Some(&new), &old, None)
                .unwrap_err()
                .contains("older")
        );
    }

    // ── synthetic hashedrekord under test keys ──────────────────────────

    /// A tiny RFC 6962 log: inclusion path for `index` among `leaves`.
    fn mth(leaves: &[[u8; 32]]) -> [u8; 32] {
        if leaves.len() == 1 {
            return leaves[0];
        }
        let k = leaves.len().next_power_of_two() / 2;
        rfc6962_node(&mth(&leaves[..k]), &mth(&leaves[k..]))
    }

    fn path(index: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
        if leaves.len() == 1 {
            return vec![];
        }
        let k = leaves.len().next_power_of_two() / 2;
        if index < k {
            let mut p = path(index, &leaves[..k]);
            p.push(mth(&leaves[k..]));
            p
        } else {
            let mut p = path(index - k, &leaves[k..]);
            p.push(mth(&leaves[..k]));
            p
        }
    }

    fn note(log: &SigningKey, origin: &str, size: u64, root: &[u8; 32]) -> String {
        let text = format!(
            "{origin}\n{size}\n{}\n",
            base64::engine::general_purpose::STANDARD.encode(root)
        );
        let sig: Signature = log.sign(text.as_bytes());
        let mut raw = Sha256::digest(spki(&point(log)))[..4].to_vec();
        raw.extend_from_slice(sig.to_der().as_bytes());
        format!(
            "{text}\n\u{2014} test-log {}\n",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    fn release_body(signer_key: &SigningKey, digest: &[u8; 32]) -> String {
        let sig: Signature = signer_key.sign_prehash(digest).unwrap();
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(spki(&point(signer_key)))
        );
        format!(
            r#"{{"apiVersion":"0.0.1","kind":"hashedrekord","spec":{{"data":{{"hash":{{"algorithm":"sha256","value":"{}"}}}},"signature":{{"content":"{}","publicKey":{{"content":"{}"}}}}}}}}"#,
            hex::encode(digest),
            base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(pem)
        )
    }

    fn bundle_for(log: &SigningKey, body: &str) -> RekorBundle {
        let body64 = base64::engine::general_purpose::STANDARD.encode(body);
        let mut leaves: Vec<[u8; 32]> = (0u8..6).map(|i| rfc6962_leaf(&[i])).collect();
        leaves.push(rfc6962_leaf(body.as_bytes()));
        let index = leaves.len() - 1;
        let root = mth(&leaves);
        let log_id = hex::encode(Sha256::digest(spki(&point(log))));
        let set_payload = format!(
            r#"{{"body":"{body64}","integratedTime":1793983256,"logID":"{log_id}","logIndex":{index}}}"#
        );
        let set: Signature = log.sign(set_payload.as_bytes());
        RekorBundle {
            uuid: hex::encode(rfc6962_leaf(body.as_bytes())),
            entry: LogEntry {
                body: body64,
                integrated_time: 1793983256,
                log_id,
                log_index: index as u64,
                verification: EntryVerification {
                    inclusion_proof: InclusionProof {
                        checkpoint: note(log, "test-log - 1", leaves.len() as u64, &root),
                        hashes: path(index, &leaves).iter().map(hex::encode).collect(),
                        log_index: index as u64,
                        root_hash: hex::encode(root),
                        tree_size: leaves.len() as u64,
                    },
                    signed_entry_timestamp: base64::engine::general_purpose::STANDARD
                        .encode(set.to_der().as_bytes()),
                },
            },
        }
    }

    #[test]
    fn release_entry_accept_and_reject() {
        let log = signer(b"test-log");
        let release = signer(b"test-transparency");
        let digest: [u8; 32] = Sha256::digest(b"artifact").into();
        let digest_hex = hex::encode(digest);
        let good = bundle_for(&log, &release_body(&release, &digest));
        let checkpoint =
            verify_release_entry(&good, &point(&log), &point(&release), &digest_hex).unwrap();
        assert_eq!(
            (checkpoint.origin.as_str(), checkpoint.size),
            ("test-log - 1", 7)
        );

        // Different downloaded artifact.
        let other = hex::encode(Sha256::digest(b"other"));
        assert!(
            verify_release_entry(&good, &point(&log), &point(&release), &other)
                .unwrap_err()
                .contains("different artifact")
        );
        // Logged by a key that is not the pinned transparency key.
        let stranger = signer(b"stranger");
        let by_stranger = bundle_for(&log, &release_body(&stranger, &digest));
        assert!(
            verify_release_entry(&by_stranger, &point(&log), &point(&release), &digest_hex)
                .unwrap_err()
                .contains("transparency key")
        );
        // Pinned key, but its signature is over another digest.
        let mut body = release_body(&release, &Sha256::digest(b"other").into());
        body = body.replacen(&other, &digest_hex, 1);
        let forged = bundle_for(&log, &body);
        assert!(
            verify_release_entry(&forged, &point(&log), &point(&release), &digest_hex)
                .unwrap_err()
                .contains("signature is invalid")
        );
        // Not a hashedrekord.
        let body = release_body(&release, &digest).replacen("hashedrekord", "rekord", 1);
        let wrong_kind = bundle_for(&log, &body);
        assert!(
            verify_release_entry(&wrong_kind, &point(&log), &point(&release), &digest_hex).is_err()
        );
        // Correct entry from a log we do not pin.
        assert!(
            verify_release_entry(&good, &point(&stranger), &point(&release), &digest_hex)
                .unwrap_err()
                .contains("another log")
        );
    }

    #[test]
    fn consistency_rules() {
        let cp = |origin: &str, size: u64, root: u8| Checkpoint {
            origin: origin.into(),
            size,
            root: [root; 32],
        };
        let a = cp("log - 1", 10, 1);
        // First use pins whatever verified.
        assert!(check_consistency(None, &a, None).is_ok());
        assert!(!needs_consistency_proof(None, &a));
        // Same tree head again.
        assert!(check_consistency(Some(&a), &a, None).is_ok());
        // Same size, different root: a fork.
        assert!(
            check_consistency(Some(&a), &cp("log - 1", 10, 2), None)
                .unwrap_err()
                .contains("forks")
        );
        // A shard rotation is accepted fresh (documented ceiling).
        let b = cp("log - 2", 3, 9);
        assert!(!needs_consistency_proof(Some(&a), &b));
        assert!(check_consistency(Some(&a), &b, None).is_ok());
        // Proof file for some other checkpoint.
        let newer = cp("log - 1", 12, 3);
        let stale = ConsistencyFile {
            to_size: 11,
            proofs: vec![],
        };
        assert!(
            check_consistency(Some(&a), &newer, Some(&stale))
                .unwrap_err()
                .contains("another checkpoint")
        );
        // Proof file without this device's size.
        let unrelated = ConsistencyFile {
            to_size: 12,
            proofs: vec![ConsistencyProof {
                from_size: 9,
                hashes: vec![],
            }],
        };
        assert!(
            check_consistency(Some(&a), &newer, Some(&unrelated))
                .unwrap_err()
                .contains("missing")
        );
    }
}
