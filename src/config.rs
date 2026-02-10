//! Application configuration.
//!
//! Contains hardcoded security-critical values that must be baked into the binary.

use libsignal_protocol::PublicKey;

/// Server trust root public key for sealed sender certificate verification.
///
/// SECURITY: This key is hardcoded following Signal's security model.
/// Like Signal-Desktop's `TRUST_ROOT`, this prevents MITM attacks during
/// registration by ensuring clients only trust certificates signed by this key.
///
/// This is the Curve25519 public key (Montgomery u-coordinate) corresponding
/// to the server's XEdDSA signing key stored in Vault.
///
/// WARNING: For development only! Production builds MUST use a different key
/// that is generated securely and kept secret.
///
/// Development key (matches docker-compose vault-init):
/// - Base64: a7jXrO8LR6JJzjhB5L9ckw34SLmsf9v08N5CjV4xMBE=
/// - Hex: 6bb8d7acef0b47a249ce3841e4bf5c930df848b9ac7fdbf4f0de428d5e313011
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [
    0x6b, 0xb8, 0xd7, 0xac, 0xef, 0x0b, 0x47, 0xa2,
    0x49, 0xce, 0x38, 0x41, 0xe4, 0xbf, 0x5c, 0x93,
    0x0d, 0xf8, 0x48, 0xb9, 0xac, 0x7f, 0xdb, 0xf4,
    0xf0, 0xde, 0x42, 0x8d, 0x5e, 0x31, 0x30, 0x11,
];

/// Trust root key in libsignal's 33-byte format (0x05 prefix + 32-byte key).
/// The 0x05 byte indicates Curve25519 key type.
const TRUST_ROOT_PUBLIC_KEY_SERIALIZED: [u8; 33] = [
    0x05, // Curve25519 key type prefix
    0x6b, 0xb8, 0xd7, 0xac, 0xef, 0x0b, 0x47, 0xa2,
    0x49, 0xce, 0x38, 0x41, 0xe4, 0xbf, 0x5c, 0x93,
    0x0d, 0xf8, 0x48, 0xb9, 0xac, 0x7f, 0xdb, 0xf4,
    0xf0, 0xde, 0x42, 0x8d, 0x5e, 0x31, 0x30, 0x11,
];

lazy_static::lazy_static! {
    /// Parsed trust root public key for use with libsignal.
    pub static ref TRUST_ROOT_PUBLIC_KEY: PublicKey = {
        PublicKey::deserialize(&TRUST_ROOT_PUBLIC_KEY_SERIALIZED)
            .expect("Invalid hardcoded trust root key - this is a build error")
    };
}

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
        let expected_b64 = "a7jXrO8LR6JJzjhB5L9ckw34SLmsf9v08N5CjV4xMBE=";
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
