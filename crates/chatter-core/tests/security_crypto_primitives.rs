//! Security tests for cryptographic primitives.
//!
//! Validates the correctness and tamper resistance of:
//!   - Identity-key HPKE (anonymous DM initial contact, section 7)
//!   - Envelope crypto (NDSS'21 mailbox encryption, section 7)
//!   - Profile encryption (AES-256-GCM with padding, section 6)
//!   - DM message crypto edge cases (XChaCha20-Poly1305, section 3)

use chatter_core::crypto::{
    // DM message crypto
    encrypt_dm_bytes, decrypt_dm_bytes, decrypt_dm_message,
    // Attachment crypto (padding)
    AttachmentCrypto,
};
use chatter_core::crypto::profile_crypto;
use chatter_core::crypto::ecies::{open_with_identity, seal_to_identity, IdentityHpkeUse};
use chatter_core::crypto::envelope_crypto;
use libsignal_protocol::IdentityKeyPair;

fn identity() -> IdentityKeyPair {
    use rand::TryRngCore;
    IdentityKeyPair::generate(&mut rand::rngs::OsRng.unwrap_err())
}

// ===========================================================================
// Identity-key HPKE — Anonymous DM Initial Contact (libsignal HPKE, RFC 9180)
// ===========================================================================

/// Anon-DM first contact round-trip: sealed to the recipient's Signal identity
/// key, opened with its identity key pair.
///
/// Architecture ref (section 7): Initiator encrypts intro_msg with responder_identity_pk.
#[test]
fn test_anon_first_contact_roundtrip() {
    let recipient = identity();
    let plaintext = b"init || pk_e || identity || delivery token";
    let sealed = seal_to_identity(recipient.identity_key(), IdentityHpkeUse::AnonDmInit, plaintext)
        .expect("seal");
    let opened = open_with_identity(&recipient, IdentityHpkeUse::AnonDmInit, &sealed).expect("open");
    assert_eq!(opened, plaintext);
}

/// Only the holder of the recipient identity key can open it.
#[test]
fn test_anon_first_contact_wrong_identity_fails() {
    let sealed = seal_to_identity(identity().identity_key(), IdentityHpkeUse::AnonDmInit, b"secret").unwrap();
    assert!(open_with_identity(&identity(), IdentityHpkeUse::AnonDmInit, &sealed).is_err());
}

/// Tampering with the ephemeral key or the ciphertext is detected.
#[test]
fn test_anon_first_contact_tamper_detected() {
    let recipient = identity();
    let sealed = seal_to_identity(recipient.identity_key(), IdentityHpkeUse::AnonDmInit, b"important data").unwrap();
    for i in [1, sealed.len() - 1] {
        let mut t = sealed.clone();
        t[i] ^= 0xFF;
        assert!(open_with_identity(&recipient, IdentityHpkeUse::AnonDmInit, &t).is_err(), "flip at {i}");
    }
}

// ===========================================================================
// Envelope crypto — NDSS'21 Mailbox Encryption (libsignal HPKE, RFC 9180)
// ===========================================================================

/// Envelope encryption roundtrip to a mailbox's separate X25519 encryption key.
///
/// Security property: RFC 9180 HPKE (libsignal, X25519/HKDF-SHA256/AES-256-GCM) provides
///   confidentiality and integrity per message. The sender half of the ECDH is a
///   fresh per-envelope ephemeral key, so the envelope carries no long-term
///   sender identifier.
#[test]
fn test_envelope_encrypt_decrypt_roundtrip() {
    let (private, public) = envelope_crypto::generate_mailbox_encryption_key();
    let plaintext = b"Envelope encrypted message for NDSS'21 mailbox";
    let envelope = envelope_crypto::encrypt_envelope(plaintext, &public)
        .expect("Envelope encryption should succeed");
    let decrypted = envelope_crypto::decrypt_envelope(&envelope, &private)
        .expect("Envelope decryption should succeed");
    assert_eq!(decrypted, plaintext, "Decrypted should match original");
}

