//! Attachment encryption/decryption.
//!
//! Signal-compatible file attachment encryption using AES-256-CBC + HMAC-SHA256.
//!
//! ## Format
//!
//! - **Key:** 64 bytes = [AES key (32)] + [MAC key (32)]
//! - **Ciphertext:** IV (16) || AES-CBC(padded plaintext) || HMAC-SHA256 (32)
//! - **Digest:** SHA256(entire ciphertext)
//!
//! ## Padding
//!
//! Uses log-bucketing to prevent file size inference:
//! `padded_size = max(541, ceil(1.05^ceil(log(size)/log(1.05))))`

use aes::Aes256;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::CryptoError;

/// AES block size and IV length.
const IV_LENGTH: usize = 16;

/// AES-256 key length.
const AES_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 key length.
const MAC_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 output length.
const HMAC_LENGTH: usize = 32;

/// Combined key length (AES key + MAC key).
pub const KEY_SET_LENGTH: usize = AES_KEY_LENGTH + MAC_KEY_LENGTH;

/// Minimum ciphertext length: IV + one AES block + HMAC.
const MIN_CIPHERTEXT_LENGTH: usize = IV_LENGTH + 16 + HMAC_LENGTH;

/// Minimum padded size for log-bucketing.
const MIN_PADDED_SIZE: usize = 541;

/// Log-bucketing base (1.05).
const BUCKET_BASE: f64 = 1.05;

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Encrypted attachment with metadata.
///
/// Contains the encrypted ciphertext, the encryption key (which should be
/// included in the E2E encrypted message), the digest for verification,
/// and the original plaintext size for proper decryption.
#[derive(Debug, Clone)]
pub struct EncryptedAttachment {
    /// Encrypted data: IV (16) || AES-CBC ciphertext || HMAC-SHA256 (32)
    pub ciphertext: Vec<u8>,

    /// Combined encryption key: [AES key (32)] + [MAC key (32)]
    /// This key should be included in the E2E encrypted message, never sent to server.
    pub key: [u8; KEY_SET_LENGTH],

    /// SHA-256 digest of the complete ciphertext.
    /// Used to verify integrity before decryption.
    pub digest: [u8; 32],

    /// Original plaintext size in bytes.
    /// Required to remove padding after decryption.
    pub plaintext_size: usize,
}

/// Attachment encryption service.
///
/// Provides Signal-compatible encryption using AES-256-CBC + HMAC-SHA256.
/// This is a stateless service - all state is passed as parameters.
pub struct AttachmentCrypto;

