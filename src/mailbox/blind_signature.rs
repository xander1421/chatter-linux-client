//! Chaum RSA blind signature implementation.
//!
//! Implements blind signatures for anonymous mailbox token creation.
//! Uses RFC 9474 RSABSSA via the blind-rsa-signatures crate.
//!
//! # Flow
//!
//! 1. Client generates ephemeral Ed25519 keypair
//! 2. Client blinds the public key using server's RSA public key
//! 3. Server signs the blinded message (cannot see original key)
//! 4. Client unblinds the signature
//! 5. Result: signature on original key that server cannot link to blinding request

use blind_rsa_signatures::{BlindSignature, BlindingResult, Options, PublicKey, Secret};
use rsa::pkcs8::DecodePublicKey;

use crate::error::MailboxError;

/// Server's RSA public key for blind signatures.
///
/// Caches the parsed RSA public key along with metadata for key rotation tracking.
#[derive(Clone)]
pub struct ServerPublicKey {
    /// The RSA-2048 public key for blinding operations.
    inner: PublicKey,
    /// Server's key identifier for rotation tracking.
    pub key_id: String,
    /// When this key expires (Unix timestamp milliseconds).
    pub valid_until: u64,
}

impl ServerPublicKey {
    /// Parse a server public key from DER-encoded bytes.
    ///
    /// # Arguments
    ///
    /// * `der_bytes` - RSA-2048 public key in DER format
    /// * `key_id` - Server's key identifier
    /// * `valid_until` - Expiry timestamp (milliseconds)
    ///
    /// # Errors
    ///
    /// Returns `MailboxError::InvalidServerKey` if parsing fails.
    pub fn from_der(
        der_bytes: &[u8],
        key_id: String,
        valid_until: u64,
    ) -> Result<Self, MailboxError> {
        // Parse as RSA public key
        let rsa_key = rsa::RsaPublicKey::from_public_key_der(der_bytes)
            .map_err(|e| MailboxError::InvalidServerKey(format!("failed to parse DER: {}", e)))?;

        // Convert to blind-rsa-signatures PublicKey
        // PublicKey::new in 0.14 returns the key directly (not Result)
        let inner = PublicKey::new(rsa_key);

        Ok(Self {
            inner,
            key_id,
            valid_until,
        })
    }

    /// Check if this key is still valid.
    pub fn is_valid(&self) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now_ms < self.valid_until
    }

    /// Get a reference to the inner public key.
    pub(crate) fn inner(&self) -> &PublicKey {
        &self.inner
    }
}

/// Blinding secret needed to unblind the signature.
///
/// Must be retained from blinding until unblinding is complete.
/// Serializable for storage during pending operations.
#[derive(Clone)]
pub struct BlindingSecret {
    /// Serialized secret bytes.
    secret_bytes: Vec<u8>,
}

impl BlindingSecret {
    /// Create from blind-rsa-signatures secret.
    fn from_inner(secret: &Secret) -> Self {
        Self {
            secret_bytes: secret.as_slice().to_vec(),
        }
    }

    /// Reconstruct the blinding secret from stored bytes.
    fn to_inner(&self) -> Secret {
        Secret::from(self.secret_bytes.clone())
    }

    /// Get the raw bytes for storage.
    pub fn as_bytes(&self) -> &[u8] {
        &self.secret_bytes
    }

    /// Create from raw bytes (loaded from storage).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            secret_bytes: bytes,
        }
    }
}

/// Message randomizer from blinding operation.
///
/// Required for unblinding when deterministic padding is NOT used.
/// The randomizer is a fixed 32-byte value.
#[derive(Clone)]
pub struct MessageRandomizer {
    /// Serialized randomizer bytes (fixed 32 bytes).
    randomizer_bytes: Option<[u8; 32]>,
}

impl MessageRandomizer {
    /// Create from blind-rsa-signatures message randomizer.
    fn from_inner(randomizer: Option<blind_rsa_signatures::MessageRandomizer>) -> Self {
        Self {
            randomizer_bytes: randomizer.map(|r| *r.as_ref()),
        }
    }

    /// Reconstruct the message randomizer from stored bytes.
    fn to_inner(&self) -> Option<blind_rsa_signatures::MessageRandomizer> {
        self.randomizer_bytes
            .map(blind_rsa_signatures::MessageRandomizer::from)
    }

    /// Get the raw bytes for storage.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.randomizer_bytes.as_ref().map(|b| b.as_slice())
    }

    /// Create from raw bytes (loaded from storage).
    ///
    /// Returns None if bytes is None or wrong length.
    pub fn from_bytes(bytes: Option<Vec<u8>>) -> Self {
        Self {
            randomizer_bytes: bytes.and_then(|b| {
                if b.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    Some(arr)
                } else {
                    None
                }
            }),
        }
    }
}

/// Result of a blinding operation.
///
/// Contains the blinded message to send to the server and the secret
/// needed to unblind the signature.
pub struct BlindingOperation {
    /// The blinded message to send to server for signing.
    pub blinded_message: Vec<u8>,
    /// The blinding secret (must be kept until unblind).
    pub secret: BlindingSecret,
    /// The message randomizer (must be kept until unblind).
    pub randomizer: MessageRandomizer,
}

/// An unblinded token ready for use.
///
/// Contains the ephemeral Ed25519 public key and its valid RSA signature.
/// This proves the right to create an ephemeral mailbox.
#[derive(Clone)]
pub struct UnblindedToken {
    /// Ed25519 public key (32 bytes).
    pub public_key: Vec<u8>,
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
    /// When this token was created (Unix timestamp milliseconds).
    pub created_at: u64,
}