/// The mailbox's Ed25519 signing key alone cannot open its envelopes.
#[test]
fn test_envelope_signing_key_cannot_open() {
    let signing = ed25519_dalek::SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let (_, public) = envelope_crypto::generate_mailbox_encryption_key();
    let envelope = envelope_crypto::encrypt_envelope(b"secret", &public).unwrap();
    assert!(envelope_crypto::decrypt_envelope(&envelope, &signing.to_bytes()).is_err());
}

/// Envelope: wrong recipient key cannot decrypt.
#[test]
fn test_envelope_wrong_recipient_fails() {
    let (_, public) = envelope_crypto::generate_mailbox_encryption_key();
    let (wrong_private, _) = envelope_crypto::generate_mailbox_encryption_key();
    let envelope = envelope_crypto::encrypt_envelope(b"secret", &public).unwrap();
    let result = envelope_crypto::decrypt_envelope(&envelope, &wrong_private);
    assert!(result.is_err(), "Wrong recipient key must fail decryption");
}

/// Envelope: tampered ciphertext detected.
#[test]
fn test_envelope_tampered_ciphertext_detected() {
    let (private, public) = envelope_crypto::generate_mailbox_encryption_key();
    let mut envelope = envelope_crypto::encrypt_envelope(b"tamper test", &public).unwrap();
    if let Some(byte) = envelope.last_mut() {
        *byte ^= 0xFF;
    }
    let result = envelope_crypto::decrypt_envelope(&envelope, &private);
    assert!(result.is_err(), "Tampered envelope must be rejected");
}

// ===========================================================================
// Profile Encryption (AES-256-GCM with zero-padding)
// ===========================================================================

/// Profile display name encrypt/decrypt roundtrip.
///
/// Security property: Profile fields are encrypted per-user with their profile key.
///   Zero-padding to fixed sizes prevents length-based inference.
#[test]
fn test_profile_display_name_roundtrip() {
    let profile_key = [42u8; 32];
    let name = "Alice Johnson";

    let encrypted = profile_crypto::encrypt_display_name(&profile_key, name)
        .expect("Encrypt display name should succeed");

    assert_ne!(encrypted.as_slice(), name.as_bytes(), "Encrypted should differ from plaintext");

    let decrypted = profile_crypto::decrypt_display_name(&profile_key, &encrypted)
        .expect("Decrypt display name should succeed");

    assert_eq!(decrypted, name);
}

/// Profile encryption: wrong key cannot decrypt.
#[test]
fn test_profile_wrong_key_fails() {
    let key1 = [1u8; 32];
    let key2 = [2u8; 32];

    let encrypted = profile_crypto::encrypt_display_name(&key1, "Secret Name").unwrap();
    let result = profile_crypto::decrypt_display_name(&key2, &encrypted);
    assert!(result.is_err(), "Wrong profile key must fail decryption");
}

/// Profile encryption: padding normalizes ciphertext size.
///
/// Security property (NDSS'21): Different-length names produce same-size ciphertext,
///   preventing the server from inferring name length.
#[test]
fn test_profile_padding_normalizes_size() {
    let key = [99u8; 32];

    let short_enc = profile_crypto::encrypt_display_name(&key, "A").unwrap();
    let long_enc = profile_crypto::encrypt_display_name(&key, "A Very Long Display Name That Is Much Longer").unwrap();

    // Both names fit the 53-byte short-name bucket, so ciphertexts have
    // identical length; only their lengths within the bucket are hidden.
    let size_diff = (short_enc.len() as i64 - long_enc.len() as i64).unsigned_abs();
    assert!(size_diff == 0, "Padded ciphertext sizes should be identical, got diff={}", size_diff);
}

/// Profile status encrypt/decrypt roundtrip.
#[test]
fn test_profile_status_roundtrip() {
    let profile_key = [55u8; 32];
    let status = "Available for chat";

    let encrypted = profile_crypto::encrypt_status(&profile_key, status).unwrap();
    let decrypted = profile_crypto::decrypt_status(&profile_key, &encrypted).unwrap();
    assert_eq!(decrypted, status);
}

