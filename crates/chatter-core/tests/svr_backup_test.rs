//! Tests for SVR backup encryption and decryption.
//!
//! Covers round-trip, error cases, and backup ID derivation.

use chatter_core::crypto::svr_backup::{
    decrypt_backup, derive_backup_id, encrypt_backup, KeyBackupPayload,
};

/// Helper: create a deterministic test payload.
fn test_payload() -> KeyBackupPayload {
    let mut identity_keypair = [0u8; 64];
    let mut profile_key = [0u8; 32];

    // Fill with recognizable patterns
    for (i, byte) in identity_keypair.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_add(0x10);
    }
    for (i, byte) in profile_key.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_add(0xA0);
    }

    KeyBackupPayload {
        identity_keypair,
        profile_key,
    }
}

/// Helper: create a deterministic SVR secret.
fn test_svr_secret() -> [u8; 32] {
    let mut secret = [0u8; 32];
    for (i, byte) in secret.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(7).wrapping_add(0x42);
    }
    secret
}

// ============================================================================
// Round-trip Tests
// ============================================================================

#[test]
fn round_trip() {
    let payload = test_payload();
    let svr_secret = test_svr_secret();

    let encrypted = encrypt_backup(&payload, &svr_secret).expect("encryption should succeed");
    let decrypted = decrypt_backup(&encrypted, &svr_secret).expect("decryption should succeed");

    assert_eq!(
        decrypted.identity_keypair, payload.identity_keypair,
        "identity keypair should survive round-trip"
    );
    assert_eq!(
        decrypted.profile_key, payload.profile_key,
        "profile key should survive round-trip"
    );
}

// ============================================================================
// Error Cases
// ============================================================================

#[test]
fn wrong_key() {
    let payload = test_payload();
    let svr_secret = test_svr_secret();

    let encrypted = encrypt_backup(&payload, &svr_secret).expect("encryption should succeed");

    // Decrypt with a different key
    let wrong_secret = [0xFFu8; 32];
    let result = decrypt_backup(&encrypted, &wrong_secret);

    assert!(result.is_err(), "decryption with wrong key should fail");
}

#[test]
fn tampered_ciphertext() {
    let payload = test_payload();
    let svr_secret = test_svr_secret();

    let mut encrypted = encrypt_backup(&payload, &svr_secret).expect("encryption should succeed");

    // Flip a byte in the ciphertext portion (after the 12-byte nonce)
    if encrypted.len() > 14 {
        encrypted[14] ^= 0xFF;
    }

    let result = decrypt_backup(&encrypted, &svr_secret);
    assert!(
        result.is_err(),
        "decryption of tampered ciphertext should fail"
    );
}

#[test]
fn malformed_data() {
    // Truncated bytes should fail to deserialize
    let truncated = vec![0x01, 0x40, 0x00]; // version + partial length
    let result = KeyBackupPayload::deserialize(&truncated);
    assert!(result.is_err(), "truncated data should fail to deserialize");
}

#[test]
fn version_mismatch() {
    let payload = test_payload();
    let mut serialized = payload.serialize();

    // Change version byte to something invalid
    serialized[0] = 0xFF;

    let result = KeyBackupPayload::deserialize(&serialized);
    assert!(
        result.is_err(),
        "wrong version byte should fail to deserialize"
    );
}

// ============================================================================
// Serialization Tests
// ============================================================================

#[test]
fn serialize_deserialize_round_trip() {
    let payload = test_payload();
    let serialized = payload.serialize();
    let deserialized =
        KeyBackupPayload::deserialize(&serialized).expect("deserialization should succeed");

    assert_eq!(deserialized.identity_keypair, payload.identity_keypair);
    assert_eq!(deserialized.profile_key, payload.profile_key);
}

// ============================================================================
// Backup ID Derivation Tests
// ============================================================================

#[test]
fn derive_backup_id_deterministic() {
    let svr_secret = test_svr_secret();

    let id1 = derive_backup_id(&svr_secret);
    let id2 = derive_backup_id(&svr_secret);

    assert_eq!(id1, id2, "same input should produce same backup ID");
}

#[test]
fn derive_backup_id_unique() {
    let secret1 = test_svr_secret();
    let mut secret2 = [0u8; 32];
    secret2.fill(0xBB);

    let id1 = derive_backup_id(&secret1);
    let id2 = derive_backup_id(&secret2);

    assert_ne!(
        id1, id2,
        "different secrets should produce different backup IDs"
    );
}
