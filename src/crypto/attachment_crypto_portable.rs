//! Portable attachment encryption/decryption with hardware acceleration.
//!
//! Cross-platform implementation using pure Rust crates that auto-detect
//! hardware intrinsics:
//! - AES-NI (x86_64) / ARM Crypto Extensions via `aes` crate
//! - SHA-NI (x86_64) / ARM SHA2 via `sha2` crate
//! - Parallel SHA256 + HMAC + AES via rayon
//!
//! No OpenSSL dependency - works on Linux, Windows, macOS, Android, iOS.

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::crypto::secure_buffer::SecureBuffer;
use crate::error::CryptoError;

use super::attachment_constants::*;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Threshold for parallel processing (1MB)
const PARALLEL_THRESHOLD: usize = 1024 * 1024;

/// Portable single-pass decryption.
///
/// Computes digest and HMAC in a single pass, then decrypts.
/// Uses hardware intrinsics when available (AES-NI, SHA-NI, ARM crypto).
pub fn decrypt_portable(
    ciphertext: &[u8],
    key: &[u8; KEY_SET_LENGTH],
    expected_digest: &[u8; 32],
    plaintext_size: usize,
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < MIN_CIPHERTEXT_LENGTH {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "ciphertext too short".to_string(),
        ));
    }

    let aes_key = &key[..AES_KEY_LENGTH];
    let mac_key = &key[AES_KEY_LENGTH..];

    let iv = &ciphertext[..IV_LENGTH];
    let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
    let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

    // Single pass: compute SHA256 and HMAC together
    let mut digest_hasher = Sha256::new();
    let mut hmac = HmacSha256::new_from_slice(mac_key)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;

    // Process IV
    digest_hasher.update(iv);
    hmac.update(iv);

    // Process encrypted data in cache-friendly chunks
    const CHUNK_SIZE: usize = 64 * 1024;
    for chunk in encrypted.chunks(CHUNK_SIZE) {
        digest_hasher.update(chunk);
        hmac.update(chunk);
    }

    // HMAC bytes only for digest
    digest_hasher.update(expected_hmac);

    let actual_digest: [u8; 32] = digest_hasher.finalize().into();
    let computed_hmac = hmac.finalize().into_bytes();

    // Verify digest
    if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentDigestMismatch);
    }

    // Verify HMAC
    if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentMacMismatch);
    }

    // Decrypt with AES-256-CBC (uses AES-NI when available)
    let mut decrypted = encrypted.to_vec();
    let cipher = Aes256CbcDec::new_from_slices(aes_key, iv)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {}", e)))?;
    cipher
        .decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES decrypt: {}", e)))?;

    if plaintext_size > decrypted.len() {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "plaintext_size exceeds decrypted data".to_string(),
        ));
    }
    decrypted.truncate(plaintext_size);

    Ok(decrypted)
}

/// Parallel decryption for large files.
///
/// Runs SHA256, HMAC, and AES decryption ALL in parallel using rayon.
/// For files > 1MB, this is significantly faster than sequential.
pub fn decrypt_parallel(
    ciphertext: Vec<u8>,
    key: &[u8; KEY_SET_LENGTH],
    expected_digest: &[u8; 32],
    plaintext_size: usize,
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < MIN_CIPHERTEXT_LENGTH {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "ciphertext too short".to_string(),
        ));
    }

    let aes_key: [u8; 32] = key[..AES_KEY_LENGTH].try_into().unwrap();
    let mac_key: [u8; 32] = key[AES_KEY_LENGTH..].try_into().unwrap();

    let iv: [u8; 16] = ciphertext[..IV_LENGTH].try_into().unwrap();
    let hmac_start = ciphertext.len() - HMAC_LENGTH;
    let encrypted = &ciphertext[IV_LENGTH..hmac_start];
    let data_len = encrypted.len();

    // For small files, use sequential (thread overhead not worth it)
    if data_len < PARALLEL_THRESHOLD {
        return decrypt_portable(&ciphertext, key, expected_digest, plaintext_size);
    }

    // Run ALL THREE in parallel: SHA256, HMAC, AES
    let ciphertext_ref = &ciphertext;

    let ((actual_digest, computed_hmac), decrypt_result) = rayon::join(
        // Thread 1+2: SHA256 and HMAC in parallel
        || {
            rayon::join(
                || {
                    // SHA256: hash entire ciphertext
                    let mut hasher = Sha256::new();
                    hasher.update(ciphertext_ref);
                    let result: [u8; 32] = hasher.finalize().into();
                    result
                },
                || {
                    // HMAC: hash IV + encrypted (not HMAC bytes)
                    let mut hmac = HmacSha256::new_from_slice(&mac_key).unwrap();
                    hmac.update(&ciphertext_ref[..hmac_start]);
                    let result: [u8; 32] = hmac.finalize().into_bytes().into();
                    result
                },
            )
        },
        // Thread 3: AES decryption
        || {
            let mut decrypted = encrypted.to_vec();
            let cipher = Aes256CbcDec::new_from_slices(&aes_key, &iv)
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES cipher init: {}", e)))?;
            cipher
                .decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted)
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES: {}", e)))?;
            Ok::<_, CryptoError>(decrypted)
        },
    );

    let mut decrypted = decrypt_result?;

    // Verify digest
    if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentDigestMismatch);
    }

    // Verify HMAC
    let expected_hmac = &ciphertext[hmac_start..];
    if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentMacMismatch);
    }

    if plaintext_size > decrypted.len() {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "plaintext_size exceeds decrypted data".to_string(),
        ));
    }
    decrypted.truncate(plaintext_size);

    Ok(decrypted)
}

/// Secure portable decryption with auto-zeroizing buffer.
pub fn decrypt_portable_secure(
    ciphertext: &[u8],
    key: &[u8; KEY_SET_LENGTH],
    expected_digest: &[u8; 32],
    plaintext_size: usize,
) -> Result<SecureBuffer, CryptoError> {
    let decrypted = decrypt_portable(ciphertext, key, expected_digest, plaintext_size)?;
    Ok(SecureBuffer::new(decrypted))
}

/// Secure parallel decryption with auto-zeroizing buffer.
pub fn decrypt_parallel_secure(
    ciphertext: Vec<u8>,
    key: &[u8; KEY_SET_LENGTH],
    expected_digest: &[u8; 32],
    plaintext_size: usize,
) -> Result<SecureBuffer, CryptoError> {
    let decrypted = decrypt_parallel(ciphertext, key, expected_digest, plaintext_size)?;
    Ok(SecureBuffer::new(decrypted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;

    #[test]
    fn test_portable_roundtrip() {
        let plaintext = b"Hello, portable decryption!";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let decrypted = decrypt_portable(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_parallel_roundtrip() {
        // 2MB to trigger parallel path
        let plaintext: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let decrypted = decrypt_parallel(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_portable_digest_mismatch() {
        let plaintext = b"test";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let mut bad_digest = encrypted.digest;
        bad_digest[0] ^= 0xFF;

        let result = decrypt_portable(
            &encrypted.ciphertext,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentDigestMismatch)));
    }
}