/// Profile avatar encrypt/decrypt roundtrip (binary data, no padding).
#[test]
fn test_profile_avatar_roundtrip() {
    let profile_key = [77u8; 32];
    let avatar = vec![0xFFu8; 1024]; // 1KB fake avatar

    let encrypted = profile_crypto::encrypt_avatar(&profile_key, &avatar).unwrap();
    assert_ne!(encrypted, avatar);

    let decrypted = profile_crypto::decrypt_avatar(&profile_key, &encrypted).unwrap();
    assert_eq!(decrypted, avatar);
}

/// Profile: unicode names are handled correctly.
#[test]
fn test_profile_unicode_name() {
    let key = [88u8; 32];

    for name in ["", "日本語テスト", "Ёмаё", "🔐🔑💬"] {
        let encrypted = profile_crypto::encrypt_display_name(&key, name).unwrap();
        let decrypted = profile_crypto::decrypt_display_name(&key, &encrypted).unwrap();
        assert_eq!(decrypted, name, "Unicode name roundtrip failed for: {}", name);
    }
}

// ===========================================================================
// DM Message Crypto — Edge Cases (XChaCha20-Poly1305)
// ===========================================================================

/// DM crypto: nonce uniqueness (IND-CPA).
///
/// Security property: Each encryption uses a fresh random 24-byte nonce.
///   Same message encrypted twice must produce different ciphertext.
#[test]
fn test_dm_nonce_uniqueness() {
    let shared_secret = [42u8; 32];
    let plaintext = b"same message encrypted twice";

    let ct1 = encrypt_dm_bytes(plaintext, &shared_secret).unwrap();
    let ct2 = encrypt_dm_bytes(plaintext, &shared_secret).unwrap();

    assert_ne!(ct1, ct2, "Same plaintext must produce different ciphertext (fresh nonce)");

    // Both must decrypt correctly
    let pt1 = decrypt_dm_bytes(&ct1, &shared_secret).unwrap();
    let pt2 = decrypt_dm_bytes(&ct2, &shared_secret).unwrap();
    assert_eq!(pt1, plaintext.to_vec());
    assert_eq!(pt2, plaintext.to_vec());
}

/// DM crypto: binary data with all byte values.
///
/// Security property: Encryption handles arbitrary binary data, not just UTF-8.
#[test]
fn test_dm_binary_data_all_bytes() {
    let shared_secret = [99u8; 32];
    let binary_data: Vec<u8> = (0..=255).collect();

    let encrypted = encrypt_dm_bytes(&binary_data, &shared_secret).unwrap();
    let decrypted = decrypt_dm_bytes(&encrypted, &shared_secret).unwrap();
    assert_eq!(decrypted, binary_data);
}

/// DM crypto: large message (1MB).
#[test]
fn test_dm_large_message() {
    let shared_secret = [11u8; 32];
    let large_data = vec![0xABu8; 1024 * 1024]; // 1MB

    let encrypted = encrypt_dm_bytes(&large_data, &shared_secret).unwrap();
    let decrypted = decrypt_dm_bytes(&encrypted, &shared_secret).unwrap();
    assert_eq!(decrypted.len(), large_data.len());
    assert_eq!(decrypted, large_data);
}

/// DM crypto: ciphertext has correct structure (nonce || ciphertext || tag).
///
/// Security property: Nonce is 24 bytes (XChaCha20), tag is 16 bytes (Poly1305).
#[test]
fn test_dm_ciphertext_structure() {
    let shared_secret = [42u8; 32];
    let plaintext = b"structure test";

    let encrypted = encrypt_dm_bytes(plaintext, &shared_secret).unwrap();

    // XChaCha20-Poly1305: 24-byte nonce + plaintext_len + 16-byte tag
    let expected_len = 24 + plaintext.len() + 16;
    assert_eq!(encrypted.len(), expected_len,
        "Ciphertext should be nonce(24) + plaintext({}) + tag(16) = {}, got {}",
        plaintext.len(), expected_len, encrypted.len());
}

