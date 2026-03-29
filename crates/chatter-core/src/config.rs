//! Application configuration.
//!
//! Contains hardcoded security-critical values that must be baked into the binary.
//!
//! # Trust Root Key Security Model
//!
//! Following Signal's security model, the trust root public key is hardcoded in
//! the binary. This prevents MITM attacks during registration by ensuring clients
//! only trust sender certificates signed by a known server key.
//!
//! - **Debug builds** (`cargo build`): Use the development trust root derived from
//!   a well-known seed. Safe for local development and testing.
//! - **Release builds** (`cargo build --release`): Require a production trust root
//!   to be configured. The binary will panic at startup if no production key is set.
//!
//! To configure a production trust root, replace the placeholder bytes in the
//! `#[cfg(not(debug_assertions))]` section below with your production key.

use libsignal_protocol::PublicKey;

// ─── Development trust root (debug builds only) ────────────────────────────

/// Server trust root public key for sealed sender certificate verification.
///
/// SECURITY: This key is hardcoded following Signal's security model.
/// Like Signal-Desktop's `TRUST_ROOT`, this prevents MITM attacks during
/// registration by ensuring clients only trust certificates signed by this key.
///
/// This is the Curve25519 public key (Montgomery u-coordinate) corresponding
/// to the server's XEdDSA signing key stored in Vault.
///
/// Development key (matches docker-compose vault-init):
/// - Seed: "chatter-dev-sealed-sender-trust-root-v1"
/// - Base64: yknZaqZBfXJLRTHfxsvcjikbUjHKrAiKMJLeUSC+aDM=
/// - Hex: ca49d96aa6417d724b4531dfc6cbdc8e291b5231caac088a3092de5120be6833
#[cfg(debug_assertions)]
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [
    0xca, 0x49, 0xd9, 0x6a, 0xa6, 0x41, 0x7d, 0x72,
    0x4b, 0x45, 0x31, 0xdf, 0xc6, 0xcb, 0xdc, 0x8e,
    0x29, 0x1b, 0x52, 0x31, 0xca, 0xac, 0x08, 0x8a,
    0x30, 0x92, 0xde, 0x51, 0x20, 0xbe, 0x68, 0x33,
];

/// Trust root key in libsignal's 33-byte format (0x05 prefix + 32-byte key).
/// The 0x05 byte indicates Curve25519 key type.
#[cfg(debug_assertions)]
const TRUST_ROOT_PUBLIC_KEY_SERIALIZED: [u8; 33] = [
    0x05, // Curve25519 key type prefix
    0xca, 0x49, 0xd9, 0x6a, 0xa6, 0x41, 0x7d, 0x72,
    0x4b, 0x45, 0x31, 0xdf, 0xc6, 0xcb, 0xdc, 0x8e,
    0x29, 0x1b, 0x52, 0x31, 0xca, 0xac, 0x08, 0x8a,
    0x30, 0x92, 0xde, 0x51, 0x20, 0xbe, 0x68, 0x33,
];

/// Parsed trust root public key for use with libsignal (debug builds).
#[cfg(debug_assertions)]
pub static TRUST_ROOT_PUBLIC_KEY: std::sync::LazyLock<PublicKey> = std::sync::LazyLock::new(|| {
    PublicKey::deserialize(&TRUST_ROOT_PUBLIC_KEY_SERIALIZED)
        .expect("Invalid hardcoded trust root key - this is a build error")
});

// ─── Production trust root (release builds) ────────────────────────────────

/// Production trust root public key bytes.
///
/// SECURITY: This is the production Curve25519 public key (Montgomery u-coordinate).
/// The corresponding private key is stored in Vault at sealed-sender/signing-keys/current.
/// Generated with crypto/rand — NOT derived from a seed.
///
/// Base64: i6I51jfBzxRyPtEFSzJs2rh5VVKNd1oZ+kSinqRL0RM=
/// GRA-249: Replaced dev key that was identical to debug build key.
#[cfg(not(debug_assertions))]
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [
    0x8b, 0xa2, 0x39, 0xd6, 0x37, 0xc1, 0xcf, 0x14,
    0x72, 0x3e, 0xd1, 0x05, 0x4b, 0x32, 0x6c, 0xda,
    0xb8, 0x79, 0x55, 0x52, 0x8d, 0x77, 0x5a, 0x19,
    0xfa, 0x44, 0xa2, 0x9e, 0xa4, 0x4b, 0xd1, 0x13,
];

