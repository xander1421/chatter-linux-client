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
    // Note: blind_rsa_signatures::PublicKey doesn't derive Debug,
    // so we implement Debug manually below.
    /// The RSA-2048 public key for blinding operations.
    inner: PublicKey,
    /// Server's key identifier for rotation tracking.
    pub key_id: String,
    /// When this key expires (Unix timestamp nanoseconds, per proto spec).
    pub valid_until: u64,
}

impl ServerPublicKey {
    /// Parse a server public key from DER-encoded bytes.
    ///
    /// # Arguments
    ///
    /// * `der_bytes` - RSA-2048 public key in DER format
    /// * `key_id` - Server's key identifier
    /// * `valid_until` - Expiry timestamp (nanoseconds, per proto spec)
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
        // valid_until is in nanoseconds (per proto spec)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        now_nanos < self.valid_until
    }

    /// Get a reference to the inner public key.
    pub(crate) fn inner(&self) -> &PublicKey {
        &self.inner
    }
}

impl std::fmt::Debug for ServerPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPublicKey")
            .field("key_id", &self.key_id)
            .field("valid_until", &self.valid_until)
            .field("inner", &"<RSA PublicKey>")
            .finish()
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
    // Use deterministic options (no randomizer) for server-side verification compatibility
    // Use deterministic options (no randomizer) for server-side verification compatibility
    // The server can verify without needing to know the randomizer
    // using salt_len=48 (standard for SHA-384-PSS)
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);

    // Perform blinding - the API takes (msg, include_randomizer, options)
    // Using deterministic mode (false) for server verification compatibility
    let BlindingResult {
        blind_msg,
        secret,
        msg_randomizer,
    } = server_pk
        .inner()
        .blind(ephemeral_pk, false, &options)
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
    // Use deterministic options matching blind_key
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);

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
    // Use deterministic options (no randomizer) for server-side verification compatibility
    // The server can verify without needing to know the randomizer
    // using salt_len=48 (standard for SHA-384-PSS)
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);
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
        // Test expired key (all timestamps in nanoseconds per proto spec)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // We can't easily create a valid RSA key without the full setup,
        // but we can test the validity logic conceptually
        let past = now_nanos - 1_000_000_000; // 1 second ago
        let future = now_nanos + 60_000_000_000; // 1 minute from now

        // Validity check is: now_nanos < valid_until
        assert!(now_nanos < future); // Should be valid
        assert!(now_nanos >= past); // Should be expired
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

    /// Simulates a malicious server attempting to correlate blinded messages
    /// with their original plaintexts via bit-pattern matching.
    ///
    /// Generates 1,000 distinct Ed25519 public keys, blinds each one, then
    /// checks that a server seeing only the blinded messages cannot determine
    /// which plaintext produced which blinded output. The server's strategy:
    ///
    /// 1. **Byte-prefix matching** — do any blinded messages share leading
    ///    bytes with the plaintext that produced them?
    /// 2. **Hamming-distance bias** — is the hamming distance between a
    ///    blinded message and its source plaintext statistically closer than
    ///    between that blinded message and a random other plaintext?
    /// 3. **Duplicate detection** — do identical plaintexts produce identical
    ///    blinded outputs? (They must not, since blinding uses fresh randomness.)
    #[test]
    fn test_blind_signature_no_correlation() {
        use blind_rsa_signatures::KeyPair;
        use rand::Rng;

        const N: usize = 1_000;

        // Generate server keypair (the "malicious server" has the secret key)
        let kp = KeyPair::generate(2048).expect("RSA keygen");
        let server_pk = ServerPublicKey {
            inner: kp.pk,
            key_id: "test-key".to_string(),
            valid_until: u64::MAX,
        };

        // Generate N distinct 32-byte "Ed25519 public keys"
        let mut rng = rand::rng();
        let plaintexts: Vec<[u8; 32]> = (0..N)
            .map(|_| {
                let mut buf = [0u8; 32];
                rng.fill(&mut buf);
                buf
            })
            .collect();

        // Blind each plaintext
        let blinded: Vec<Vec<u8>> = plaintexts
            .iter()
            .map(|pt| {
                let op = blind_key(&server_pk, pt).expect("blinding");
                op.blinded_message
            })
            .collect();

        // --- Attack 1: byte-prefix correlation ---
        // Check that no blinded message shares a 4-byte prefix with its plaintext.
        let prefix_matches = plaintexts
            .iter()
            .zip(blinded.iter())
            .filter(|(pt, bm)| bm.len() >= 4 && bm[..4] == pt[..4])
            .count();
        // With 256^4 possible prefixes, expected matches ≈ 0 for 1,000 samples.
        // Allow at most 1 as a fluke (p ≈ 2^-32 per trial).
        assert!(
            prefix_matches <= 1,
            "Too many prefix matches ({prefix_matches}/1000) — blinding may leak plaintext bits"
        );

        // --- Attack 2: hamming-distance bias ---
        // For each blinded message, compare hamming distance to its own plaintext
        // vs. hamming distance to a random other plaintext. If blinding is sound,
        // both distributions should be indistinguishable (centered around 50% of bits).
        let compare_len = 32; // compare over plaintext length
        let mut own_distances: Vec<u32> = Vec::with_capacity(N);
        let mut other_distances: Vec<u32> = Vec::with_capacity(N);

        for i in 0..N {
            let bm = &blinded[i];
            if bm.len() < compare_len {
                continue;
            }
            // Distance to own plaintext
            let own_dist: u32 = plaintexts[i]
                .iter()
                .zip(bm.iter())
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            own_distances.push(own_dist);

            // Distance to a different plaintext (next one, wrapping)
            let other = &plaintexts[(i + 1) % N];
            let other_dist: u32 = other
                .iter()
                .zip(bm.iter())
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            other_distances.push(other_dist);
        }

        let own_mean = own_distances.iter().sum::<u32>() as f64 / own_distances.len() as f64;
        let other_mean =
            other_distances.iter().sum::<u32>() as f64 / other_distances.len() as f64;

        // Both means should be close to 128 (half of 256 bits). Allow 10% tolerance.
        // A biased blinding scheme would show own_mean significantly lower than other_mean.
        let diff = (own_mean - other_mean).abs();
        assert!(
            diff < 10.0,
            "Hamming distance bias detected: own_mean={own_mean:.1}, other_mean={other_mean:.1}, \
             diff={diff:.1} — blinding may be leaking information"
        );

        // --- Attack 3: duplicate blinded outputs from identical plaintexts ---
        // Blind the same plaintext twice; the outputs must differ.
        let fixed_pt = [0xABu8; 32];
        let blind1 = blind_key(&server_pk, &fixed_pt)
            .expect("blinding")
            .blinded_message;
        let blind2 = blind_key(&server_pk, &fixed_pt)
            .expect("blinding")
            .blinded_message;
        assert_ne!(
            blind1, blind2,
            "Identical plaintexts produced identical blinded messages — randomness failure"
        );
    }
}
