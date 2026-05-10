//! Profile data encryption.
//!
//! Encrypts profile fields (display_name, status) with profile key per Signal spec.
//! Uses AES-256-GCM with random nonce, padded to fixed lengths to prevent length inference.

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};

use crate::error::CryptoError;

/// Display-name padding buckets per Signal spec
/// (`Signal-Android/.../ProfileCipher.java::NAME_PADDED_LENGTH_{1,2}`).
/// Short names ride the small bucket; longer ones bump up. Multi-bucket
/// padding hides exact length within each bucket and saves bandwidth vs a
/// single global maximum.
pub const NAME_PADDED_LENGTH_SHORT: usize = 53;
pub const NAME_PADDED_LENGTH_LONG: usize = 257;

/// Status / "about" padding buckets per Signal spec
/// (`ABOUT_PADDED_LENGTH_{1,2,3}`).
pub const STATUS_PADDED_LENGTH_SHORT: usize = 128;
pub const STATUS_PADDED_LENGTH_MID: usize = 254;
pub const STATUS_PADDED_LENGTH_LONG: usize = 512;

/// Maximum supported display-name length (the largest bucket).
pub const MAX_NAME_LENGTH: usize = NAME_PADDED_LENGTH_LONG;
/// Maximum supported status length (the largest bucket).
pub const MAX_STATUS_LENGTH: usize = STATUS_PADDED_LENGTH_LONG;

/// Pick the smallest padding bucket that fits `len` bytes.
///
/// Returns `None` if no bucket fits (caller should reject as oversized).
fn pick_bucket(len: usize, buckets: &[usize]) -> Option<usize> {
    buckets.iter().copied().find(|&b| len <= b)
}

