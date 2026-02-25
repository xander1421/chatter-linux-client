//! Crypto-specific errors.
//!
//! Errors related to cryptographic operations including Signal Protocol.

use thiserror::Error;

use super::StorageError;

/// Crypto error types.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// Storage layer error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// No local identity key has been generated.
    #[error("no local identity")]
    NoIdentity,

    /// Signal protocol error.
    #[error("signal protocol error: {0}")]
    Protocol(String),

    /// Invalid key format or corrupted key data.
    #[error("invalid key format")]
    InvalidKeyFormat,

    /// Invalid key length provided.
    #[error("invalid key length: expected {expected}, got {got}")]
    InvalidKeyLength {
        /// Expected key length in bytes.
        expected: usize,
        /// Actual key length in bytes.
        got: usize,
    },

    /// Key derivation operation failed.
    #[error("key derivation failed: {0}")]
    KeyDerivationFailed(String),

    /// Untrusted identity key for a remote user.
    #[error("untrusted identity for {0}")]
    UntrustedIdentity(String),

    /// Session not found for the given address.
    #[error("session not found for {0}")]
    SessionNotFound(String),

    /// Encryption operation failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption operation failed.
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    /// Key not found in store.
    #[error("key not found: {0}")]
    KeyNotFound(String),

    /// Invalid signature.
    #[error("invalid signature")]
    InvalidSignature,

    /// Fingerprint generation failed.
    #[error("fingerprint generation failed: {0}")]
    FingerprintFailed(String),

    /// Invalid or malformed sealed sender message.
    #[error("invalid sealed sender message: {0}")]
    InvalidSealedSenderMessage(String),

    /// Attempted to decrypt own sealed sender message.
    #[error("cannot decrypt sealed sender message from self")]
    SealedSenderSelfSend,

    /// Sender certificate validation failed.
    #[error("invalid sender certificate: {0}")]
    InvalidSenderCertificate(String),

    /// Server certificate ID not recognized.
    #[error("unknown server certificate: {0}")]
    UnknownServerCertificate(u32),

    // =========================================================================
    // Attachment Encryption Errors
    // =========================================================================
    /// Attachment encryption operation failed.
    #[error("attachment encryption failed: {0}")]
    AttachmentEncryptionFailed(String),

    /// Attachment decryption operation failed.
    #[error("attachment decryption failed: {0}")]
    AttachmentDecryptionFailed(String),

    /// Attachment MAC verification failed (tampered or corrupted).
    #[error("attachment MAC mismatch")]
    AttachmentMacMismatch,

    /// Attachment digest verification failed.
    #[error("attachment digest mismatch")]
    AttachmentDigestMismatch,

    // =========================================================================
    // Blind Signature Errors (NDSS '21 Ephemeral Mailboxes)
    // =========================================================================
    /// Blind signature operation failed (blinding/unblinding).
    #[error("blind signature error: {0}")]
    BlindSignatureError(String),

    /// Invalid key provided (RSA public key parsing, Ed25519 format, etc.).
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// Validation error (e.g., invalid parameters, bounds exceeded).
    #[error("validation error: {0}")]
    Validation(String),
}