impl AttachmentCrypto {
    /// Encrypt plaintext with a randomly generated key.
    ///
    /// Generates a random 64-byte key, pads plaintext using log-bucketing,
    /// encrypts with AES-256-CBC, and authenticates with HMAC-SHA256.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The data to encrypt
    ///
    /// # Returns
    ///
    /// An `EncryptedAttachment` containing the ciphertext, key, digest,
    /// and original plaintext size. The key should be included in the
    /// E2E encrypted message.
    ///
    /// # Errors
    ///
    /// Returns `CryptoError::AttachmentEncryptionFailed` if encryption fails.
    pub fn encrypt(plaintext: &[u8]) -> Result<EncryptedAttachment, CryptoError> {
        // Generate 64-byte random key: [AES key (32)] + [MAC key (32)]
        let mut key = [0u8; KEY_SET_LENGTH];
        rand::rng().fill_bytes(&mut key);

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Pad plaintext using log-bucketing
        let padded_size = Self::padded_size(plaintext.len());
        let mut padded = vec![0u8; padded_size];
        padded[..plaintext.len()].copy_from_slice(plaintext);

        // Generate random IV (16 bytes)
        let mut iv = [0u8; IV_LENGTH];
        rand::rng().fill_bytes(&mut iv);

        // Encrypt with AES-256-CBC (NoPadding - we handle padding manually with zeros)
        let cipher = Aes256CbcEnc::new_from_slices(aes_key, &iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;

        // Clone padded data for in-place encryption
        let mut encrypted = padded.clone();
        cipher
            .encrypt_padded_mut::<block_padding::NoPadding>(&mut encrypted, padded_size)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;

        // Compute HMAC-SHA256 over IV + ciphertext
        let mut mac = HmacSha256::new_from_slice(mac_key)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;
        mac.update(&iv);
        mac.update(&encrypted);
        let hmac_result = mac.finalize().into_bytes();

        // Assemble: IV || ciphertext || HMAC
        let mut ciphertext = Vec::with_capacity(IV_LENGTH + encrypted.len() + HMAC_LENGTH);
        ciphertext.extend_from_slice(&iv);
        ciphertext.extend_from_slice(&encrypted);
        ciphertext.extend_from_slice(&hmac_result);

        // Compute digest = SHA256(entire ciphertext)
        let digest = Self::compute_digest(&ciphertext);

        Ok(EncryptedAttachment {
            ciphertext,
            key,
            digest,
            plaintext_size: plaintext.len(),
        })
    }

    /// Decrypt ciphertext with provided key and verify integrity.
    ///
    /// Verifies the digest first, then the HMAC, then decrypts.
    /// This is the verify-then-decrypt pattern required for security.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The encrypted data: IV (16) || encrypted || HMAC (32)
    /// * `key` - The 64-byte combined key: [AES key (32)] + [MAC key (32)]
    /// * `expected_digest` - The expected SHA-256 digest of the ciphertext
    /// * `plaintext_size` - The original plaintext size (for removing padding)
    ///
    /// # Returns
    ///
    /// The decrypted plaintext.
    ///
    /// # Errors
    ///
    /// - `CryptoError::AttachmentDigestMismatch` if digest verification fails
    /// - `CryptoError::AttachmentMacMismatch` if HMAC verification fails
    /// - `CryptoError::AttachmentDecryptionFailed` if decryption fails
    pub fn decrypt(
        ciphertext: &[u8],
        key: &[u8; KEY_SET_LENGTH],
        expected_digest: &[u8; 32],
        plaintext_size: usize,
    ) -> Result<Vec<u8>, CryptoError> {
        // Verify minimum length
        if ciphertext.len() < MIN_CIPHERTEXT_LENGTH {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "ciphertext too short".to_string(),
            ));
        }

