//! Sealed Sender envelope encryption for NDSS '21 mailbox system.
//!
//! Implements the transport layer encryption for ephemeral mailboxes.
//! The envelope is encrypted using ECDH between the sender's identity key
//! and the recipient's mailbox ephemeral key.
//!
//! This is STATELESS - no Signal session store required.
//!
//! # Protocol (NDSS '21)
//!
//! - Sender encrypts: `ECDH(sender_identity_private, recipient_mailbox_public)`
//! - Recipient decrypts: `ECDH(recipient_mailbox_private, sender_identity_public)`
//!
//! # Key Conversion
//!
//! Mailbox keys are Ed25519 (for signing). For ECDH, we convert to X25519
//! using the birational map defined in the XEdDSA specification:
//!
//! ```text
//! u = (1 + y) / (1 - y) mod p
//! ```
//!
//! Where y is the Ed25519 point and u is the X25519 point.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};

use crate::error::CryptoError;

/// Nonce size for XChaCha20-Poly1305 (24 bytes).
const NONCE_SIZE: usize = 24;

/// HKDF info string for envelope key derivation.
const ENVELOPE_HKDF_INFO: &[u8] = b"NDSS21-MailboxEnvelope-v1";

/// Convert an Ed25519 public key to X25519 public key.
///
/// Uses the birational map from the XEdDSA specification:
/// u = (1 + y) / (1 - y) mod p
///
/// # Arguments
///
/// * `ed25519_pk` - 32-byte Ed25519 public key
///
/// # Returns
///
/// 32-byte X25519 public key, or error if conversion fails.
pub fn ed25519_to_x25519_public(ed25519_pk: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    // Decompress the Ed25519 point
    let compressed = CompressedEdwardsY(*ed25519_pk);
    let point = compressed
        .decompress()
        .ok_or_else(|| CryptoError::InvalidKey("Invalid Ed25519 public key".into()))?;

    // Convert to Montgomery form (X25519)
    let montgomery = point.to_montgomery();
    Ok(montgomery.to_bytes())
}

/// Convert an Ed25519 private key (seed) to X25519 private key.
///
/// The Ed25519 seed is hashed with SHA-512, and the first 32 bytes
/// (after clamping) become the X25519 private key.
///
/// # Arguments
///
/// * `ed25519_seed` - 32-byte Ed25519 private key seed
///
/// # Returns
///
/// X25519 private key.
pub fn ed25519_to_x25519_private(ed25519_seed: &[u8; 32]) -> X25519PrivateKey {
    // Hash the seed with SHA-512 (same as Ed25519 key derivation)
    use sha2::{Digest, Sha512};
    let hash = Sha512::digest(ed25519_seed);

    // Take first 32 bytes and clamp (X25519 clamping is done by StaticSecret)
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&hash[..32]);

    X25519PrivateKey::from(scalar)
}

/// Encrypt an envelope for a mailbox recipient.
///
/// # Arguments
///
/// * `plaintext` - The content to encrypt (Signal ciphertext)
/// * `sender_identity_private` - Sender's X25519 identity private key
/// * `recipient_mailbox_public` - Recipient's mailbox public key (Ed25519, will be converted)
///
/// # Returns
///
/// Encrypted envelope: `sender_identity_public (32) || nonce (24) || ciphertext || tag (16)`
pub fn encrypt_envelope(
    plaintext: &[u8],
    sender_identity_private: &X25519PrivateKey,
    recipient_mailbox_public: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    // Convert recipient's Ed25519 mailbox key to X25519
    let recipient_x25519 = ed25519_to_x25519_public(recipient_mailbox_public)?;
    let recipient_pk = X25519PublicKey::from(recipient_x25519);

    // Compute ECDH shared secret
    let shared_secret = sender_identity_private.diffie_hellman(&recipient_pk);

    // Derive encryption key using HKDF
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut encryption_key = [0u8; 32];
    hkdf.expand(ENVELOPE_HKDF_INFO, &mut encryption_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("HKDF expand failed".into()))?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::fill(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    // Encrypt with XChaCha20-Poly1305
    let cipher = XChaCha20Poly1305::new_from_slice(&encryption_key)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Build envelope: sender_public (32) || nonce (24) || ciphertext
    let sender_public = X25519PublicKey::from(sender_identity_private);
    let mut envelope = Vec::with_capacity(32 + NONCE_SIZE + ciphertext.len());
    envelope.extend_from_slice(sender_public.as_bytes());
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);

    Ok(envelope)
}

