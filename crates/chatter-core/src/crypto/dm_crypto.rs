//! DM message encryption.
//!
//! Symmetric encryption for DM messages using the shared secret
//! derived from X25519 key exchange.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};

use crate::error::CryptoError;

/// Nonce size for XChaCha20-Poly1305 (24 bytes).
const NONCE_SIZE: usize = 24;

/// Encrypt a DM message using the shared secret.
///
/// Uses XChaCha20-Poly1305 for authenticated encryption.
/// The nonce is randomly generated and prepended to the ciphertext.
///
/// # Arguments
///
/// * `plaintext` - The message to encrypt
/// * `shared_secret` - 32-byte shared secret from X25519 key exchange
///
/// # Returns
///
/// Encrypted message: `nonce (24 bytes) || ciphertext || tag (16 bytes)`
pub fn encrypt_dm_message(plaintext: &str, shared_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    // Create cipher from shared secret
    let cipher = XChaCha20Poly1305::new_from_slice(shared_secret)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::fill(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    // Encrypt
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Prepend nonce to ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Encrypt raw bytes using the shared secret.
///
/// Uses XChaCha20-Poly1305 for authenticated encryption.
/// The nonce is randomly generated and prepended to the ciphertext.
///
/// # Arguments
///
/// * `plaintext` - The bytes to encrypt
/// * `shared_secret` - 32-byte shared secret from X25519 key exchange
///
/// # Returns
///
/// Encrypted bytes: `nonce (24 bytes) || ciphertext || tag (16 bytes)`
pub fn encrypt_dm_bytes(plaintext: &[u8], shared_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    // Create cipher from shared secret
    let cipher = XChaCha20Poly1305::new_from_slice(shared_secret)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::fill(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    // Encrypt
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Prepend nonce to ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypt raw bytes using the shared secret.
///
/// Expects the format: `nonce (24 bytes) || ciphertext || tag (16 bytes)`
///
/// # Arguments
///
/// * `encrypted` - The encrypted bytes (nonce + ciphertext + tag)
/// * `shared_secret` - 32-byte shared secret from X25519 key exchange
///
/// # Returns
///
/// Decrypted bytes.
pub fn decrypt_dm_bytes(encrypted: &[u8], shared_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    if encrypted.len() < NONCE_SIZE + 16 {
        // Minimum: nonce + tag
        return Err(CryptoError::DecryptionFailed(
            "Encrypted message too short".to_string(),
        ));
    }

    // Extract nonce and ciphertext
    let nonce = XNonce::from_slice(&encrypted[..NONCE_SIZE]);
    let ciphertext = &encrypted[NONCE_SIZE..];

    // Create cipher and decrypt
    let cipher = XChaCha20Poly1305::new_from_slice(shared_secret)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))
}

/// Decrypt a DM message using the shared secret.
///
/// Expects the format: `nonce (24 bytes) || ciphertext || tag (16 bytes)`
///
/// # Arguments
///
/// * `encrypted` - The encrypted message (nonce + ciphertext + tag)
/// * `shared_secret` - 32-byte shared secret from X25519 key exchange
///
/// # Returns
///
/// Decrypted message as a string.
pub fn decrypt_dm_message(encrypted: &[u8], shared_secret: &[u8; 32]) -> Result<String, CryptoError> {
    if encrypted.len() < NONCE_SIZE + 16 {
        // Minimum: nonce + tag
        return Err(CryptoError::DecryptionFailed(
            "Encrypted message too short".to_string(),
        ));
    }

    // Extract nonce and ciphertext
    let nonce = XNonce::from_slice(&encrypted[..NONCE_SIZE]);
    let ciphertext = &encrypted[NONCE_SIZE..];

    // Create cipher and decrypt
    let cipher = XChaCha20Poly1305::new_from_slice(shared_secret)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    String::from_utf8(plaintext)
        .map_err(|e| CryptoError::DecryptionFailed(format!("Invalid UTF-8: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let shared_secret = [0x42u8; 32];
        let message = "Hello, this is a secret DM!";

        let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();

        // Verify encrypted is different from plaintext
        assert_ne!(encrypted, message.as_bytes());

        // Verify we can decrypt
        let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();
        assert_eq!(decrypted, message);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts() {
        let shared_secret = [0x42u8; 32];
        let message = "Same message";

        let encrypted1 = encrypt_dm_message(message, &shared_secret).unwrap();
        let encrypted2 = encrypt_dm_message(message, &shared_secret).unwrap();

        // Different random nonces should produce different ciphertexts
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same message
        assert_eq!(
            decrypt_dm_message(&encrypted1, &shared_secret).unwrap(),
            message
        );
        assert_eq!(
            decrypt_dm_message(&encrypted2, &shared_secret).unwrap(),
            message
        );
    }

    #[test]
    fn test_wrong_key_fails() {
        let shared_secret1 = [0x42u8; 32];
        let shared_secret2 = [0x43u8; 32];
        let message = "Secret message";

        let encrypted = encrypt_dm_message(message, &shared_secret1).unwrap();

        // Decrypting with wrong key should fail
        let result = decrypt_dm_message(&encrypted, &shared_secret2);
        assert!(result.is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let shared_secret = [0x42u8; 32];
        let message = "Secret message";

        let mut encrypted = encrypt_dm_message(message, &shared_secret).unwrap();

        // Tamper with the ciphertext
        if let Some(byte) = encrypted.get_mut(NONCE_SIZE + 5) {
            *byte ^= 0xFF;
        }

        // Should fail authentication
        let result = decrypt_dm_message(&encrypted, &shared_secret);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_message() {
        let shared_secret = [0x42u8; 32];
        let message = "";

        let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();
        let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();

        assert_eq!(decrypted, message);
    }

    #[test]
    fn test_unicode_message() {
        let shared_secret = [0x42u8; 32];
        let message = "Hello 世界! 🚀 Привет";

        let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();
        let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();

        assert_eq!(decrypted, message);
    }
}
