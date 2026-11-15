//! Signal-compatible attachment crypto: AES-256-CBC + HMAC-SHA256.
//!
//! A crate of its own so the generic `cbc`/`aes` code is monomorphised here
//! and gets this crate's dev opt-level (see the workspace `Cargo.toml`), while
//! chatter-core builds at O0. Every public function is non-generic for that
//! reason.

mod attachment_crypto;
mod attachment_crypto_fast;
mod hmac_compat;
mod range_cipher;
mod secure_buffer;
mod streaming_decrypt;

pub use attachment_crypto::{
    AttachmentCrypto, EncryptedAttachment, KEY_SET_LENGTH, StreamingEncryptResult,
    max_attachment_size,
};
#[cfg(any(test, feature = "test-support"))]
pub use attachment_crypto::{MaxAttachmentSizeOverride, override_max_attachment_size};
pub use attachment_crypto_fast::decrypt_zero_copy;
pub use range_cipher::CbcRangeCipher;
pub use secure_buffer::{SecureBuffer, SecureCursor};
pub use streaming_decrypt::StreamingDecryptor;

/// Attachment crypto failure. chatter-core converts it to the `CryptoError`
/// variant of the same name; the messages are identical.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentCryptoError {
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

    /// Attachment exceeds maximum allowed size.
    #[error("attachment too large: {size} bytes exceeds {max} byte limit")]
    AttachmentTooLarge { size: u64, max: u64 },
}