/// Encrypt profile item with padding.
///
/// 1. Pad plaintext to fixed length
/// 2. Generate random 12-byte nonce
/// 3. AES-256-GCM encrypt
/// 4. Return nonce || ciphertext
pub fn encrypt_profile_item(
    profile_key: &[u8; 32],
    plaintext: &str,
    padded_length: usize,
) -> Result<Vec<u8>, CryptoError> {
    // Validate plaintext fits
    let bytes = plaintext.as_bytes();
    if bytes.len() > padded_length {
        return Err(CryptoError::Validation(format!(
            "plaintext too long: {} > {}",
            bytes.len(),
            padded_length
        )));
    }

    // Pad with zeros to fixed length
    let mut padded = vec![0u8; padded_length];
    padded[..bytes.len()].copy_from_slice(bytes);

    // Generate random nonce
    let mut nonce_bytes = [0u8; 12];
    {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut nonce_bytes).expect("OS CSPRNG must not fail");}
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt
    let cipher =
        Aes256Gcm::new_from_slice(profile_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let ciphertext = cipher
        .encrypt(nonce, padded.as_ref())
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Return nonce || ciphertext
    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt profile item.
///
/// 1. Extract nonce (first 12 bytes)
/// 2. AES-256-GCM decrypt
/// 3. Strip padding (trailing zeros)
/// 4. Return as UTF-8 string
pub fn decrypt_profile_item(
    profile_key: &[u8; 32],
    encrypted: &[u8],
) -> Result<String, CryptoError> {
    // Minimum length: 12-byte nonce + 16-byte auth tag
    if encrypted.len() < 12 + 16 {
        return Err(CryptoError::Validation(
            "ciphertext too short".to_string(),
        ));
    }

    let nonce = Nonce::from_slice(&encrypted[..12]);
    let ciphertext = &encrypted[12..];

    let cipher =
        Aes256Gcm::new_from_slice(profile_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let padded = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    // Strip trailing zeros
    let end = padded
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);

    String::from_utf8(padded[..end].to_vec()).map_err(|e| {
        CryptoError::DecryptionFailed(format!("invalid UTF-8 in decrypted profile data: {}", e))
    })
}

/// Encrypt display name. Pads to the smallest Signal name-bucket (53 or 257).
pub fn encrypt_display_name(profile_key: &[u8; 32], name: &str) -> Result<Vec<u8>, CryptoError> {
    let bucket = pick_bucket(
        name.len(),
        &[NAME_PADDED_LENGTH_SHORT, NAME_PADDED_LENGTH_LONG],
    )
    .ok_or_else(|| {
        CryptoError::Validation(format!(
            "display name too long: {} > {}",
            name.len(),
            MAX_NAME_LENGTH
        ))
    })?;
    encrypt_profile_item(profile_key, name, bucket)
}

/// Decrypt display name. Bucket is recovered from ciphertext length; the
/// trailing-zero strip in `decrypt_profile_item` handles all bucket sizes.
pub fn decrypt_display_name(profile_key: &[u8; 32], encrypted: &[u8]) -> Result<String, CryptoError> {
    decrypt_profile_item(profile_key, encrypted)
}

/// Encrypt status message. Pads to the smallest Signal about-bucket
/// (128 / 254 / 512).
pub fn encrypt_status(profile_key: &[u8; 32], status: &str) -> Result<Vec<u8>, CryptoError> {
    let bucket = pick_bucket(
        status.len(),
        &[
            STATUS_PADDED_LENGTH_SHORT,
            STATUS_PADDED_LENGTH_MID,
            STATUS_PADDED_LENGTH_LONG,
        ],
    )
    .ok_or_else(|| {
        CryptoError::Validation(format!(
            "status too long: {} > {}",
            status.len(),
            MAX_STATUS_LENGTH
        ))
    })?;
    encrypt_profile_item(profile_key, status, bucket)
}

/// Decrypt status message.
pub fn decrypt_status(profile_key: &[u8; 32], encrypted: &[u8]) -> Result<String, CryptoError> {
    decrypt_profile_item(profile_key, encrypted)
}

// =============================================================================
// Avatar Encryption (binary data, no padding)
// =============================================================================

/// Maximum avatar size (2MB).
pub const MAX_AVATAR_SIZE: usize = 2 * 1024 * 1024;

/// Encrypt avatar bytes with profile key.
///
/// Unlike text fields, avatars are not padded (they're shared directly in
/// encrypted form, not stored on server where size could leak information).
///
/// 1. Validate size
/// 2. Generate random 12-byte nonce
/// 3. AES-256-GCM encrypt
/// 4. Return nonce || ciphertext
pub fn encrypt_avatar(profile_key: &[u8; 32], avatar: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if avatar.is_empty() {
        return Err(CryptoError::Validation("avatar is empty".to_string()));
    }
    if avatar.len() > MAX_AVATAR_SIZE {
        return Err(CryptoError::Validation(format!(
            "avatar too large: {} > {} bytes",
            avatar.len(),
            MAX_AVATAR_SIZE
        )));
    }

    // Generate random nonce
    let mut nonce_bytes = [0u8; 12];
    {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut nonce_bytes).expect("OS CSPRNG must not fail");}
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt
    let cipher =
        Aes256Gcm::new_from_slice(profile_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let ciphertext = cipher
        .encrypt(nonce, avatar)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Return nonce || ciphertext
    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt avatar bytes.
///
/// 1. Extract nonce (first 12 bytes)
/// 2. AES-256-GCM decrypt
/// 3. Return raw bytes
pub fn decrypt_avatar(profile_key: &[u8; 32], encrypted: &[u8]) -> Result<Vec<u8>, CryptoError> {
    // Minimum length: 12-byte nonce + 16-byte auth tag + 1 byte data
    if encrypted.len() < 12 + 16 + 1 {
        return Err(CryptoError::Validation(
            "encrypted avatar too short".to_string(),
        ));
    }

    let nonce = Nonce::from_slice(&encrypted[..12]);
    let ciphertext = &encrypted[12..];

    let cipher =
        Aes256Gcm::new_from_slice(profile_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_display_name() {
        let profile_key = [0x42u8; 32];
        let name = "Alice Smith";

        let encrypted = encrypt_display_name(&profile_key, name).unwrap();

        // Short name fits the 53-byte bucket: 12-byte nonce + 53 + 16-byte tag.
        assert_eq!(encrypted.len(), 12 + NAME_PADDED_LENGTH_SHORT + 16);

        let decrypted = decrypt_display_name(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, name);
    }

    #[test]
    fn long_name_uses_long_bucket() {
        let profile_key = [0x42u8; 32];
        // 100 bytes overflows the 53-byte bucket → 257-byte bucket.
        let name = "a".repeat(100);
        let encrypted = encrypt_display_name(&profile_key, &name).unwrap();
        assert_eq!(encrypted.len(), 12 + NAME_PADDED_LENGTH_LONG + 16);
        assert_eq!(decrypt_display_name(&profile_key, &encrypted).unwrap(), name);
    }

    #[test]
    fn test_encrypt_decrypt_status() {
        let profile_key = [0x42u8; 32];
        let status = "Available for chat";

        let encrypted = encrypt_status(&profile_key, status).unwrap();

        // Short status fits the 128-byte bucket.
        assert_eq!(encrypted.len(), 12 + STATUS_PADDED_LENGTH_SHORT + 16);

        let decrypted = decrypt_status(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, status);
    }

    #[test]
    fn status_buckets_promote_with_length() {
        let profile_key = [0x42u8; 32];
        for (n, bucket) in [
            (50, STATUS_PADDED_LENGTH_SHORT),
            (200, STATUS_PADDED_LENGTH_MID),
            (400, STATUS_PADDED_LENGTH_LONG),
        ] {
            let s = "a".repeat(n);
            let enc = encrypt_status(&profile_key, &s).unwrap();
            assert_eq!(enc.len(), 12 + bucket + 16, "len={n} expected bucket={bucket}");
            assert_eq!(decrypt_status(&profile_key, &enc).unwrap(), s);
        }
    }

    #[test]
    fn test_empty_string() {
        let profile_key = [0x42u8; 32];
        let name = "";

        let encrypted = encrypt_display_name(&profile_key, name).unwrap();
        let decrypted = decrypt_display_name(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, name);
    }

    #[test]
    fn test_unicode_name() {
        let profile_key = [0x42u8; 32];
        let name = "日本語の名前 🎉";

        let encrypted = encrypt_display_name(&profile_key, name).unwrap();
        let decrypted = decrypt_display_name(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, name);
    }

    #[test]
    fn test_max_length_name() {
        let profile_key = [0x42u8; 32];
        // Exactly the largest bucket size still encrypts cleanly.
        let name = "a".repeat(MAX_NAME_LENGTH);
        let encrypted = encrypt_display_name(&profile_key, &name).unwrap();
        let decrypted = decrypt_display_name(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, name);
    }

    #[test]
    fn test_too_long_name() {
        let profile_key = [0x42u8; 32];
        // One byte past the largest bucket - rejected.
        let name = "a".repeat(MAX_NAME_LENGTH + 1);
        let result = encrypt_display_name(&profile_key, &name);
        assert!(result.is_err());
    }

    #[test]
    fn test_wrong_key_fails_decryption() {
        let profile_key = [0x42u8; 32];
        let wrong_key = [0x43u8; 32];
        let name = "Alice";

        let encrypted = encrypt_display_name(&profile_key, name).unwrap();
        let result = decrypt_display_name(&wrong_key, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let profile_key = [0x42u8; 32];
        let name = "Alice";

        let mut encrypted = encrypt_display_name(&profile_key, name).unwrap();
        // Tamper with ciphertext
        encrypted[20] ^= 0xFF;

        let result = decrypt_display_name(&profile_key, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_truncated_ciphertext_fails() {
        let profile_key = [0x42u8; 32];
        let name = "Alice";

        let encrypted = encrypt_display_name(&profile_key, name).unwrap();
        // Truncate to just nonce (12 bytes) - missing ciphertext
        let truncated = &encrypted[..12];

        let result = decrypt_display_name(&profile_key, truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_different_encryptions_produce_different_ciphertext() {
        let profile_key = [0x42u8; 32];
        let name = "Alice";

        let encrypted1 = encrypt_display_name(&profile_key, name).unwrap();
        let encrypted2 = encrypt_display_name(&profile_key, name).unwrap();

        // Different nonces mean different ciphertext
        assert_ne!(encrypted1, encrypted2);

        // But both decrypt to the same plaintext
        let decrypted1 = decrypt_display_name(&profile_key, &encrypted1).unwrap();
        let decrypted2 = decrypt_display_name(&profile_key, &encrypted2).unwrap();
        assert_eq!(decrypted1, name);
        assert_eq!(decrypted2, name);
    }

    // =========================================================================
    // Avatar encryption tests
    // =========================================================================

    #[test]
    fn test_encrypt_decrypt_avatar() {
        let profile_key = [0x42u8; 32];
        // Simulate a small PNG header
        let avatar = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];

        let encrypted = encrypt_avatar(&profile_key, &avatar).unwrap();

        // Verify structure: 12-byte nonce + ciphertext + 16-byte tag
        assert_eq!(encrypted.len(), 12 + avatar.len() + 16);

        let decrypted = decrypt_avatar(&profile_key, &encrypted).unwrap();
        assert_eq!(decrypted, avatar);
    }

    #[test]
    fn test_avatar_empty_fails() {
        let profile_key = [0x42u8; 32];
        let result = encrypt_avatar(&profile_key, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_avatar_too_large_fails() {
        let profile_key = [0x42u8; 32];
        let large_avatar = vec![0u8; MAX_AVATAR_SIZE + 1];
        let result = encrypt_avatar(&profile_key, &large_avatar);
        assert!(result.is_err());
    }

    #[test]
    fn test_avatar_wrong_key_fails() {
        let profile_key = [0x42u8; 32];
        let wrong_key = [0x43u8; 32];
        let avatar = vec![1, 2, 3, 4, 5];

        let encrypted = encrypt_avatar(&profile_key, &avatar).unwrap();
        let result = decrypt_avatar(&wrong_key, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_avatar_tampered_fails() {
        let profile_key = [0x42u8; 32];
        let avatar = vec![1, 2, 3, 4, 5];

        let mut encrypted = encrypt_avatar(&profile_key, &avatar).unwrap();
        encrypted[15] ^= 0xFF; // Tamper with ciphertext

        let result = decrypt_avatar(&profile_key, &encrypted);
        assert!(result.is_err());
    }
}
