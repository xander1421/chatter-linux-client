//! Gossiped one-time prekey bundles: wire format, signature binding, and the
//! store policy that governs their lifetime.
//!
//! Ported from `BitFoundation/PrekeyBundle.swift`, plus the rules (not the
//! storage) in `Services/Prekeys/LocalPrekeyStore.swift` and
//! `Services/Prekeys/PrekeyBundleStore.swift`.
//!
//! A bundle publishes a batch of one-time Curve25519 public prekeys bound to the
//! owner's Noise static key by an Ed25519 signature. Anyone holding the owner's
//! announce-verified signing key can check a bundle offline, which is what lets
//! bundles spread and persist mesh-wide via gossip sync while the owner is away.
//! Senders then seal courier mail to a one-time prekey instead of the owner's
//! long-lived static key, which is what restores forward secrecy to async first
//! contact.
//!
//! # What the signature covers, and why it must
//!
//! [`PrekeyBundle::signing_bytes`] covers a domain-separating context, the owner's
//! static key, the prekey count, every `(id, key)` pair in order, and the
//! generation timestamp. Every one of those is load-bearing:
//!
//! - drop the owner key and a bundle can be re-attributed to another peer;
//! - drop any prekey and an attacker substitutes their own, making every courier
//!   message sealed to it readable by them;
//! - drop the count and prekeys can be appended or truncated;
//! - drop the timestamp and a stale bundle full of already-consumed prekeys can be
//!   replayed to force senders onto dead keys.
//!
//! The signature itself is not covered, which is what lets the owner compute the
//! bytes before it exists.
//!
//! # Storage is not here
//!
//! This crate is sans-IO and holds no clock. The replenishment thresholds, the
//! redelivery grace window, and the staleness limits are exposed as constants and
//! pure functions over a caller-supplied `now_ms`; keychain and disk persistence
//! belong to the integration layer.

use crate::error::NoiseError;
use crate::identity::MeshIdentity;
use crate::sign::{sign_bytes, verify_bytes};

/// Domain separation for the bundle signature, so it can never be confused with
/// an announce signature or a packet signature.
///
/// Distinct from [`crate::PREKEY_PROLOGUE_PREFIX`], which separates *seals* to a
/// prekey. Same subsystem, different construction, deliberately different string.
pub const PREKEY_BUNDLE_CONTEXT: &[u8] = b"bitchat-prekey-bundle-v1";

/// Curve25519 public key width.
pub const KEY_LEN: usize = 32;
/// Ed25519 signature width.
pub const SIGNATURE_LEN: usize = 64;
/// Most prekeys a single bundle may carry, and the size of a freshly minted batch.
pub const MAX_PREKEYS: usize = 8;

/// One `(id, key)` pair on the wire: big-endian u32 then a 32-byte public key.
const PREKEY_ENTRY_LEN: usize = 4 + KEY_LEN;

const TLV_OWNER_STATIC: u8 = 0x01;
const TLV_PREKEYS: u8 = 0x02;
const TLV_GENERATED_AT: u8 = 0x03;
const TLV_SIGNATURE: u8 = 0x04;

// MARK: - Store policy (rules only; the stores themselves live upstack)

/// Unconsumed prekeys below this count trigger a top-up back to [`MAX_PREKEYS`].
pub const REPLENISH_THRESHOLD: usize = 3;

/// How long a consumed prekey's private key is retained.
///
/// Spray-and-wait means the same prekey-sealed ciphertext can arrive via several
/// couriers days apart, and the recipient cannot distinguish a redelivery from a
/// fresh ciphertext sealed to an already-used prekey — so refusing the latter
/// while accepting the former is not possible. The window is therefore short and
/// fixed. The forward-secrecy clock starts when the private key is *deleted*, not
/// when the message is first opened; that is the cost of redelivery, and it is
/// why this constant must not be raised casually.
pub const CONSUMED_GRACE_MS: u64 = 48 * 60 * 60 * 1000;

/// Unconsumed prekeys older than this rotate out: no honest sender will seal to a
/// bundle that stale, since [`MAX_BUNDLE_AGE_FOR_SEALING_MS`] is shorter.
pub const UNCONSUMED_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Do not seal to a cached bundle older than this — the owner may already have
/// rotated those unconsumed keys out from under it.
pub const MAX_BUNDLE_AGE_FOR_SEALING_MS: u64 = 7 * 24 * 60 * 60 * 1000;