#[cfg(not(debug_assertions))]
const TRUST_ROOT_PUBLIC_KEY_SERIALIZED: [u8; 33] = [
    0x05, // Curve25519 key type prefix
    0x8b, 0xa2, 0x39, 0xd6, 0x37, 0xc1, 0xcf, 0x14,
    0x72, 0x3e, 0xd1, 0x05, 0x4b, 0x32, 0x6c, 0xda,
    0xb8, 0x79, 0x55, 0x52, 0x8d, 0x77, 0x5a, 0x19,
    0xfa, 0x44, 0xa2, 0x9e, 0xa4, 0x4b, 0xd1, 0x13,
];

/// Compile-time assertion: release key must differ from the development key.
/// Prevents accidentally shipping a release build with the dev trust root,
/// which would allow sender certificate forgery (GRA-249).
#[cfg(not(debug_assertions))]
const _: () = {
    const DEV_KEY: [u8; 32] = [
        0xca, 0x49, 0xd9, 0x6a, 0xa6, 0x41, 0x7d, 0x72,
        0x4b, 0x45, 0x31, 0xdf, 0xc6, 0xcb, 0xdc, 0x8e,
        0x29, 0x1b, 0x52, 0x31, 0xca, 0xac, 0x08, 0x8a,
        0x30, 0x92, 0xde, 0x51, 0x20, 0xbe, 0x68, 0x33,
    ];
    let mut i = 0;
    let mut same = true;
    while i < 32 {
        if TRUST_ROOT_PUBLIC_KEY_BYTES[i] != DEV_KEY[i] {
            same = false;
        }
        i += 1;
    }
    assert!(!same, "Production trust root must differ from development key");
};

/// Parsed trust root public key for use with libsignal (release builds).
#[cfg(not(debug_assertions))]
pub static TRUST_ROOT_PUBLIC_KEY: std::sync::LazyLock<PublicKey> = std::sync::LazyLock::new(|| {
    PublicKey::deserialize(&TRUST_ROOT_PUBLIC_KEY_SERIALIZED)
        .expect("Invalid production trust root key - check config.rs")
});

// ─── Shared constants ──────────────────────────────────────────────────────

/// Trust root key ID (matches server's key_id in Vault).
/// Used for key rotation coordination.
pub const TRUST_ROOT_KEY_ID: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

    #[test]
    fn test_trust_root_key_is_valid() {
        // Verify the key can be deserialized (33-byte format with 0x05 prefix)
        let key = PublicKey::deserialize(&TRUST_ROOT_PUBLIC_KEY_SERIALIZED);
        assert!(key.is_ok(), "Trust root key must be a valid Curve25519 point");
    }

    #[test]
    fn test_trust_root_matches_expected_base64() {
        // Verify our hex bytes match the expected base64 from docker-compose
        // Public key derived from seed: "chatter-dev-sealed-sender-trust-root-v1"
        let expected_b64 = "yknZaqZBfXJLRTHfxsvcjikbUjHKrAiKMJLeUSC+aDM=";
        let actual_b64 = BASE64.encode(TRUST_ROOT_PUBLIC_KEY_BYTES);
        assert_eq!(actual_b64, expected_b64, "Trust root key must match server's key");
    }

    #[test]
    fn test_lazy_static_works() {
        // Access the lazy static to ensure it initializes without panic
        let _ = TRUST_ROOT_PUBLIC_KEY.serialize();
    }

    #[test]
    fn test_serialized_format_is_correct() {
        // Verify the 33-byte format has correct prefix
        assert_eq!(TRUST_ROOT_PUBLIC_KEY_SERIALIZED[0], 0x05, "Key type prefix must be 0x05 (Curve25519)");
        assert_eq!(&TRUST_ROOT_PUBLIC_KEY_SERIALIZED[1..], &TRUST_ROOT_PUBLIC_KEY_BYTES);
    }
}