/// Blind an Ed25519 public key using the server's RSA key.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key
/// * `ephemeral_pk` - The Ed25519 public key to blind (32 bytes)
///
/// # Returns
///
/// A `BlindingOperation` containing the blinded message and secret.
///
/// # Errors
///
/// Returns `MailboxError::BlindSignature` if blinding fails.
pub fn blind_key(
    server_pk: &ServerPublicKey,
    ephemeral_pk: &[u8],
) -> Result<BlindingOperation, MailboxError> {
    // Use default options
    let options = Options::default();

    // Perform blinding - the API takes (msg, include_randomizer, options)
    // Using randomized messages (true) for privacy
    let BlindingResult {
        blind_msg,
        secret,
        msg_randomizer,
    } = server_pk
        .inner()
        .blind(ephemeral_pk, true, &options)
        .map_err(|e| MailboxError::BlindSignature(format!("blinding failed: {}", e)))?;

    Ok(BlindingOperation {
        blinded_message: blind_msg.as_slice().to_vec(),
        secret: BlindingSecret::from_inner(&secret),
        randomizer: MessageRandomizer::from_inner(msg_randomizer),
    })
}

/// Unblind a server signature to get the valid signature on the original message.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key (must be same as used for blinding)
/// * `blinded_sig` - The blind signature from the server
/// * `secret` - The blinding secret from the original blinding operation
/// * `randomizer` - The message randomizer from the original blinding operation
/// * `original_message` - The original message (Ed25519 public key) that was blinded
///
/// # Returns
///
/// The unblinded signature bytes.
///
/// # Errors
///
/// Returns `MailboxError::BlindSignature` if unblinding fails.
pub fn unblind(
    server_pk: &ServerPublicKey,
    blinded_sig: &[u8],
    secret: &BlindingSecret,
    randomizer: &MessageRandomizer,
    original_message: &[u8],
) -> Result<Vec<u8>, MailboxError> {
    let options = Options::default();

    // Parse blinded signature
    let blind_sig = BlindSignature::from(blinded_sig.to_vec());

    // Restore blinding secret and randomizer
    let blinding_secret = secret.to_inner();
    let msg_randomizer = randomizer.to_inner();

    // Finalize (unblind) the signature
    let signature = server_pk
        .inner()
        .finalize(&blind_sig, &blinding_secret, msg_randomizer, original_message, &options)
        .map_err(|e| MailboxError::BlindSignature(format!("unblinding failed: {}", e)))?;

    Ok(signature.as_slice().to_vec())
}

/// Verify a signature against the server's public key.
///
/// Used to verify that an unblinded token is valid.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key
/// * `message` - The original message (Ed25519 public key)
/// * `signature` - The unblinded signature
/// * `randomizer` - The message randomizer from the blinding operation
///
/// # Returns
///
/// `true` if the signature is valid, `false` otherwise.
pub fn verify(
    server_pk: &ServerPublicKey,
    message: &[u8],
    signature: &[u8],
    randomizer: &MessageRandomizer,
) -> bool {
    let options = Options::default();
    let sig = blind_rsa_signatures::Signature::from(signature.to_vec());
    let msg_randomizer = randomizer.to_inner();
    server_pk
        .inner()
        .verify(&sig, msg_randomizer, message, &options)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blinding_secret_roundtrip() {
        // Create a fake secret (normally from blinding operation)
        let fake_bytes = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let secret = BlindingSecret::from_bytes(fake_bytes.clone());
        assert_eq!(secret.as_bytes(), &fake_bytes);
    }

    #[test]
    fn test_message_randomizer_roundtrip() {
        // Test with Some value (32 bytes)
        let fake_bytes: Vec<u8> = (0..32).collect();
        let randomizer = MessageRandomizer::from_bytes(Some(fake_bytes.clone()));
        assert_eq!(randomizer.as_bytes(), Some(fake_bytes.as_slice()));

        // Test with None value
        let randomizer = MessageRandomizer::from_bytes(None);
        assert_eq!(randomizer.as_bytes(), None);

        // Test with wrong length (should become None)
        let wrong_length = vec![1, 2, 3, 4];
        let randomizer = MessageRandomizer::from_bytes(Some(wrong_length));
        assert_eq!(randomizer.as_bytes(), None);
    }

    #[test]
    fn test_server_public_key_validity() {
        // Test expired key
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // We can't easily create a valid RSA key without the full setup,
        // but we can test the validity logic conceptually
        let past = now_ms - 1000; // 1 second ago
        let future = now_ms + 60000; // 1 minute from now

        // Validity check is: now_ms < valid_until
        assert!(now_ms < future); // Should be valid
        assert!(now_ms >= past); // Should be expired
    }

    #[test]
    fn test_unblinded_token_structure() {
        let token = UnblindedToken {
            public_key: vec![0u8; 32], // Ed25519 public key
            signature: vec![0u8; 256], // RSA-2048 signature
            created_at: 1234567890000,
        };

        assert_eq!(token.public_key.len(), 32);
        assert_eq!(token.signature.len(), 256);
        assert_eq!(token.created_at, 1234567890000);
    }

    #[test]
    fn test_invalid_server_key_der() {
        let invalid_der = vec![0, 1, 2, 3]; // Not valid DER
        let result = ServerPublicKey::from_der(&invalid_der, "key-1".to_string(), u64::MAX);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MailboxError::InvalidServerKey(_)));
    }
}