// Ordering invariants between the three windows. Checked at compile time rather
// than in a test: all three are constants, so a runtime assert is folded away and
// would never observe a bad edit.
//
// A bundle must stop being sealable well before its unconsumed prekeys are rotated
// out, or an honest sender would seal to a key the recipient has already deleted —
// mail that is undecryptable on arrival through no fault of either party.
const _: () = assert!(MAX_BUNDLE_AGE_FOR_SEALING_MS < UNCONSUMED_RETENTION_MS);
const _: () = assert!(CONSUMED_GRACE_MS < MAX_BUNDLE_AGE_FOR_SEALING_MS);

/// Cap on cached peer bundles; the least recently updated is evicted past it.
pub const MAX_CACHED_BUNDLES: usize = 200;

/// Whether the local batch should be topped back up to [`MAX_PREKEYS`].
pub fn needs_replenish(unconsumed: usize) -> bool {
    unconsumed < REPLENISH_THRESHOLD
}

/// Whether a local prekey private key may still open a seal: unconsumed, or
/// consumed within the redelivery grace window.
///
/// A `consumed_at_ms` in the future (clock skew, a restored backup) counts as
/// just-consumed rather than long-expired, matching Swift's signed
/// `timeIntervalSince` comparison. Failing open here is the right direction: the
/// alternative silently drops real mail.
pub fn prekey_can_open(consumed_at_ms: Option<u64>, now_ms: u64) -> bool {
    match consumed_at_ms {
        None => true,
        Some(consumed_at) => now_ms.saturating_sub(consumed_at) <= CONSUMED_GRACE_MS,
    }
}

/// Whether a local prekey record should be deleted outright: a consumed one past
/// its grace window, or an unconsumed one past its retention window.
pub fn prekey_is_expired(created_at_ms: u64, consumed_at_ms: Option<u64>, now_ms: u64) -> bool {
    match consumed_at_ms {
        Some(consumed_at) => now_ms.saturating_sub(consumed_at) > CONSUMED_GRACE_MS,
        None => now_ms.saturating_sub(created_at_ms) > UNCONSUMED_RETENTION_MS,
    }
}

/// Whether a cached peer bundle is fresh enough to seal to.
pub fn bundle_is_sealable(generated_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(generated_at_ms) <= MAX_BUNDLE_AGE_FOR_SEALING_MS
}

/// Whether an incoming bundle replaces the cached one for the same static key.
///
/// Strictly newer only. An equal stamp does *not* replace, which is why
/// [`next_generated_at`] must advance strictly on every change to the published
/// set: a same-stamp replacement is rejected by peers, they keep handing out an
/// ID we have already consumed, and their mail starts failing once the grace
/// window lapses.
pub fn bundle_supersedes(incoming_generated_at_ms: u64, cached_generated_at_ms: u64) -> bool {
    incoming_generated_at_ms > cached_generated_at_ms
}

/// The next `generated_at` stamp: wall-clock millis, but never repeating or
/// regressing, so two changes inside the same millisecond still produce distinct
/// increasing stamps that a peer's monotonic ingest accepts.
pub fn next_generated_at(previous_ms: u64, now_ms: u64) -> u64 {
    now_ms.max(previous_ms.wrapping_add(1))
}

/// A fresh one-time prekey: the half to publish and the secret to keep.
///
/// The secret is returned as raw bytes because its custody (sealed storage,
/// consumption, expiry) belongs to the caller; this crate holds no store.
pub fn generate_prekey(id: u32, rng: &mut impl rand_core::CryptoRng) -> (Prekey, [u8; KEY_LEN]) {
    let secret = x25519_dalek::StaticSecret::random_from_rng(rng);
    let public_key = x25519_dalek::PublicKey::from(&secret).to_bytes();
    (Prekey { id, public_key }, secret.to_bytes())
}

/// The public half of a stored prekey secret.
pub fn prekey_public(secret: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*secret)).to_bytes()
}

// MARK: - Types

/// One published one-time prekey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prekey {
    /// Owner-assigned identifier, unique within a bundle. Travels in the seal
    /// prologue so a ciphertext cannot be replayed against a different prekey.
    pub id: u32,
    /// Curve25519 key-agreement public key.
    pub public_key: [u8; KEY_LEN],
}

