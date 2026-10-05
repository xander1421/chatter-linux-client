//! Nostr identities: the keypair, its bech32 forms, and the per-cell derivation
//! that keeps location activity unlinkable.
//!
//! Ported from `bitchat/Nostr/NostrIdentity.swift`,
//! `bitchat/Nostr/NostrIdentityBridge.swift`, and `bitchat/Nostr/Bech32.swift`.
//!
//! # Why identities are derived rather than reused
//!
//! Publishing every geohash message under one key would let any relay operator
//! join a user's cells into a movement trace. Instead each cell gets its own key,
//! derived from a device-local seed that never leaves the device:
//!
//! ```text
//! secret = HMAC-SHA256(key = device_seed, msg = geohash ‖ be32(iteration))
//! ```
//!
//! The derivation is deterministic, so the same device rejoining a cell keeps its
//! identity, and it is one-way, so two cells' pubkeys cannot be tied together
//! without the seed. Mesh-bridge rendezvous identities take the same path under a
//! [`BRIDGE_IDENTITY_LABEL`] prefix, which makes the bridge identity for a cell
//! unlinkable from the chat identity for that same cell.
//!
//! The seed itself is the whole secret: anyone holding it can recompute every
//! identity the device has ever used. Callers must store it in the platform
//! keychain, never on disk in the clear, and wiping it must also drop any cached
//! identities — otherwise post-wipe messages stay signed with pre-wipe keys and
//! remain linkable across the wipe.

use bech32::{Bech32, Hrp};
use hkdf::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::BRIDGE_IDENTITY_LABEL;
use crate::error::NostrError;
use crate::event::public_key_hex;

/// Human-readable part of a bech32-encoded Nostr public key (NIP-19).
const NPUB_HRP: &str = "npub";
/// Human-readable part of a bech32-encoded Nostr secret key (NIP-19).
const NSEC_HRP: &str = "nsec";

/// How many HMAC iterations to try before falling back.
///
/// A 32-byte HMAC output lands outside the secp256k1 scalar range with
/// probability under 2⁻¹²⁷, so the loop effectively never runs twice. It exists
/// because "effectively never" is not "never", and a key derivation that can fail
/// is a key derivation that will fail on someone's device.
const MAX_DERIVE_ITERATIONS: u32 = 10;

/// A secp256k1 keypair used as a Nostr identity.
///
/// The public key is stored as the 32-byte x-only form, hex encoded, because that
/// is what a Nostr event's `pubkey` field carries.
#[derive(Clone)]
pub struct NostrIdentity {
    secret: [u8; 32],
    public_key_hex: String,
}

impl NostrIdentity {
    /// Build an identity from raw secret key bytes.
    ///
    /// Fails when the bytes are not a valid secp256k1 scalar (zero, or at or above
    /// the group order).
    pub fn from_secret(secret: [u8; 32]) -> Result<Self, NostrError> {
        let public_key_hex = public_key_hex(&secret)?;
        Ok(Self {
            secret,
            public_key_hex,
        })
    }

    /// The x-only public key, hex encoded — the `pubkey` field of every event
    /// this identity signs.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// The raw secret key bytes.
    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// NIP-19 `npub1…` form of the public key. Safe to display and to share.
    pub fn npub(&self) -> Result<String, NostrError> {
        npub_encode(&self.public_key_hex)
    }

    /// NIP-19 `nsec1…` form of the secret key.
    ///
    /// This is the private key in a shape that is trivial to paste into another
    /// client — treat it as the secret it is. It exists for key export and backup;
    /// it must never be logged.
    pub fn nsec(&self) -> Result<String, NostrError> {
        encode_bech32(NSEC_HRP, &self.secret)
    }
}

/// Redacts the secret. An identity's whole value is that its private half stays
/// private, and `{:?}` on a struct is the classic way that stops being true.
impl std::fmt::Debug for NostrIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NostrIdentity")
            .field("public_key_hex", &self.public_key_hex)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Derive the deterministic, per-cell identity for a geohash.
///
/// Same seed and same geohash always yield the same identity; different geohashes
/// yield identities that cannot be linked without the seed.
pub fn derive_for_geohash(
    device_seed: &[u8; 32],
    geohash: &str,
) -> Result<NostrIdentity, NostrError> {
    for iteration in 0..MAX_DERIVE_ITERATIONS {
        let candidate = hmac_candidate(device_seed, geohash.as_bytes(), iteration)?;
        if let Ok(identity) = NostrIdentity::from_secret(candidate) {
            return Ok(identity);
        }
    }

    // Every HMAC candidate was off-curve. Unreachable in practice; still has to
    // produce a key rather than an error, so that joining a cell cannot fail.
    let mut hasher = Sha256::new();
    hasher.update(device_seed);
    hasher.update(geohash.as_bytes());
    NostrIdentity::from_secret(hasher.finalize().into())
}

