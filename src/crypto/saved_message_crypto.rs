//! Saved message encryption/decryption.
//!
//! Client-side encryption for saved messages using XChaCha20-Poly1305.
//! Encrypts content with the user's profile key before sending to server.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::CryptoError;
use crate::proto::chat_v1::{CryptoVersion, EncryptedData};
use crate::proto::saved_messages_v1::{
    saved_message_content, SavedMessageContent, SavedMessageSource,
};

/// XChaCha20-Poly1305 nonce length (24 bytes).
const NONCE_LENGTH: usize = 24;

/// Key length (256-bit).
const KEY_LENGTH: usize = 32;

/// Poly1305 authentication tag length.
const TAG_LENGTH: usize = 16;

/// Plaintext source metadata (before encryption).
///
/// This struct is serialized to JSON before encryption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedMessageSourcePlain {
    /// Original channel ID.
    pub channel_id: String,
    /// Original message ID.
    pub message_id: String,
    /// Original author ID.
    pub author_id: String,
    /// Original timestamp (Unix ms).
    pub timestamp: u64,
}

/// Saved message encryption service.
///
/// Provides client-side encryption for saved messages using XChaCha20-Poly1305.
/// Content is encrypted with the user's profile key before sending to server.
///
/// # Example
///
/// ```ignore
/// let profile_key = [0u8; 32]; // Get from key storage
///
/// // Encrypt text for saving
/// let content = SavedMessageCrypto::encrypt_text("Hello", &profile_key)?;
///
/// // Decrypt after fetching
/// let text = SavedMessageCrypto::decrypt_content(&content, &profile_key)?;
/// ```
pub struct SavedMessageCrypto;

impl SavedMessageCrypto {
    /// Encrypt plaintext bytes with XChaCha20-Poly1305.
    ///
    /// Returns an EncryptedData proto with version, nonce, ciphertext, and auth_tag.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The data to encrypt
    /// * `profile_key` - 32-byte encryption key
    pub fn encrypt(plaintext: &[u8], profile_key: &[u8; KEY_LENGTH]) -> Result<EncryptedData, CryptoError> {
        // Generate random nonce
        let mut nonce_bytes = [0u8; NONCE_LENGTH];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        // Create cipher
        let cipher = XChaCha20Poly1305::new_from_slice(profile_key)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

        // Encrypt (ciphertext includes auth tag at the end)
        let ciphertext_with_tag = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

        // Split ciphertext and auth tag
        let ciphertext_len = ciphertext_with_tag.len() - TAG_LENGTH;
        let ciphertext = ciphertext_with_tag[..ciphertext_len].to_vec();
        let auth_tag = ciphertext_with_tag[ciphertext_len..].to_vec();

        Ok(EncryptedData {
            version: CryptoVersion::SimpleE2e as i32,
            nonce: nonce_bytes.to_vec(),
            ciphertext,
            auth_tag,
            associated_data: Vec::new(),
            signature: Vec::new(),
            device_id: String::new(),
        })
    }

    /// Decrypt EncryptedData with XChaCha20-Poly1305.
    ///
    /// Validates version and decrypts using the profile key.
    ///
    /// # Arguments
    ///
    /// * `encrypted` - The EncryptedData proto to decrypt
    /// * `profile_key` - 32-byte decryption key
    pub fn decrypt(encrypted: &EncryptedData, profile_key: &[u8; KEY_LENGTH]) -> Result<Vec<u8>, CryptoError> {
        // Validate version
        if encrypted.version != CryptoVersion::SimpleE2e as i32 {
            return Err(CryptoError::DecryptionFailed(format!(
                "unsupported crypto version: {}",
                encrypted.version
            )));
        }

        // Validate nonce length
        if encrypted.nonce.len() != NONCE_LENGTH {
            return Err(CryptoError::DecryptionFailed(format!(
                "invalid nonce length: expected {}, got {}",
                NONCE_LENGTH,
                encrypted.nonce.len()
            )));
        }

        let nonce = XNonce::from_slice(&encrypted.nonce);

        // Reconstruct ciphertext with auth tag appended
        let mut ciphertext_with_tag = encrypted.ciphertext.clone();
        ciphertext_with_tag.extend_from_slice(&encrypted.auth_tag);

        // Create cipher and decrypt
        let cipher = XChaCha20Poly1305::new_from_slice(profile_key)
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext_with_tag.as_slice())
            .map_err(|_| CryptoError::DecryptionFailed("authentication failed".to_string()))?;

