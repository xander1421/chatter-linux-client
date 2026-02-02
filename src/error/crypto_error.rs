//! Crypto-specific errors.
//!
//! Errors related to cryptographic operations.

/// Crypto error.
pub enum CryptoError {
    /// Encryption failed.
    EncryptionFailed,
    /// Decryption failed.
    DecryptionFailed,
    /// Key not found.
    KeyNotFound,
    /// Invalid signature.
    InvalidSignature,
}