/// Derive the identity for a mesh-bridge rendezvous cell.
///
/// The [`BRIDGE_IDENTITY_LABEL`] prefix is the entire point: bridging and
/// chatting in the same cell would otherwise share a pubkey and reveal that the
/// two roles are the same device.
pub fn derive_for_bridge(device_seed: &[u8; 32], cell: &str) -> Result<NostrIdentity, NostrError> {
    let mut label = String::with_capacity(BRIDGE_IDENTITY_LABEL.len() + cell.len());
    label.push_str(BRIDGE_IDENTITY_LABEL);
    label.push_str(cell);
    derive_for_geohash(device_seed, &label)
}

/// One HMAC candidate: `HMAC-SHA256(seed, msg ‖ big-endian iteration)`.
fn hmac_candidate(
    device_seed: &[u8; 32],
    msg: &[u8],
    iteration: u32,
) -> Result<[u8; 32], NostrError> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(device_seed).map_err(|_| NostrError::InvalidPrivateKey)?;
    mac.update(msg);
    mac.update(&iteration.to_be_bytes());
    Ok(mac.finalize().into_bytes().into())
}

/// Encode an x-only public key hex string as NIP-19 `npub1…`.
///
/// Validates length and hex only — a well-formed 32-byte value that is not a
/// point on the curve still encodes, exactly as bitchat's encoder does. Curve
/// validity is checked where the key is used, not where it is displayed.
pub fn npub_encode(pubkey_hex: &str) -> Result<String, NostrError> {
    let bytes = hex::decode(pubkey_hex).map_err(|_| NostrError::InvalidPublicKey)?;
    if bytes.len() != 32 {
        return Err(NostrError::InvalidPublicKey);
    }
    encode_bech32(NPUB_HRP, &bytes)
}

/// Decode a NIP-19 `npub1…` back to an x-only public key hex string.
pub fn npub_decode(npub: &str) -> Result<String, NostrError> {
    let bytes = decode_bech32(NPUB_HRP, npub)?;
    if bytes.len() != 32 {
        return Err(NostrError::InvalidPublicKey);
    }
    Ok(hex::encode(bytes))
}

fn encode_bech32(hrp: &str, data: &[u8]) -> Result<String, NostrError> {
    let hrp = Hrp::parse(hrp).map_err(|e| NostrError::Bech32(e.to_string()))?;
    bech32::encode::<Bech32>(hrp, data).map_err(|e| NostrError::Bech32(e.to_string()))
}