        Ok(plaintext)
    }

    /// Encrypt text content for saving.
    ///
    /// Wraps encrypted text in SavedMessageContent::Encrypted.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to encrypt
    /// * `profile_key` - 32-byte encryption key
    pub fn encrypt_text(text: &str, profile_key: &[u8; KEY_LENGTH]) -> Result<SavedMessageContent, CryptoError> {
        let encrypted = Self::encrypt(text.as_bytes(), profile_key)?;

        Ok(SavedMessageContent {
            r#type: Some(saved_message_content::Type::Encrypted(encrypted)),
        })
    }

    /// Decrypt SavedMessageContent to text.
    ///
    /// Currently only supports the Encrypted variant.
    ///
    /// # Arguments
    ///
    /// * `content` - The SavedMessageContent to decrypt
    /// * `profile_key` - 32-byte decryption key
    pub fn decrypt_content(content: &SavedMessageContent, profile_key: &[u8; KEY_LENGTH]) -> Result<String, CryptoError> {
        match &content.r#type {
            Some(saved_message_content::Type::Encrypted(encrypted)) => {
                let plaintext = Self::decrypt(encrypted, profile_key)?;
                String::from_utf8(plaintext)
                    .map_err(|e| CryptoError::DecryptionFailed(format!("invalid UTF-8: {}", e)))
            }
            Some(saved_message_content::Type::EncryptedFile(_)) => {
                // File content is handled separately via file decryption
                Err(CryptoError::DecryptionFailed("file content requires file decryption".to_string()))
            }
            Some(saved_message_content::Type::EncryptedMixed(_)) => {
                // Mixed content requires separate handling for text and files
                Err(CryptoError::DecryptionFailed("mixed content not yet supported".to_string()))
            }
            None => {
                Err(CryptoError::DecryptionFailed("empty content".to_string()))
            }
        }
    }

    /// Encrypt source metadata for saving.
    ///
    /// Serializes source to JSON and encrypts.
    ///
    /// # Arguments
    ///
    /// * `source` - The plaintext source metadata
    /// * `profile_key` - 32-byte encryption key
    pub fn encrypt_source(
        source: &SavedMessageSourcePlain,
        profile_key: &[u8; KEY_LENGTH],
    ) -> Result<SavedMessageSource, CryptoError> {
        // Serialize to JSON
        let json = serde_json::to_vec(source)
            .map_err(|e| CryptoError::EncryptionFailed(format!("failed to serialize source: {}", e)))?;

        // Generate random nonce
        let mut nonce_bytes = [0u8; NONCE_LENGTH];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        // Create cipher
        let cipher = XChaCha20Poly1305::new_from_slice(profile_key)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

        // Encrypt
        let encrypted = cipher
            .encrypt(nonce, json.as_slice())
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

        Ok(SavedMessageSource {
            encrypted_source: encrypted,
            nonce: nonce_bytes.to_vec(),
        })
    }

    /// Decrypt source metadata after fetching.
    ///
    /// Decrypts and deserializes from JSON.
    ///
    /// # Arguments
    ///
    /// * `source` - The encrypted SavedMessageSource
    /// * `profile_key` - 32-byte decryption key
    pub fn decrypt_source(
        source: &SavedMessageSource,
        profile_key: &[u8; KEY_LENGTH],
    ) -> Result<SavedMessageSourcePlain, CryptoError> {
        // Validate nonce length
        if source.nonce.len() != NONCE_LENGTH {
            return Err(CryptoError::DecryptionFailed(format!(
                "invalid nonce length: expected {}, got {}",
                NONCE_LENGTH,
                source.nonce.len()
            )));
        }

        let nonce = XNonce::from_slice(&source.nonce);

        // Create cipher and decrypt
        let cipher = XChaCha20Poly1305::new_from_slice(profile_key)
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

        let plaintext = cipher
            .decrypt(nonce, source.encrypted_source.as_slice())
            .map_err(|_| CryptoError::DecryptionFailed("source authentication failed".to_string()))?;

        // Deserialize from JSON
        serde_json::from_slice(&plaintext)
            .map_err(|e| CryptoError::DecryptionFailed(format!("failed to deserialize source: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(7);
        }
        key
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let profile_key = test_profile_key();
        let plaintext = b"Hello, encrypted saved messages!";

        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");

        assert_eq!(encrypted.version, CryptoVersion::SimpleE2e as i32);
        assert_eq!(encrypted.nonce.len(), NONCE_LENGTH);
        assert!(!encrypted.ciphertext.is_empty());
        assert_eq!(encrypted.auth_tag.len(), TAG_LENGTH);

        let decrypted = SavedMessageCrypto::decrypt(&encrypted, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_text_decrypt_content_roundtrip() {
        let profile_key = test_profile_key();
        let text = "Important message to save";

        let content = SavedMessageCrypto::encrypt_text(text, &profile_key)
            .expect("encryption should succeed");

        let decrypted = SavedMessageCrypto::decrypt_content(&content, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, text);
    }

    #[test]
    fn test_encrypt_source_decrypt_source_roundtrip() {
        let profile_key = test_profile_key();
        let source = SavedMessageSourcePlain {
            channel_id: "channel-123".to_string(),
            message_id: "message-456".to_string(),
            author_id: "user-789".to_string(),
            timestamp: 1234567890000,
        };

        let encrypted = SavedMessageCrypto::encrypt_source(&source, &profile_key)
            .expect("encryption should succeed");

        assert!(!encrypted.encrypted_source.is_empty());
        assert_eq!(encrypted.nonce.len(), NONCE_LENGTH);

        let decrypted = SavedMessageCrypto::decrypt_source(&encrypted, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, source);
    }

    #[test]
    fn test_different_plaintexts_different_ciphertexts() {
        let profile_key = test_profile_key();
        let plaintext = b"same text";

        let encrypted1 = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");
        let encrypted2 = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");

        // Same plaintext should produce different ciphertext (random nonce)
        assert_ne!(encrypted1.nonce, encrypted2.nonce);
        assert_ne!(encrypted1.ciphertext, encrypted2.ciphertext);
    }

    #[test]
    fn test_invalid_key_fails_decryption() {
        let profile_key = test_profile_key();
        let mut wrong_key = test_profile_key();
        wrong_key[0] ^= 0xFF;

        let plaintext = b"secret message";
        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");

        let result = SavedMessageCrypto::decrypt(&encrypted, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_auth_tag_fails_decryption() {
        let profile_key = test_profile_key();
        let plaintext = b"secret message";

        let mut encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");

        // Tamper with auth tag
        encrypted.auth_tag[0] ^= 0xFF;

        let result = SavedMessageCrypto::decrypt(&encrypted, &profile_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_plaintext() {
        let profile_key = test_profile_key();
        let plaintext = b"";

        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key)
            .expect("encryption should succeed");

        let decrypted = SavedMessageCrypto::decrypt(&encrypted, &profile_key)
            .expect("decryption should succeed");

        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_invalid_version_fails() {
        let profile_key = test_profile_key();

        let encrypted = EncryptedData {
            version: 999, // Invalid version
            nonce: vec![0u8; NONCE_LENGTH],
            ciphertext: vec![1, 2, 3],
            auth_tag: vec![0u8; TAG_LENGTH],
            associated_data: Vec::new(),
            signature: Vec::new(),
            device_id: String::new(),
        };

        let result = SavedMessageCrypto::decrypt(&encrypted, &profile_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_nonce_length_fails() {
        let profile_key = test_profile_key();

        let encrypted = EncryptedData {
            version: CryptoVersion::SimpleE2e as i32,
            nonce: vec![0u8; 12], // Wrong nonce length
            ciphertext: vec![1, 2, 3],
            auth_tag: vec![0u8; TAG_LENGTH],
            associated_data: Vec::new(),
            signature: Vec::new(),
            device_id: String::new(),
        };

        let result = SavedMessageCrypto::decrypt(&encrypted, &profile_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_empty_content_fails() {
        let profile_key = test_profile_key();

        let content = SavedMessageContent { r#type: None };

        let result = SavedMessageCrypto::decrypt_content(&content, &profile_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_large_plaintext() {
        let profile_key = test_profile_key();
        let plaintext: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        let encrypted = SavedMessageCrypto::encrypt(&plaintext, &profile_key)
            .expect("encryption should succeed");

        let decrypted = SavedMessageCrypto::decrypt(&encrypted, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }
}