/// A signed batch of one-time prekeys, gossiped as `MessageType::PrekeyBundle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrekeyBundle {
    /// Noise static public key identifying whose prekeys these are.
    pub owner_static: [u8; KEY_LEN],
    /// One-time prekeys, at most [`MAX_PREKEYS`].
    ///
    /// **Order is part of the signature.** [`signing_bytes`](Self::signing_bytes)
    /// walks this vector as given and never sorts it, because a verifier must
    /// reproduce the bytes it received, not a normalised form of them. bitchat
    /// publishes ascending by ID; producers here should do the same, and
    /// verifiers must preserve whatever order arrived on the wire.
    pub prekeys: Vec<Prekey>,
    /// Milliseconds since the epoch when this bundle was generated. Newer bundles
    /// replace older ones for the same owner — see [`bundle_supersedes`].
    pub generated_at: u64,
    /// Ed25519 signature by the owner's announce-bound signing key over
    /// [`signing_bytes`](Self::signing_bytes).
    pub signature: [u8; SIGNATURE_LEN],
}

impl PrekeyBundle {
    /// Serialises to the gossiped TLV payload.
    ///
    /// ```text
    /// 0x01 ‖ be_u16(32)              ‖ owner_static
    /// 0x02 ‖ be_u16(36 * n)          ‖ (be_u32(id) ‖ key32) * n
    /// 0x03 ‖ be_u16(8)               ‖ be_u64(generated_at)
    /// 0x04 ‖ be_u16(64)              ‖ signature
    /// ```
    ///
    /// Field order is fixed and reproduced exactly: peers tolerate reordering, but
    /// emitting the canonical order keeps gossip-sync packet IDs stable across
    /// implementations.
    ///
    /// Rejects an empty batch and one over [`MAX_PREKEYS`], mirroring Swift's
    /// nil-returning guards. It does *not* reject duplicate IDs — Swift's encoder
    /// does not either, and [`decode`](Self::decode) is where that check lives, on
    /// both sides.
    pub fn encode(&self) -> Result<Vec<u8>, NoiseError> {
        if self.prekeys.is_empty() {
            return Err(NoiseError::MalformedBundle("bundle carries no prekeys"));
        }
        if self.prekeys.len() > MAX_PREKEYS {
            return Err(NoiseError::MalformedBundle("bundle exceeds 8 prekeys"));
        }

        let entries_len = self.prekeys.len() * PREKEY_ENTRY_LEN;
        let mut out = Vec::with_capacity(4 * 3 + KEY_LEN + entries_len + 8 + SIGNATURE_LEN);

        out.push(TLV_OWNER_STATIC);
        // Every length below is a compile-time-bounded constant or at most
        // 8 * 36 = 288, so none of these casts can wrap.
        out.extend_from_slice(&(KEY_LEN as u16).to_be_bytes());
        out.extend_from_slice(&self.owner_static);

        out.push(TLV_PREKEYS);
        out.extend_from_slice(&(entries_len as u16).to_be_bytes());
        for prekey in &self.prekeys {
            out.extend_from_slice(&prekey.id.to_be_bytes());
            out.extend_from_slice(&prekey.public_key);
        }

        out.push(TLV_GENERATED_AT);
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&self.generated_at.to_be_bytes());

        out.push(TLV_SIGNATURE);
        out.extend_from_slice(&(SIGNATURE_LEN as u16).to_be_bytes());
        out.extend_from_slice(&self.signature);