fn decode_bech32(expected_hrp: &str, encoded: &str) -> Result<Vec<u8>, NostrError> {
    let (hrp, data) = bech32::decode(encoded).map_err(|e| NostrError::Bech32(e.to_string()))?;
    if !hrp.to_string().eq_ignore_ascii_case(expected_hrp) {
        return Err(NostrError::Bech32(format!(
            "expected a {expected_hrp} but got a {hrp}"
        )));
    }

    // `bech32::decode` accepts either checksum variant. Nostr's NIP-19 strings are
    // always plain bech32, so re-encoding and comparing pins the variant — a
    // bech32m string with an `npub` prefix is not an npub.
    let reencoded = encode_bech32(expected_hrp, &data)?;
    if !reencoded.eq_ignore_ascii_case(encoded) {
        return Err(NostrError::Bech32(
            "checksum is bech32m, not bech32".to_string(),
        ));
    }

    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_A: [u8; 32] = [7u8; 32];
    const SEED_B: [u8; 32] = [9u8; 32];

    /// NIP-19 reference vector.
    const NIP19_PUBKEY_HEX: &str =
        "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
    const NIP19_NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    /// NIP-19 reference vector.
    const NIP19_SECKEY_HEX: &str =
        "67dea2ed018072d675f5415ecfaed7d2597555e202d85b3d65ea4e58d2d92ffa";
    const NIP19_NSEC: &str = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";

    fn seckey(hex_str: &str) -> [u8; 32] {
        let bytes = hex::decode(hex_str).expect("valid hex");
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    #[test]
    fn identity_exposes_a_32_byte_x_only_pubkey() {
        let identity = NostrIdentity::from_secret(seckey(NIP19_SECKEY_HEX)).expect("valid secret");
        assert_eq!(identity.public_key_hex().len(), 64);
        assert!(
            identity
                .public_key_hex()
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert_eq!(identity.secret(), &seckey(NIP19_SECKEY_HEX));
    }

    #[test]
    fn invalid_secrets_are_rejected() {
        // Zero is not a valid scalar.
        assert!(NostrIdentity::from_secret([0u8; 32]).is_err());
        // All-ones is above the group order.
        assert!(NostrIdentity::from_secret([0xffu8; 32]).is_err());
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let identity = NostrIdentity::from_secret(seckey(NIP19_SECKEY_HEX)).expect("valid secret");
        let rendered = format!("{identity:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(NIP19_SECKEY_HEX));
        assert!(!rendered.contains("67dea2ed"));
    }

    #[test]
    fn nip19_npub_vector() {
        assert_eq!(npub_encode(NIP19_PUBKEY_HEX).expect("encodes"), NIP19_NPUB);
        assert_eq!(npub_decode(NIP19_NPUB).expect("decodes"), NIP19_PUBKEY_HEX);
    }

    #[test]
    fn nip19_nsec_vector() {
        let identity = NostrIdentity::from_secret(seckey(NIP19_SECKEY_HEX)).expect("valid secret");
        assert_eq!(identity.nsec().expect("encodes"), NIP19_NSEC);
    }

    #[test]
    fn npub_and_nsec_round_trip() {
        let identity = derive_for_geohash(&SEED_A, "u4pru").expect("derives");

        let npub = identity.npub().expect("npub");
        assert!(npub.starts_with("npub1"));
        assert_eq!(
            npub_decode(&npub).expect("decodes"),
            identity.public_key_hex()
        );

        let nsec = identity.nsec().expect("nsec");
        assert!(nsec.starts_with("nsec1"));
        let secret = decode_bech32(NSEC_HRP, &nsec).expect("decodes");
        assert_eq!(secret, identity.secret());
    }

    #[test]
    fn npub_rejects_malformed_input() {
        assert!(npub_encode("not hex").is_err());
        assert!(npub_encode("abcd").is_err()); // valid hex, wrong length
        assert!(npub_decode("not a bech32 string").is_err());
        // Right shape, wrong prefix.
        assert!(npub_decode(NIP19_NSEC).is_err());
        // Corrupted checksum.
        let mut corrupted = NIP19_NPUB.to_string();
        corrupted.pop();
        corrupted.push(if NIP19_NPUB.ends_with('q') { 'p' } else { 'q' });
        assert!(npub_decode(&corrupted).is_err());
    }

    #[test]
    fn derivation_is_deterministic_for_a_seed_and_geohash() {
        let first = derive_for_geohash(&SEED_A, "u4pruy").expect("derives");
        let second = derive_for_geohash(&SEED_A, "u4pruy").expect("derives");
        assert_eq!(first.public_key_hex(), second.public_key_hex());
        assert_eq!(first.secret(), second.secret());
    }

    #[test]
    fn different_geohashes_give_unlinkable_identities() {
        let a = derive_for_geohash(&SEED_A, "u4pruy").expect("derives");
        let b = derive_for_geohash(&SEED_A, "u4pruz").expect("derives");
        let c = derive_for_geohash(&SEED_A, "u4pru").expect("derives");
        assert_ne!(a.public_key_hex(), b.public_key_hex());
        assert_ne!(a.public_key_hex(), c.public_key_hex());
        assert_ne!(b.public_key_hex(), c.public_key_hex());
    }

    #[test]
    fn different_seeds_give_different_identities_for_the_same_cell() {
        let a = derive_for_geohash(&SEED_A, "u4pruy").expect("derives");
        let b = derive_for_geohash(&SEED_B, "u4pruy").expect("derives");
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn a_bridge_identity_is_not_the_chat_identity_for_the_same_cell() {
        let chat = derive_for_geohash(&SEED_A, "u4pruy").expect("derives");
        let bridge = derive_for_bridge(&SEED_A, "u4pruy").expect("derives");
        assert_ne!(chat.public_key_hex(), bridge.public_key_hex());

        // And it is exactly the labelled derivation, not a separate scheme.
        let labelled = derive_for_geohash(&SEED_A, &format!("{BRIDGE_IDENTITY_LABEL}u4pruy"))
            .expect("derives");
        assert_eq!(bridge.public_key_hex(), labelled.public_key_hex());
    }

    #[test]
    fn bridge_derivation_is_deterministic() {
        let a = derive_for_bridge(&SEED_A, "u4pru").expect("derives");
        let b = derive_for_bridge(&SEED_A, "u4pru").expect("derives");
        assert_eq!(a.secret(), b.secret());
        assert_ne!(
            a.public_key_hex(),
            derive_for_bridge(&SEED_A, "u4prv")
                .expect("derives")
                .public_key_hex()
        );
    }

    /// Pins the derivation to `HMAC-SHA256(seed, geohash ‖ be32(0))` — the exact
    /// input bitchat uses. A change here silently rotates every user's per-cell
    /// pubkey and breaks their presence continuity, so it is worth catching.
    #[test]
    fn derivation_matches_the_hmac_construction() {
        let expected = hmac_candidate(&SEED_A, b"u4pru", 0).expect("hmac");
        let identity = derive_for_geohash(&SEED_A, "u4pru").expect("derives");
        assert_eq!(identity.secret(), &expected);
    }

    #[test]
    fn derivation_accepts_a_non_geohash_label() {
        // The derivation is over arbitrary bytes; bridge labels contain a `|` and
        // cells are not always geohashes.
        assert!(derive_for_geohash(&SEED_A, "").is_ok());
        assert!(derive_for_geohash(&SEED_A, "bridge|whatever").is_ok());
        assert!(derive_for_bridge(&SEED_A, "").is_ok());
    }
}