/// Decrypt an envelope received at a mailbox.
///
/// # Arguments
///
/// * `envelope` - The encrypted envelope
/// * `recipient_mailbox_private` - Recipient's mailbox private key (Ed25519 seed, will be converted)
/// * `expected_sender_identity` - Optional: expected sender's X25519 identity public key for verification
///
/// # Returns
///
/// Tuple of (sender_identity_public, decrypted_plaintext)
pub fn decrypt_envelope(
    envelope: &[u8],
    recipient_mailbox_private: &[u8; 32],
    expected_sender_identity: Option<&[u8; 32]>,
) -> Result<([u8; 32], Vec<u8>), CryptoError> {
    // Minimum size: sender_pk (32) + nonce (24) + tag (16)
    if envelope.len() < 32 + NONCE_SIZE + 16 {
        return Err(CryptoError::DecryptionFailed("Envelope too short".into()));
    }

    // Extract sender's public key
    let mut sender_public = [0u8; 32];
    sender_public.copy_from_slice(&envelope[..32]);

    // Verify sender if expected
    if let Some(expected) = expected_sender_identity {
        if sender_public != *expected {
            return Err(CryptoError::DecryptionFailed(
                "Sender identity mismatch".into(),
            ));
        }
    }

    // Extract nonce and ciphertext
    let nonce = XNonce::from_slice(&envelope[32..32 + NONCE_SIZE]);
    let ciphertext = &envelope[32 + NONCE_SIZE..];

    // Convert recipient's Ed25519 private key to X25519
    let recipient_x25519 = ed25519_to_x25519_private(recipient_mailbox_private);

    // Compute ECDH shared secret
    let sender_pk = X25519PublicKey::from(sender_public);
    let shared_secret = recipient_x25519.diffie_hellman(&sender_pk);

    // Derive decryption key using HKDF
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut decryption_key = [0u8; 32];
    hkdf.expand(ENVELOPE_HKDF_INFO, &mut decryption_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("HKDF expand failed".into()))?;

    // Decrypt with XChaCha20-Poly1305
    let cipher = XChaCha20Poly1305::new_from_slice(&decryption_key)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    Ok((sender_public, plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn test_ed25519_to_x25519_conversion() {
        // Generate Ed25519 keypair
        let mut seed = [0u8; 32];
        rand::fill(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        // Convert public key
        let x25519_public = ed25519_to_x25519_public(verifying_key.as_bytes()).unwrap();
        assert_eq!(x25519_public.len(), 32);

        // Convert private key
        let x25519_private = ed25519_to_x25519_private(&seed);

        // Verify the public key matches what we'd get from the private key
        let derived_public = X25519PublicKey::from(&x25519_private);
        assert_eq!(x25519_public, *derived_public.as_bytes());
    }

    #[test]
    fn test_envelope_roundtrip() {
        // Generate sender identity (X25519)
        let mut sender_seed = [0u8; 32];
        rand::fill(&mut sender_seed);
        let sender_private = X25519PrivateKey::from(sender_seed);

        // Generate recipient mailbox (Ed25519)
        let mut recipient_seed = [0u8; 32];
        rand::fill(&mut recipient_seed);
        let recipient_signing = SigningKey::from_bytes(&recipient_seed);
        let recipient_public = recipient_signing.verifying_key();

        // Encrypt
        let plaintext = b"Hello, this is a secret message!";
        let envelope = encrypt_envelope(
            plaintext,
            &sender_private,
            recipient_public.as_bytes(),
        )
        .unwrap();

        // Decrypt
        let (sender_pk, decrypted) =
            decrypt_envelope(&envelope, &recipient_seed, None).unwrap();

        assert_eq!(decrypted, plaintext);

        // Verify sender public key
        let expected_sender = X25519PublicKey::from(&sender_private);
        assert_eq!(sender_pk, *expected_sender.as_bytes());
    }

    #[test]
    fn test_envelope_wrong_recipient() {
        // Generate sender identity
        let mut sender_seed = [0u8; 32];
        rand::fill(&mut sender_seed);
        let sender_private = X25519PrivateKey::from(sender_seed);

        // Generate recipient mailbox
        let mut recipient_seed = [0u8; 32];
        rand::fill(&mut recipient_seed);
        let recipient_signing = SigningKey::from_bytes(&recipient_seed);
        let recipient_public = recipient_signing.verifying_key();

        // Generate wrong recipient
        let mut wrong_seed = [0u8; 32];
        rand::fill(&mut wrong_seed);

        // Encrypt to correct recipient
        let plaintext = b"Secret message";
        let envelope = encrypt_envelope(
            plaintext,
            &sender_private,
            recipient_public.as_bytes(),
        )
        .unwrap();

        // Try to decrypt with wrong recipient - should fail
        let result = decrypt_envelope(&envelope, &wrong_seed, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_envelope_sender_verification() {
        // Generate sender identity
        let mut sender_seed = [0u8; 32];
        rand::fill(&mut sender_seed);
        let sender_private = X25519PrivateKey::from(sender_seed);
        let sender_public = X25519PublicKey::from(&sender_private);

        // Generate recipient mailbox
        let mut recipient_seed = [0u8; 32];
        rand::fill(&mut recipient_seed);
        let recipient_signing = SigningKey::from_bytes(&recipient_seed);
        let recipient_public = recipient_signing.verifying_key();

        // Encrypt
        let plaintext = b"Verified message";
        let envelope = encrypt_envelope(
            plaintext,
            &sender_private,
            recipient_public.as_bytes(),
        )
        .unwrap();

        // Decrypt with correct sender verification
        let (_, decrypted) = decrypt_envelope(
            &envelope,
            &recipient_seed,
            Some(sender_public.as_bytes()),
        )
        .unwrap();
        assert_eq!(decrypted, plaintext);

        // Decrypt with wrong sender verification - should fail
        let wrong_sender = [0u8; 32];
        let result = decrypt_envelope(&envelope, &recipient_seed, Some(&wrong_sender));
        assert!(result.is_err());
    }
}