        Ok(out)
    }

    /// Parses a gossiped bundle. Never panics on hostile bytes.
    ///
    /// Unknown TLV types are skipped for forward compatibility — that is how a
    /// future bitchat ships a new field to a mixed-version mesh — but all four
    /// known fields must be present. A repeated known type takes its last value,
    /// matching Swift's plain assignment.
    ///
    /// Duplicate prekey IDs are rejected: one consumed ID would otherwise shadow
    /// another, and the sender's "lowest unused ID" assignment would hand out a
    /// key the owner has already retired.
    pub fn decode(bytes: &[u8]) -> Result<Self, NoiseError> {
        let mut cursor = 0usize;
        let mut owner_static: Option<[u8; KEY_LEN]> = None;
        let mut prekeys: Option<Vec<Prekey>> = None;
        let mut generated_at: Option<u64> = None;
        let mut signature: Option<[u8; SIGNATURE_LEN]> = None;

        while cursor < bytes.len() {
            let Some(&tlv_type) = bytes.get(cursor) else {
                return Err(NoiseError::MalformedBundle("truncated TLV type"));
            };
            cursor += 1;

            let Some(&[len_hi, len_lo]) = bytes.get(cursor..cursor + 2) else {
                return Err(NoiseError::MalformedBundle("truncated TLV length"));
            };
            let length = usize::from(u16::from_be_bytes([len_hi, len_lo]));
            cursor += 2;

            let Some(value) = bytes.get(cursor..cursor + length) else {
                return Err(NoiseError::MalformedBundle("TLV value overruns buffer"));
            };
            cursor += length;

            match tlv_type {
                TLV_OWNER_STATIC => {
                    let key: [u8; KEY_LEN] = value.try_into().map_err(|_| {
                        NoiseError::MalformedBundle("owner static key must be 32 bytes")
                    })?;
                    owner_static = Some(key);
                }
                TLV_PREKEYS => {
                    prekeys = Some(decode_prekeys(value)?);
                }
                TLV_GENERATED_AT => {
                    let raw: [u8; 8] = value
                        .try_into()
                        .map_err(|_| NoiseError::MalformedBundle("generatedAt must be 8 bytes"))?;
                    generated_at = Some(u64::from_be_bytes(raw));
                }
                TLV_SIGNATURE => {
                    let sig: [u8; SIGNATURE_LEN] = value
                        .try_into()
                        .map_err(|_| NoiseError::MalformedBundle("signature must be 64 bytes"))?;
                    signature = Some(sig);
                }
                // Unknown TLV: skip for forward compatibility.
                _ => {}
            }
        }

        let (Some(owner_static), Some(prekeys), Some(generated_at), Some(signature)) =
            (owner_static, prekeys, generated_at, signature)
        else {
            return Err(NoiseError::MalformedBundle("bundle is missing a field"));
        };

        Ok(Self {
            owner_static,
            prekeys,
            generated_at,
            signature,
        })
    }

    /// The exact bytes the Ed25519 signature covers.
    ///
    /// ```text
    /// u8(min(len(context), 255)) ‖ context[..255]   context = "bitchat-prekey-bundle-v1"
    /// owner_static                                  32 bytes
    /// u8(min(len(prekeys), 255))
    ///   be_u32(id) ‖ public_key                     32 bytes, repeated, in order
    /// be_u64(generated_at)
    /// ```
    ///
    /// The signature field itself is excluded, which is what lets the owner
    /// compute these bytes before signing.
    ///
    /// Swift truncates or zero-pads each key to 32 bytes here. Our keys are
    /// `[u8; 32]`, so that step is structurally a no-op and is omitted rather than
    /// reproduced — there is no input that could reach it.
    ///
    /// The `take(255)` on the prekey loop likewise mirrors Swift and is
    /// unreachable given [`MAX_PREKEYS`]; it is kept so a caller who hand-builds
    /// an over-long vector still produces bytes that agree with a real peer's
    /// rather than silently diverging.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let context_len = PREKEY_BUNDLE_CONTEXT.len().min(255);
        let counted = self.prekeys.len().min(255);
        let mut out =
            Vec::with_capacity(1 + context_len + KEY_LEN + 1 + counted * PREKEY_ENTRY_LEN + 8);

        // `context_len <= 255` by construction, so the cast cannot wrap.
        out.push(context_len as u8);
        out.extend_from_slice(&PREKEY_BUNDLE_CONTEXT[..context_len]);
        out.extend_from_slice(&self.owner_static);
        out.push(counted as u8);
        for prekey in self.prekeys.iter().take(255) {
            out.extend_from_slice(&prekey.id.to_be_bytes());
            out.extend_from_slice(&prekey.public_key);
        }
        out.extend_from_slice(&self.generated_at.to_be_bytes());

        out
    }

    /// Builds and signs a bundle with our own Ed25519 signing key.
    ///
    /// `prekeys` is used in the order given — see the field docs. `generated_at`
    /// is a parameter rather than a clock read because this crate is sans-IO;
    /// derive it with [`next_generated_at`] so it advances strictly.
    pub fn sign(
        owner_static: [u8; KEY_LEN],
        prekeys: Vec<Prekey>,
        generated_at: u64,
        identity: &MeshIdentity,
    ) -> Self {
        let mut bundle = Self {
            owner_static,
            prekeys,
            generated_at,
            signature: [0u8; SIGNATURE_LEN],
        };
        bundle.signature = sign_bytes(identity, &bundle.signing_bytes());
        bundle
    }

    /// Verifies the bundle against the owner's announce-bound Ed25519 key.
    ///
    /// This proves only that the holder of `signing_public_key` published these
    /// prekeys under this owner static key at this time. Binding that signing key
    /// to `owner_static` is the announce signature's job — a caller that verifies
    /// a bundle without having verified the announce has authenticated nothing.
    pub fn verify(&self, signing_public_key: &[u8; 32]) -> bool {
        verify_bytes(&self.signature, &self.signing_bytes(), signing_public_key)
    }
}

