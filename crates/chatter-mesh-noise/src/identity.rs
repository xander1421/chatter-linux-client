//! Long-term device identity: one Curve25519 key for Noise key agreement, one
//! Ed25519 key for signatures.
//!
//! Ported from `Services/NoiseEncryptionService.swift`.
//!
//! Key *custody* is deliberately not here. Every constructor takes raw secret bytes,
//! so this type is usable with the fixed keys the official Noise vectors require, and
//! the OS-keychain binding lands in P6 where `chatter-core` already has `keyring`.
//!
//! The two keys are separate on purpose: reusing one keypair across a key-agreement
//! protocol and a signature scheme is a cross-protocol footgun, and bitchat keeps
//! them apart. Do not "simplify" them into one.

use sha2::{Digest, Sha256};

/// Curve25519 static key plus Ed25519 signing key.
///
/// Secrets are held as raw bytes because `snow` wants `&[u8]` and both dalek crates
/// reconstruct cheaply from a 32-byte array. Callers that need zeroization should
/// wrap this; the mesh layer never persists it.
#[derive(Clone)]
pub struct MeshIdentity {
    static_secret: [u8; 32],
    signing_secret: [u8; 32],
    static_public: [u8; 32],
    signing_public: [u8; 32],
}

impl MeshIdentity {
    /// Builds an identity from raw secret material.
    pub fn from_keys(static_secret: [u8; 32], signing_secret: [u8; 32]) -> Self {
        let static_public =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(static_secret))
                .to_bytes();
        let signing_public = ed25519_dalek::SigningKey::from_bytes(&signing_secret)
            .verifying_key()
            .to_bytes();

        Self {
            static_secret,
            signing_secret,
            static_public,
            signing_public,
        }
    }

    /// Generates a fresh identity.
    pub fn generate(rng: &mut impl rand_core::CryptoRng) -> Self {
        let mut static_secret = [0u8; 32];
        let mut signing_secret = [0u8; 32];
        rng.fill_bytes(&mut static_secret);
        rng.fill_bytes(&mut signing_secret);
        Self::from_keys(static_secret, signing_secret)
    }

    /// Raw Curve25519 secret, for handing to `snow`.
    pub fn static_secret(&self) -> &[u8; 32] {
        &self.static_secret
    }

    /// Raw Ed25519 secret.
    pub fn signing_secret(&self) -> &[u8; 32] {
        &self.signing_secret
    }

    /// Curve25519 public key — the peer's cryptographic identity on the mesh.
    pub fn static_public(&self) -> [u8; 32] {
        self.static_public
    }

    /// Ed25519 public key, announced so peers can verify our signatures.
    pub fn signing_public(&self) -> [u8; 32] {
        self.signing_public
    }

    /// SHA-256 of the static public key, lowercase hex. Stable across sessions and
    /// across peer-ID rotation, which is what makes it usable for verification.
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.static_public)
    }

    /// The short routing ID derived from the static key.
    pub fn peer_id(&self) -> chatter_mesh::ids::PeerId {
        chatter_mesh::ids::PeerId::from_noise_key(&self.static_public)
    }
}

impl core::fmt::Debug for MeshIdentity {
    /// Prints the fingerprint only. Secrets must never reach a log line, and a
    /// derived `Debug` would put both of them there.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MeshIdentity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

/// SHA-256 of a static public key, lowercase hex (64 chars).
pub fn fingerprint_of(static_public: &[u8; 32]) -> String {
    let digest = Sha256::digest(static_public);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(HEX[(byte >> 4) as usize]);
        out.push_str(HEX[(byte & 0x0F) as usize]);
    }
    out
}

const HEX: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "a", "b", "c", "d", "e", "f",
];

#[cfg(test)]
mod tests {
    use super::*;

    const STATIC_SECRET: [u8; 32] = [7u8; 32];
    const SIGNING_SECRET: [u8; 32] = [9u8; 32];

    #[test]
    fn public_keys_are_derived_deterministically() {
        let a = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        let b = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        assert_eq!(a.static_public(), b.static_public());
        assert_eq!(a.signing_public(), b.signing_public());
    }

    #[test]
    fn the_two_keys_are_independent() {
        let identity = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        // Same secret bytes must not yield the same public key across the two
        // schemes — if they ever do, the keys have been conflated somewhere.
        let same_bytes = MeshIdentity::from_keys(STATIC_SECRET, STATIC_SECRET);
        assert_ne!(same_bytes.static_public(), same_bytes.signing_public());
        assert_eq!(identity.static_public(), same_bytes.static_public());
    }

    #[test]
    fn fingerprint_is_sha256_hex_of_the_static_key() {
        let identity = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        let fingerprint = identity.fingerprint();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(fingerprint.chars().all(|c| !c.is_ascii_uppercase()));

        let expected = Sha256::digest(identity.static_public());
        assert_eq!(fingerprint, hex_lower(&expected));
    }

    #[test]
    fn debug_never_leaks_secret_material() {
        let identity = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        let rendered = format!("{identity:?}");
        assert!(rendered.contains(&identity.fingerprint()));
        assert!(!rendered.contains("secret"));
        // The literal secret bytes must not appear in any form.
        assert!(!rendered.contains(&hex_lower(&STATIC_SECRET)));
        assert!(!rendered.contains(&hex_lower(&SIGNING_SECRET)));
    }

    #[test]
    fn peer_id_matches_the_fingerprint_prefix() {
        let identity = MeshIdentity::from_keys(STATIC_SECRET, SIGNING_SECRET);
        // PeerID is the first 16 hex chars of the same SHA-256.
        assert!(
            identity
                .fingerprint()
                .starts_with(identity.peer_id().bare())
        );
    }

    fn hex_lower(bytes: &[u8]) -> String {
        let mut out = String::new();
        for byte in bytes {
            out.push_str(HEX[(byte >> 4) as usize]);
            out.push_str(HEX[(byte & 0x0F) as usize]);
        }
        out
    }
}
