//! HPKE encryption for NDSS'21 initial contact messages.
//!
//! This module provides standardized hybrid public key encryption (RFC 9180)
//! for initial anonymous DM contact messages as specified in NDSS'21.
//!
//! Unlike X3DH which requires fetching prekey bundles from a server, HPKE uses
//! only the long-term identity keys that are exchanged out-of-band (via friend code).
//!
//! # Security Properties
//!
//! - **Sender Authentication**: Not included in base mode (anonymous sender)
//! - **Confidentiality**: Encrypted to recipient's identity key
//! - **No Forward Secrecy on Initial Message**: Acceptable since initial message
//!   only contains ephemeral mailbox keys for Phase 2, NOT sensitive content
//! - **Post-Acceptance Forward Secrecy**: Full Double Ratchet sessions are
//!   established via mailbox exchange after acceptance
//!
//! # Wire Format (HPKE)
//!
//! ```text
//! | enc (32 bytes) | ciphertext (variable) |
//! ```
//!
//! Using DHKEM(X25519, HKDF-SHA256) + HKDF-SHA256 + ChaCha20Poly1305
//!
//! # Important Security Note
//!
//! Phase 1 (this HPKE message) does NOT provide Perfect Forward Secrecy.
//! If the recipient's long-term key is compromised, past Phase 1 messages
//! could be decrypted. Therefore, Phase 1 should ONLY contain:
//! - Ephemeral mailbox public key (pk_e)
//! - Identity key (for Phase 2 session establishment)
//! - Delivery token
//!
//! NO sensitive user data should be in Phase 1. PFS begins in Phase 2
//! when the Double Ratchet session is established.

use hpke::{
    aead::ChaCha20Poly1305,
    kdf::HkdfSha256,
    kem::X25519HkdfSha256,
    Deserializable, Kem, OpModeR, OpModeS, Serializable,
};
use rand_core_06::OsRng;

/// Type aliases for our HPKE configuration.
type HpkeKem = X25519HkdfSha256;
type HpkeKdf = HkdfSha256;
type HpkeAead = ChaCha20Poly1305;

/// HPKE encryption error.
#[derive(Debug, thiserror::Error)]
pub enum EciesError {
    /// Invalid key format.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption failed.
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    /// Invalid ciphertext format.
    #[error("invalid ciphertext: {0}")]
    InvalidCiphertext(String),

    /// HPKE error.
    #[error("HPKE error: {0}")]
    HpkeError(String),
}

/// HPKE encrypted message.
#[derive(Clone)]
pub struct EciesMessage {
    /// Encapsulated key (32 bytes for X25519).
    pub enc: Vec<u8>,
    /// Ciphertext with authentication tag.
    pub ciphertext: Vec<u8>,
}

impl EciesMessage {
    /// Minimum message size (enc + tag).
    pub const MIN_SIZE: usize = 32 + 16;

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.enc.len() + self.ciphertext.len());
        bytes.extend_from_slice(&self.enc);
        bytes.extend_from_slice(&self.ciphertext);
        bytes
    }

    /// Parse from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EciesError> {
        // X25519 encapsulated key is 32 bytes
        const ENC_SIZE: usize = 32;

        if bytes.len() < Self::MIN_SIZE {
            return Err(EciesError::InvalidCiphertext(format!(
                "too short: {} bytes, need at least {}",
                bytes.len(),
                Self::MIN_SIZE
            )));
        }

        let enc = bytes[..ENC_SIZE].to_vec();
        let ciphertext = bytes[ENC_SIZE..].to_vec();

        Ok(Self { enc, ciphertext })
    }
}

/// Encrypt plaintext to a recipient's identity key using HPKE (RFC 9180).
///
/// This performs encapsulation and authenticated encryption using:
/// - KEM: DHKEM(X25519, HKDF-SHA256)
/// - KDF: HKDF-SHA256
/// - AEAD: ChaCha20Poly1305
///
/// # Arguments
///
/// * `recipient_public_key` - Recipient's X25519 public key (32 bytes)
/// * `plaintext` - Data to encrypt
///
/// # Returns
///
/// HPKE encrypted message containing encapsulated key and ciphertext.
pub fn encrypt(
    recipient_public_key: &[u8; 32],
    plaintext: &[u8],
) -> Result<EciesMessage, EciesError> {
    // Parse recipient's public key
    let recipient_pk = <HpkeKem as Kem>::PublicKey::from_bytes(recipient_public_key)
        .map_err(|e| EciesError::InvalidKey(format!("invalid recipient key: {:?}", e)))?;

    // Info string for domain separation
    let info = b"Chatter-AnonDM-InitContact-v1";

    // Setup sender context (Base mode - no sender authentication)
    // Use rand_core 0.6's OsRng for HPKE compatibility
    let mut csprng = OsRng;
    let (enc, mut sender_ctx) = hpke::setup_sender::<HpkeAead, HpkeKdf, HpkeKem, _>(
        &OpModeS::Base,
        &recipient_pk,
        info,
        &mut csprng,
    )
    .map_err(|e| EciesError::HpkeError(format!("setup_sender failed: {:?}", e)))?;

    // Encrypt with empty AAD (additional authenticated data)
    let ciphertext = sender_ctx
        .seal(plaintext, &[])
        .map_err(|e| EciesError::EncryptionFailed(format!("seal failed: {:?}", e)))?;

    Ok(EciesMessage {
        enc: enc.to_bytes().to_vec(),
        ciphertext,
    })
}