/// DM string message: UTF-8 validation on decrypt.
///
/// Security property: decrypt_dm_message validates UTF-8 to prevent
///   corrupted data from being displayed.
#[test]
fn test_dm_message_utf8_validation() {
    let shared_secret = [42u8; 32];

    // Encrypt invalid UTF-8 as raw bytes
    let invalid_utf8 = vec![0xFF, 0xFE, 0x00, 0x01];
    let encrypted = encrypt_dm_bytes(&invalid_utf8, &shared_secret).unwrap();

    // decrypt_dm_message should fail (expects valid UTF-8)
    let result = decrypt_dm_message(&encrypted, &shared_secret);
    assert!(result.is_err(), "Invalid UTF-8 should be rejected by string decryption");

    // decrypt_dm_bytes should succeed (raw bytes, no UTF-8 check)
    let raw = decrypt_dm_bytes(&encrypted, &shared_secret).unwrap();
    assert_eq!(raw, invalid_utf8);
}

/// DM crypto: truncated ciphertext rejected.
///
/// Security property: Ciphertext shorter than nonce + tag is invalid.
#[test]
fn test_dm_truncated_ciphertext_rejected() {
    let shared_secret = [42u8; 32];

    // Too short (less than nonce + tag = 40 bytes)
    let short = vec![0u8; 10];
    let result = decrypt_dm_bytes(&short, &shared_secret);
    assert!(result.is_err(), "Truncated ciphertext must be rejected");
}

/// DM crypto: single byte flip in ciphertext detected.
///
/// Security property: Poly1305 tag detects any modification.
#[test]
fn test_dm_single_bit_flip_detected() {
    let shared_secret = [42u8; 32];
    let plaintext = b"integrity check";

    let mut encrypted = encrypt_dm_bytes(plaintext, &shared_secret).unwrap();

    // Flip one bit in the middle of the ciphertext (after nonce, before tag)
    let mid = 24 + plaintext.len() / 2;
    encrypted[mid] ^= 0x01;

    let result = decrypt_dm_bytes(&encrypted, &shared_secret);
    assert!(result.is_err(), "Single bit flip must be detected");
}

// ===========================================================================
// Attachment padding — traffic analysis resistance
// ===========================================================================

/// Attachment padding uses log-bucketing to prevent file size inference.
///
/// Security property (NDSS'21): Server cannot determine exact file size
///   from the padded/encrypted size.
#[test]
fn test_attachment_padding_log_bucketing() {
    // Different sizes should map to a smaller set of bucket sizes
    let sizes = [1, 100, 500, 1000, 5000, 10000, 50000, 100000, 500000, 1000000];
    let padded: Vec<usize> = sizes.iter().map(|&s| AttachmentCrypto::padded_size(s)).collect();

    // Padded size should always be >= original
    for (i, &s) in sizes.iter().enumerate() {
        assert!(padded[i] >= s, "Padded size {} must be >= original {}", padded[i], s);
    }

    // Padded size should always be 16-byte aligned (AES block size)
    for &p in &padded {
        assert_eq!(p % 16, 0, "Padded size {} must be 16-byte aligned", p);
    }

    // Adjacent similar sizes should map to the same bucket
    let p_999 = AttachmentCrypto::padded_size(999);
    let p_1000 = AttachmentCrypto::padded_size(1000);
    let p_1001 = AttachmentCrypto::padded_size(1001);
    // At least two of these three should be equal (same bucket)
    let same_bucket = (p_999 == p_1000) || (p_1000 == p_1001) || (p_999 == p_1001);
    assert!(same_bucket, "Log bucketing should group nearby sizes: 999→{}, 1000→{}, 1001→{}", p_999, p_1000, p_1001);
}

/// Attachment padding has a minimum size.
///
/// Security property: Even tiny files are padded to a minimum size
///   to prevent distinguishing "empty" from "small" attachments.
#[test]
fn test_attachment_padding_minimum_size() {
    let padded_0 = AttachmentCrypto::padded_size(0);
    let padded_1 = AttachmentCrypto::padded_size(1);

    assert!(padded_0 > 0, "Even empty file should have padding: {}", padded_0);
    assert!(padded_0 >= 541, "Minimum padded size should be >= 541 (Signal compat): {}", padded_0);
    assert_eq!(padded_0, padded_1, "0-byte and 1-byte files should have same padded size");
}