        // Verify digest first (constant-time comparison)
        let actual_digest = Self::compute_digest(ciphertext);
        if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentDigestMismatch);
        }

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Split ciphertext into IV, encrypted data, and HMAC
        let iv = &ciphertext[..IV_LENGTH];
        let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
        let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

        // Verify HMAC (constant-time comparison)
        let mut mac = HmacSha256::new_from_slice(mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;
        mac.update(iv);
        mac.update(encrypted);
        let computed_hmac = mac.finalize().into_bytes();

        if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentMacMismatch);
        }

        // Decrypt with AES-256-CBC
        let cipher = Aes256CbcDec::new_from_slices(aes_key, iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;

        let mut decrypted = encrypted.to_vec();
        cipher
            .decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;

        // Truncate to original plaintext size (remove zero padding)
        if plaintext_size > decrypted.len() {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "plaintext_size exceeds decrypted data".to_string(),
            ));
        }
        decrypted.truncate(plaintext_size);

        Ok(decrypted)
    }

    /// Compute SHA-256 digest of ciphertext.
    ///
    /// This digest is included in the AttachmentPointer and verified
    /// before decryption to detect corruption or tampering.
    #[must_use]
    pub fn compute_digest(ciphertext: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(ciphertext);
        hasher.finalize().into()
    }

    /// Compute padded size using Signal's log-bucketing algorithm.
    ///
    /// This prevents file size leakage while minimizing overhead.
    /// Formula: `max(541, ceil(1.05^ceil(log(size)/log(1.05))))`
    ///
    /// The result is always a multiple of 16 (AES block size).
    #[must_use]
    pub fn padded_size(original_size: usize) -> usize {
        if original_size == 0 {
            // Minimum size that's a multiple of 16 and >= 541
            return Self::round_to_block_size(MIN_PADDED_SIZE);
        }

        let bucket = (original_size as f64).ln() / BUCKET_BASE.ln();
        let rounded = bucket.ceil();
        let padded = BUCKET_BASE.powf(rounded) as usize;
        let result = padded.max(MIN_PADDED_SIZE);

        // Ensure result is a multiple of AES block size (16)
        Self::round_to_block_size(result)
    }

    /// Round up to the next multiple of 16 (AES block size).
    #[inline]
    fn round_to_block_size(size: usize) -> usize {
        (size + 15) & !15
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = b"Hello, Signal-compatible encryption!";

        // Encrypt
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Verify ciphertext structure: IV (16) + encrypted + HMAC (32)
        assert!(encrypted.ciphertext.len() >= MIN_CIPHERTEXT_LENGTH);
        assert_eq!(encrypted.plaintext_size, plaintext.len());

        // Decrypt
        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty() {
        let plaintext = b"";

        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_large() {
        // Test with 1MB of data
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext)
            .expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_digest_mismatch() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Tamper with digest
        let mut bad_digest = encrypted.digest;
        bad_digest[0] ^= 0xFF;

        let result = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentDigestMismatch)));
    }

    #[test]
    fn test_mac_mismatch() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Tamper with ciphertext (the encrypted portion, not the HMAC)
        // This will cause HMAC verification to fail
        let mut bad_ciphertext = encrypted.ciphertext.clone();
        bad_ciphertext[IV_LENGTH + 5] ^= 0xFF;

        // Recompute digest for the tampered ciphertext
        let bad_digest = AttachmentCrypto::compute_digest(&bad_ciphertext);

        let result = AttachmentCrypto::decrypt(
            &bad_ciphertext,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentMacMismatch)));
    }

    #[test]
    fn test_ciphertext_too_short() {
        let key = [0u8; KEY_SET_LENGTH];
        let digest = [0u8; 32];
        let short_ciphertext = vec![0u8; 10];

        let result = AttachmentCrypto::decrypt(&short_ciphertext, &key, &digest, 0);

        assert!(matches!(result, Err(CryptoError::AttachmentDecryptionFailed(_))));
    }

    #[test]
    fn test_padded_size_zero() {
        let padded = AttachmentCrypto::padded_size(0);
        assert!(padded >= MIN_PADDED_SIZE);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_small() {
        let padded = AttachmentCrypto::padded_size(100);
        assert!(padded >= MIN_PADDED_SIZE);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_large() {
        let padded = AttachmentCrypto::padded_size(10_000);
        assert!(padded >= 10_000);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_always_larger() {
        for size in [1, 100, 1000, 10_000, 100_000, 1_000_000] {
            let padded = AttachmentCrypto::padded_size(size);
            assert!(padded >= size, "padded size should be >= original for size {}", size);
            assert_eq!(padded % 16, 0, "padded size should be multiple of 16 for size {}", size);
        }
    }

    #[test]
    fn test_different_plaintexts_different_ciphertexts() {
        let encrypted1 = AttachmentCrypto::encrypt(b"hello")
            .expect("encryption should succeed");
        let encrypted2 = AttachmentCrypto::encrypt(b"hello")
            .expect("encryption should succeed");

        // Same plaintext should produce different ciphertext (random IV and key)
        assert_ne!(encrypted1.ciphertext, encrypted2.ciphertext);
        assert_ne!(encrypted1.key, encrypted2.key);
        assert_ne!(encrypted1.digest, encrypted2.digest);
    }

    #[test]
    fn test_key_length() {
        let encrypted = AttachmentCrypto::encrypt(b"test")
            .expect("encryption should succeed");

        assert_eq!(encrypted.key.len(), KEY_SET_LENGTH);
        assert_eq!(KEY_SET_LENGTH, 64);
    }

    #[test]
    fn test_ciphertext_format() {
        let plaintext = b"test message";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Format: IV (16) + encrypted (padded, multiple of 16) + HMAC (32)
        let expected_padded = AttachmentCrypto::padded_size(plaintext.len());
        let expected_length = IV_LENGTH + expected_padded + HMAC_LENGTH;

        assert_eq!(encrypted.ciphertext.len(), expected_length);
    }
}