/// Decrypt an HPKE message using our identity secret key.
///
/// # Arguments
///
/// * `our_secret_key` - Our X25519 private key (32 bytes)
/// * `message` - HPKE encrypted message
///
/// # Returns
///
/// Decrypted plaintext.
pub fn decrypt(
    our_secret_key: &[u8; 32],
    message: &EciesMessage,
) -> Result<Vec<u8>, EciesError> {
    // Parse our secret key
    let our_sk = <HpkeKem as Kem>::PrivateKey::from_bytes(our_secret_key)
        .map_err(|e| EciesError::InvalidKey(format!("invalid secret key: {:?}", e)))?;

    // Parse encapsulated key
    let enc = <HpkeKem as Kem>::EncappedKey::from_bytes(&message.enc)
        .map_err(|e| EciesError::InvalidCiphertext(format!("invalid enc: {:?}", e)))?;

    // Info string for domain separation (must match encryption)
    let info = b"Chatter-AnonDM-InitContact-v1";

    // Setup receiver context
    let mut receiver_ctx = hpke::setup_receiver::<HpkeAead, HpkeKdf, HpkeKem>(
        &OpModeR::Base,
        &our_sk,
        &enc,
        info,
    )
    .map_err(|e| EciesError::HpkeError(format!("setup_receiver failed: {:?}", e)))?;

    // Decrypt with empty AAD
    let plaintext = receiver_ctx
        .open(&message.ciphertext, &[])
        .map_err(|e| EciesError::DecryptionFailed(format!("open failed: {:?}", e)))?;

    Ok(plaintext)
}

/// Convert a Signal Protocol identity key (33 bytes with 0x05 prefix) to X25519 format.
///
/// Signal uses Curve25519 keys with a type byte prefix. We extract the 32-byte
/// public key for X25519 ECDH.
pub fn signal_identity_to_x25519(signal_key: &[u8]) -> Result<[u8; 32], EciesError> {
    if signal_key.len() != 33 {
        return Err(EciesError::InvalidKey(format!(
            "expected 33 bytes, got {}",
            signal_key.len()
        )));
    }

    if signal_key[0] != 0x05 {
        return Err(EciesError::InvalidKey(format!(
            "expected type byte 0x05, got 0x{:02x}",
            signal_key[0]
        )));
    }

    // Extract the 32-byte public key (skip type byte)
    let mut key = [0u8; 32];
    key.copy_from_slice(&signal_key[1..33]);

    Ok(key)
}

/// Convert X25519 public key to Signal Protocol format (33 bytes with 0x05 prefix).
pub fn x25519_to_signal_identity(x25519_key: &[u8; 32]) -> [u8; 33] {
    let mut signal_key = [0u8; 33];
    signal_key[0] = 0x05; // Curve25519 type byte
    signal_key[1..33].copy_from_slice(x25519_key);
    signal_key
}

/// Generate a new X25519 keypair for ephemeral use.
pub fn generate_x25519_keypair() -> ([u8; 32], [u8; 32]) {
    // Use rand_core 0.6's OsRng for HPKE compatibility
    let mut csprng = OsRng;
    let (secret_key, public_key) = HpkeKem::gen_keypair(&mut csprng);

    let mut sk_bytes = [0u8; 32];
    let mut pk_bytes = [0u8; 32];

    let sk_vec = secret_key.to_bytes();
    let pk_vec = public_key.to_bytes();

    sk_bytes.copy_from_slice(&sk_vec);
    pk_bytes.copy_from_slice(&pk_vec);

    (sk_bytes, pk_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hpke_roundtrip() {
        // Generate recipient keypair
        let (recipient_secret, recipient_public) = generate_x25519_keypair();

        let plaintext = b"Hello, this is an initial contact message!";

        // Encrypt
        let message = encrypt(&recipient_public, plaintext).unwrap();

        // Decrypt
        let decrypted = decrypt(&recipient_secret, &message).unwrap();

        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_hpke_message_serialization() {
        let (recipient_secret, recipient_public) = generate_x25519_keypair();

        let plaintext = b"Test message for serialization";
        let message = encrypt(&recipient_public, plaintext).unwrap();

        // Serialize and deserialize
        let bytes = message.to_bytes();
        let parsed = EciesMessage::from_bytes(&bytes).unwrap();

        assert_eq!(message.enc, parsed.enc);
        assert_eq!(message.ciphertext, parsed.ciphertext);

        // Verify decryption still works
        let decrypted = decrypt(&recipient_secret, &parsed).unwrap();
        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_signal_key_conversion() {
        let (_, x25519_key) = generate_x25519_keypair();

        let signal_key = x25519_to_signal_identity(&x25519_key);
        assert_eq!(signal_key[0], 0x05);
        assert_eq!(&signal_key[1..33], &x25519_key);

        let converted_back = signal_identity_to_x25519(&signal_key).unwrap();
        assert_eq!(x25519_key, converted_back);
    }

    #[test]
    fn test_wrong_key_fails() {
        let (_, recipient_public) = generate_x25519_keypair();

        let plaintext = b"Secret message";
        let message = encrypt(&recipient_public, plaintext).unwrap();

        // Try to decrypt with wrong key
        let (wrong_secret, _) = generate_x25519_keypair();

        let result = decrypt(&wrong_secret, &message);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_plaintext() {
        let (recipient_secret, recipient_public) = generate_x25519_keypair();

        let plaintext = b"";
        let message = encrypt(&recipient_public, plaintext).unwrap();
        let decrypted = decrypt(&recipient_secret, &message).unwrap();

        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_large_plaintext() {
        let (recipient_secret, recipient_public) = generate_x25519_keypair();

        // 1MB of data
        let plaintext: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
        let message = encrypt(&recipient_public, &plaintext).unwrap();
        let decrypted = decrypt(&recipient_secret, &message).unwrap();

        assert_eq!(plaintext, decrypted);
    }
}
