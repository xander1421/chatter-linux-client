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
}
