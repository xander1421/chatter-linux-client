//! Saved message encryption/decryption.
//!
//! Client-side encryption for saved messages using XChaCha20-Poly1305.
//! Encrypts content with the user's profile key before sending to the
//! saved messages channel (channel_id == user_id).

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::CryptoError;
use crate::proto::chat_v1::{message, CryptoVersion, EncryptedData};

/// XChaCha20-Poly1305 nonce length (24 bytes).
const NONCE_LENGTH: usize = 24;

/// Key length (256-bit).
const KEY_LENGTH: usize = 32;

/// Poly1305 authentication tag length.
const TAG_LENGTH: usize = 16;

/// Plaintext source metadata (before encryption).
///
/// This struct is serialized to JSON and stored in associated_data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedMessageSource {
    /// Original channel ID where message was saved from.
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
/// Content is encrypted with the user's profile key before sending to the
/// saved messages channel.
///
/// # Example
///
/// ```ignore
/// let profile_key = [0u8; 32]; // Get from key storage
///
/// // Encrypt text for saving
/// let content = SavedMessageCrypto::encrypt_text("Hello", &profile_key, None)?;
///
/// // Decrypt after fetching
/// let (text, source) = SavedMessageCrypto::decrypt_content(&content, &profile_key)?;
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
    /// * `associated_data` - Optional AAD (e.g., serialized source metadata)
    pub fn encrypt(
        plaintext: &[u8],
        profile_key: &[u8; KEY_LENGTH],
        associated_data: Option<&[u8]>,
    ) -> Result<EncryptedData, CryptoError> {
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
            associated_data: associated_data.map(|a| a.to_vec()).unwrap_or_default(),
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
    pub fn decrypt(
        encrypted: &EncryptedData,
        profile_key: &[u8; KEY_LENGTH],
    ) -> Result<Vec<u8>, CryptoError> {
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

    /// Encrypt text content for saving to saved messages channel.
    ///
    /// Returns message::Content with Encrypted variant.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to encrypt
    /// * `profile_key` - 32-byte encryption key
    /// * `source` - Optional source metadata (where the message came from)
    pub fn encrypt_text(
        text: &str,
        profile_key: &[u8; KEY_LENGTH],
        source: Option<&SavedMessageSource>,
    ) -> Result<message::Content, CryptoError> {
        // Serialize source to JSON if provided (stored in associated_data)
        let associated_data = source
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| CryptoError::EncryptionFailed(format!("failed to serialize source: {}", e)))?;

        let encrypted = Self::encrypt(
            text.as_bytes(),
            profile_key,
            associated_data.as_deref(),
        )?;

        Ok(message::Content {
            r#type: Some(message::content::Type::Encrypted(encrypted)),
        })
    }

    /// Decrypt message content from saved messages channel.
    ///
    /// Returns the decrypted text and optional source metadata.
    ///
    /// # Arguments
    ///
    /// * `content` - The message::Content to decrypt
    /// * `profile_key` - 32-byte decryption key
    pub fn decrypt_content(
        content: &message::Content,
        profile_key: &[u8; KEY_LENGTH],
    ) -> Result<(String, Option<SavedMessageSource>), CryptoError> {
        match &content.r#type {
            Some(message::content::Type::Encrypted(encrypted)) => {
                let plaintext = Self::decrypt(encrypted, profile_key)?;

                let text = String::from_utf8(plaintext)
                    .map_err(|e| CryptoError::DecryptionFailed(format!("invalid UTF-8: {}", e)))?;

                // Try to deserialize source from associated_data
                let source = if !encrypted.associated_data.is_empty() {
                    serde_json::from_slice(&encrypted.associated_data).ok()
                } else {
                    None
                };

                Ok((text, source))
            }
            Some(message::content::Type::Text(text)) => {
                // Unencrypted text (shouldn't happen for saved messages, but handle gracefully)
                Ok((text.clone(), None))
            }
            _ => Err(CryptoError::DecryptionFailed(
                "unsupported content type for saved message".to_string(),
            )),
        }
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

        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
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

        let content = SavedMessageCrypto::encrypt_text(text, &profile_key, None)
            .expect("encryption should succeed");

        let (decrypted, source) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, text);
        assert!(source.is_none());
    }

    #[test]
    fn test_encrypt_text_with_source_roundtrip() {
        let profile_key = test_profile_key();
        let text = "Saved from another channel";
        let source = SavedMessageSource {
            channel_id: "channel-123".to_string(),
            message_id: "message-456".to_string(),
            author_id: "user-789".to_string(),
            timestamp: 1234567890000,
        };

        let content = SavedMessageCrypto::encrypt_text(text, &profile_key, Some(&source))
            .expect("encryption should succeed");

        let (decrypted, decrypted_source) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, text);
        assert_eq!(decrypted_source, Some(source));
    }

    #[test]
    fn test_different_plaintexts_different_ciphertexts() {
        let profile_key = test_profile_key();
        let plaintext = b"same text";

        let encrypted1 = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
            .expect("encryption should succeed");
        let encrypted2 = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
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
        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
            .expect("encryption should succeed");

        let result = SavedMessageCrypto::decrypt(&encrypted, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_auth_tag_fails_decryption() {
        let profile_key = test_profile_key();
        let plaintext = b"secret message";

        let mut encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
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

        let encrypted = SavedMessageCrypto::encrypt(plaintext, &profile_key, None)
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
    fn test_large_plaintext() {
        let profile_key = test_profile_key();
        let plaintext: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        let encrypted = SavedMessageCrypto::encrypt(&plaintext, &profile_key, None)
            .expect("encryption should succeed");

        let decrypted = SavedMessageCrypto::decrypt(&encrypted, &profile_key)
            .expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }
}