/// Parses the `0x02` value: a whole number of `(id, key)` entries, at most
/// [`MAX_PREKEYS`], with no repeated ID.
fn decode_prekeys(value: &[u8]) -> Result<Vec<Prekey>, NoiseError> {
    if value.is_empty() {
        return Err(NoiseError::MalformedBundle("prekey list is empty"));
    }
    if value.len() % PREKEY_ENTRY_LEN != 0 {
        return Err(NoiseError::MalformedBundle(
            "prekey list is not a whole number of entries",
        ));
    }
    if value.len() / PREKEY_ENTRY_LEN > MAX_PREKEYS {
        return Err(NoiseError::MalformedBundle("prekey list exceeds 8 entries"));
    }

    let mut parsed = Vec::with_capacity(value.len() / PREKEY_ENTRY_LEN);
    for entry in value.chunks_exact(PREKEY_ENTRY_LEN) {
        // `chunks_exact` hands out exactly `PREKEY_ENTRY_LEN` bytes, so both of
        // these are always `Some`; the `else` exists so no bound is assumed.
        let (Some(id_bytes), Some(key_bytes)) =
            (entry.first_chunk::<4>(), entry.last_chunk::<KEY_LEN>())
        else {
            return Err(NoiseError::MalformedBundle("prekey entry is truncated"));
        };
        parsed.push(Prekey {
            id: u32::from_be_bytes(*id_bytes),
            public_key: *key_bytes,
        });
    }

    // At most 8 entries, so the quadratic scan is cheaper than a set.
    for (index, prekey) in parsed.iter().enumerate() {
        if parsed
            .iter()
            .skip(index + 1)
            .any(|other| other.id == prekey.id)
        {
            return Err(NoiseError::MalformedBundle("duplicate prekey ID"));
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_STATIC: [u8; 32] = [0x5A; 32];
    const GENERATED_AT: u64 = 1_700_000_000_123;

    fn identity() -> MeshIdentity {
        MeshIdentity::from_keys([0x11; 32], [0x22; 32])
    }

    fn prekeys(count: u32) -> Vec<Prekey> {
        (0..count)
            .map(|index| Prekey {
                id: index,
                // Distinct, deterministic key material; these are never used for
                // key agreement in this module, only signed over.
                public_key: [(0x40 + index) as u8; 32],
            })
            .collect()
    }

    fn signed_bundle(count: u32) -> PrekeyBundle {
        PrekeyBundle::sign(OWNER_STATIC, prekeys(count), GENERATED_AT, &identity())
    }

    #[test]
    fn a_generated_prekey_opens_what_is_sealed_to_its_public_half() {
        use crate::seal::{open_prekey, seal_prekey};
        let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
        let (prekey, secret) = generate_prekey(9, &mut rng);
        assert_eq!(prekey.id, 9);
        assert_eq!(prekey_public(&secret), prekey.public_key);

        let sender = MeshIdentity::generate(&mut rng);
        let recipient = MeshIdentity::generate(&mut rng);
        let sealed = seal_prekey(b"later", 9, &prekey.public_key, &sender).unwrap();
        let (opened, from) = open_prekey(&sealed, 9, &secret, &recipient).unwrap();
        assert_eq!(opened, b"later");
        assert_eq!(from, sender.static_public());
    }

    // MARK: wire format

    #[test]
    fn encode_decode_round_trips() {
        let bundle = signed_bundle(8);
        let encoded = bundle.encode().expect("a well-formed bundle encodes");
        let decoded = PrekeyBundle::decode(&encoded).expect("our own encoding decodes");
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn encoding_has_the_documented_tlv_layout() {
        let bundle = signed_bundle(2);
        let encoded = bundle.encode().expect("encodes");

        assert_eq!(encoded[0], TLV_OWNER_STATIC);
        assert_eq!(&encoded[1..3], &[0x00, 0x20]);
        assert_eq!(&encoded[3..35], &OWNER_STATIC);

        assert_eq!(encoded[35], TLV_PREKEYS);
        assert_eq!(&encoded[36..38], &[0x00, 72]);
        assert_eq!(&encoded[38..42], &0u32.to_be_bytes());
        assert_eq!(&encoded[42..74], &[0x40u8; 32]);
        assert_eq!(&encoded[74..78], &1u32.to_be_bytes());
        assert_eq!(&encoded[78..110], &[0x41u8; 32]);

        assert_eq!(encoded[110], TLV_GENERATED_AT);
        assert_eq!(&encoded[111..113], &[0x00, 0x08]);
        assert_eq!(&encoded[113..121], &GENERATED_AT.to_be_bytes());

        assert_eq!(encoded[121], TLV_SIGNATURE);
        assert_eq!(&encoded[122..124], &[0x00, 0x40]);
        assert_eq!(&encoded[124..188], &bundle.signature);
        assert_eq!(encoded.len(), 188);
    }

    #[test]
    fn decode_skips_unknown_tlvs() {
        let bundle = signed_bundle(2);
        let mut encoded = bundle.encode().expect("encodes");
        // An unknown TLV 0x7F appended by a future client.
        encoded.extend_from_slice(&[0x7F, 0x00, 0x03, 0x01, 0x02, 0x03]);
        let decoded = PrekeyBundle::decode(&encoded).expect("unknown TLVs are skipped");
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn encode_rejects_invalid_shapes() {
        let mut empty = signed_bundle(1);
        empty.prekeys.clear();
        assert!(empty.encode().is_err(), "an empty batch is not a bundle");

        let mut oversized = signed_bundle(8);
        oversized.prekeys.push(Prekey {
            id: 99,
            public_key: [0xEE; 32],
        });
        assert!(oversized.encode().is_err(), "nine prekeys exceed the cap");
    }

    #[test]
    fn decode_rejects_duplicate_prekey_ids() {
        // Built by hand: the encoder deliberately does not check this, so a peer
        // can send it and our decoder is the thing that must refuse.
        let mut bundle = signed_bundle(2);
        bundle.prekeys[1].id = bundle.prekeys[0].id;
        let encoded = bundle.encode().expect("the encoder permits duplicates");
        assert!(PrekeyBundle::decode(&encoded).is_err());
    }

    #[test]
    fn decode_rejects_missing_fields() {
        let bundle = signed_bundle(3);
        let encoded = bundle.encode().expect("encodes");

        // Owner key TLV alone: everything else is missing.
        assert!(PrekeyBundle::decode(&encoded[..35]).is_err());
        // Everything but the signature.
        assert!(PrekeyBundle::decode(&encoded[..bundle_signature_offset(&bundle)]).is_err());
        // Nothing at all.
        assert!(PrekeyBundle::decode(&[]).is_err());
    }

    fn bundle_signature_offset(bundle: &PrekeyBundle) -> usize {
        3 + KEY_LEN + 3 + bundle.prekeys.len() * PREKEY_ENTRY_LEN + 3 + 8
    }

    #[test]
    fn decode_rejects_bad_field_widths() {
        // 31-byte owner key.
        let mut bytes = vec![TLV_OWNER_STATIC, 0x00, 31];
        bytes.extend_from_slice(&[0xAA; 31]);
        assert!(PrekeyBundle::decode(&bytes).is_err());

        // Prekey list that is not a multiple of the entry length.
        let mut bytes = vec![TLV_PREKEYS, 0x00, 35];
        bytes.extend_from_slice(&[0xBB; 35]);
        assert!(PrekeyBundle::decode(&bytes).is_err());

        // Nine entries.
        let mut bytes = vec![TLV_PREKEYS, 0x01, 0x44];
        bytes.extend_from_slice(&[0xCC; 9 * PREKEY_ENTRY_LEN]);
        assert!(PrekeyBundle::decode(&bytes).is_err());

        // Empty prekey list.
        assert!(PrekeyBundle::decode(&[TLV_PREKEYS, 0x00, 0x00]).is_err());

        // 4-byte generatedAt.
        assert!(PrekeyBundle::decode(&[TLV_GENERATED_AT, 0x00, 0x04, 1, 2, 3, 4]).is_err());

        // 32-byte signature.
        let mut bytes = vec![TLV_SIGNATURE, 0x00, 32];
        bytes.extend_from_slice(&[0xDD; 32]);
        assert!(PrekeyBundle::decode(&bytes).is_err());
    }

    #[test]
    fn decode_never_panics_on_malformed_bytes() {
        let encoded = signed_bundle(4).encode().expect("encodes");

        // Every truncation of a valid encoding.
        for cut in 0..encoded.len() {
            let _ = PrekeyBundle::decode(&encoded[..cut]);
        }

        // Every single-byte corruption of the type and length bytes.
        for index in 0..encoded.len() {
            let mut corrupted = encoded.clone();
            corrupted[index] ^= 0xFF;
            let _ = PrekeyBundle::decode(&corrupted);
        }

        // A declared length far beyond the buffer.
        assert!(PrekeyBundle::decode(&[TLV_PREKEYS, 0xFF, 0xFF, 0x00]).is_err());
        // A type byte with no room for a length.
        assert!(PrekeyBundle::decode(&[TLV_SIGNATURE]).is_err());
        assert!(PrekeyBundle::decode(&[TLV_SIGNATURE, 0x00]).is_err());
        // Pathological repeated zero-length unknown TLVs must terminate.
        let zeros = vec![0x7F; 3 * 400];
        assert!(PrekeyBundle::decode(&zeros).is_err());
    }

    #[test]
    fn decode_takes_the_last_value_of_a_repeated_field() {
        let first = signed_bundle(2);
        let mut second = first.clone();
        second.generated_at = GENERATED_AT + 5;

        let mut bytes = first.encode().expect("encodes");
        bytes.push(TLV_GENERATED_AT);
        bytes.extend_from_slice(&8u16.to_be_bytes());
        bytes.extend_from_slice(&second.generated_at.to_be_bytes());

        let decoded = PrekeyBundle::decode(&bytes).expect("decodes");
        assert_eq!(decoded.generated_at, second.generated_at);
    }

    // MARK: signing bytes

    #[test]
    fn signing_bytes_have_the_documented_layout() {
        let bundle = signed_bundle(2);
        let bytes = bundle.signing_bytes();

        let context_len = PREKEY_BUNDLE_CONTEXT.len();
        assert_eq!(context_len, 24, "\"bitchat-prekey-bundle-v1\" is 24 bytes");
        assert_eq!(bytes[0], context_len as u8);
        assert_eq!(&bytes[1..1 + context_len], PREKEY_BUNDLE_CONTEXT);

        let mut cursor = 1 + context_len;
        assert_eq!(&bytes[cursor..cursor + 32], &OWNER_STATIC);
        cursor += 32;
        assert_eq!(bytes[cursor], 2);
        cursor += 1;
        assert_eq!(&bytes[cursor..cursor + 4], &0u32.to_be_bytes());
        cursor += 4;
        assert_eq!(&bytes[cursor..cursor + 32], &[0x40u8; 32]);
        cursor += 32;
        assert_eq!(&bytes[cursor..cursor + 4], &1u32.to_be_bytes());
        cursor += 4;
        assert_eq!(&bytes[cursor..cursor + 32], &[0x41u8; 32]);
        cursor += 32;
        assert_eq!(&bytes[cursor..], &GENERATED_AT.to_be_bytes());
    }

    #[test]
    fn signing_bytes_exclude_the_signature() {
        let bundle = signed_bundle(3);
        let mut forged = bundle.clone();
        forged.signature = [0xFF; 64];
        assert_eq!(bundle.signing_bytes(), forged.signing_bytes());
    }

    #[test]
    fn signing_bytes_are_prefix_free_across_prekey_counts() {
        // The count prefix is what stops a 3-prekey bundle's bytes from being a
        // prefix of a 4-prekey one.
        let three = signed_bundle(3).signing_bytes();
        let four = signed_bundle(4).signing_bytes();
        assert!(!four.starts_with(&three[..three.len() - 8]));
    }

    // MARK: signature binding

    #[test]
    fn sign_verify_round_trips() {
        let id = identity();
        let bundle = signed_bundle(8);
        assert_eq!(bundle.prekeys.len(), MAX_PREKEYS);
        assert!(bundle.verify(&id.signing_public()));
    }

    #[test]
    fn verification_fails_when_a_prekey_is_substituted() {
        // The attack the signature exists to stop: Mallory swaps in her own prekey
        // so courier mail sealed to it is readable by her.
        let id = identity();
        let mut tampered = signed_bundle(4);
        tampered.prekeys[1].public_key = [0xFF; 32];
        assert!(!tampered.verify(&id.signing_public()));

        // Substituting only the ID is just as fatal.
        let mut renumbered = signed_bundle(4);
        renumbered.prekeys[1].id = 999;
        assert!(!renumbered.verify(&id.signing_public()));
    }

    #[test]
    fn verification_fails_when_a_prekey_is_added_or_removed() {
        let id = identity();

        let mut extended = signed_bundle(4);
        extended.prekeys.push(Prekey {
            id: 4,
            public_key: [0x99; 32],
        });
        assert!(!extended.verify(&id.signing_public()));

        let mut truncated = signed_bundle(4);
        truncated.prekeys.pop();
        assert!(!truncated.verify(&id.signing_public()));

        let mut reordered = signed_bundle(4);
        reordered.prekeys.swap(0, 3);
        assert!(
            !reordered.verify(&id.signing_public()),
            "order is inside the signed bytes"
        );
    }

    #[test]
    fn verification_fails_when_the_owner_or_timestamp_changes() {
        let id = identity();

        let mut reattributed = signed_bundle(3);
        reattributed.owner_static = [0x77; 32];
        assert!(
            !reattributed.verify(&id.signing_public()),
            "a bundle must not be re-attributable to another peer"
        );

        let mut replayed = signed_bundle(3);
        replayed.generated_at = GENERATED_AT + 1;
        assert!(
            !replayed.verify(&id.signing_public()),
            "the generation stamp must be bound"
        );
    }

    #[test]
    fn verification_fails_on_a_forged_signature_or_the_wrong_key() {
        let id = identity();

        let mut forged = signed_bundle(3);
        forged.signature[0] ^= 0x01;
        assert!(!forged.verify(&id.signing_public()));

        let mallory = MeshIdentity::from_keys([0x33; 32], [0x44; 32]);
        assert!(!signed_bundle(3).verify(&mallory.signing_public()));

        // A public key that is not a valid Edwards point must be a clean `false`.
        assert!(!signed_bundle(3).verify(&[0xFF; 32]));
    }

    #[test]
    fn a_verified_bundle_survives_the_wire() {
        let id = identity();
        let encoded = signed_bundle(5).encode().expect("encodes");
        let decoded = PrekeyBundle::decode(&encoded).expect("decodes");
        assert!(decoded.verify(&id.signing_public()));
    }

    // MARK: store policy

    #[test]
    fn replenish_triggers_below_the_threshold() {
        assert!(needs_replenish(0));
        assert!(needs_replenish(2));
        assert!(!needs_replenish(3));
        assert!(!needs_replenish(MAX_PREKEYS));
    }

    #[test]
    fn consumed_prekeys_open_only_inside_the_grace_window() {
        let now = 10_000_000_000u64;
        assert!(prekey_can_open(None, now), "unconsumed always opens");
        assert!(prekey_can_open(Some(now), now));
        assert!(prekey_can_open(Some(now - CONSUMED_GRACE_MS), now));
        assert!(!prekey_can_open(Some(now - CONSUMED_GRACE_MS - 1), now));
        // Clock skew: a stamp in the future reads as just-consumed, not expired.
        assert!(prekey_can_open(Some(now + 60_000), now));
    }

    #[test]
    fn expiry_uses_the_grace_window_when_consumed_and_retention_when_not() {
        let now = 10_000_000_000u64;
        assert!(!prekey_is_expired(now, None, now));
        assert!(!prekey_is_expired(now - UNCONSUMED_RETENTION_MS, None, now));
        assert!(prekey_is_expired(
            now - UNCONSUMED_RETENTION_MS - 1,
            None,
            now
        ));

        // Once consumed, only the grace window matters — age is irrelevant.
        let ancient = now - UNCONSUMED_RETENTION_MS - 1;
        assert!(!prekey_is_expired(ancient, Some(now), now));
        assert!(prekey_is_expired(
            ancient,
            Some(now - CONSUMED_GRACE_MS - 1),
            now
        ));
    }

    #[test]
    fn bundles_stop_being_sealable_after_a_week() {
        let now = 10_000_000_000u64;
        assert!(bundle_is_sealable(now, now));
        assert!(bundle_is_sealable(now - MAX_BUNDLE_AGE_FOR_SEALING_MS, now));
        assert!(!bundle_is_sealable(
            now - MAX_BUNDLE_AGE_FOR_SEALING_MS - 1,
            now
        ));
        assert!(
            bundle_is_sealable(now + 60_000, now),
            "future stamps are fresh"
        );
    }

    #[test]
    fn only_a_strictly_newer_bundle_supersedes() {
        assert!(bundle_supersedes(11, 10));
        assert!(!bundle_supersedes(10, 10));
        assert!(!bundle_supersedes(9, 10));
    }

    #[test]
    fn generated_at_advances_strictly_even_within_one_millisecond() {
        let now = 1_700_000_000_000u64;
        assert_eq!(next_generated_at(0, now), now);
        // Two changes in the same millisecond still produce increasing stamps.
        let first = next_generated_at(0, now);
        let second = next_generated_at(first, now);
        assert!(second > first);
        // A stamp ahead of the clock never regresses.
        assert_eq!(next_generated_at(now + 100, now), now + 101);
    }
}
