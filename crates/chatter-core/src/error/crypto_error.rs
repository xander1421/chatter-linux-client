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

    /// Session exists but is not usable (expired, corrupted, or invalid state).
    #[error("session unusable for {address}: {reason}")]
    SessionUnusable {
        /// The remote address.
        address: String,
        /// Why the session is unusable.
        reason: String,
    },

    /// Session ratchet has reached a state where it cannot be used for sending.
    #[error("session expired for {0}")]
    SessionExpired(String),

    /// The inner message had already been decrypted once — libsignal's
    /// `DuplicatedMessage`, raised when the ratchet chain has advanced past the
    /// counter the message carries and its message keys are gone.
    ///
    /// **Not** a [`Self::SessionDesync`], although it arrives down the same path.
    /// A duplicate is a re-delivery: the session is healthy and the plaintext was
    /// already handled, so the message is dropped. Reporting it as a desync sends
    /// the peer a `DecryptionErrorMessage` asking it to tear down a working
    /// session and resend — which produces more traffic, more re-deliveries, and
    /// more reports. Measured before this was split out: 201 inbound reports and
    /// 12 outbound in four minutes between two idle devices.
    ///
    /// libsignal treats it as routine too, logging `Duplicate message for
    /// counter: N` at info (`session_cipher.rs`).
    #[error("duplicate message from {sender_uuid}:{sender_device_id} (already decrypted)")]
    DuplicateMessage {
        /// Authenticated sender user id.
        sender_uuid: String,
        /// Authenticated sender device id.
        sender_device_id: u32,
    },

    /// A sealed-sender envelope opened cleanly (sender authenticated via the
    /// certificate) but the INNER Whisper/PreKey message failed to decrypt —
    /// the classic desynced-1:1-session wedge. Carries everything needed to
    /// build a Signal `DecryptionErrorMessage` back to the sender so the
    /// session self-heals (see `MessageGateway::process_incoming_message`).
    #[error("session desync with {sender_uuid}:{sender_device_id}: {reason}")]
    SessionDesync {
        /// Authenticated sender user id (from the validated sender certificate).
        sender_uuid: String,
        /// Authenticated sender device id.
        sender_device_id: u32,
        /// Inner `CiphertextMessageType` discriminant (2 = Whisper, 3 = PreKey).
        msg_type: u8,
        /// The failed inner ciphertext (serialized Whisper/PreKey message).
        ciphertext: Vec<u8>,
        /// Underlying libsignal error string.
        reason: String,
    },

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

    /// Incremental MAC verification failed at a specific chunk.
    #[error("incremental MAC mismatch at chunk {chunk_index}")]
    IncrementalMacMismatch { chunk_index: usize },

    /// Attachment exceeds maximum allowed size.
    #[error("attachment too large: {size} bytes exceeds {max} byte limit")]
    AttachmentTooLarge { size: u64, max: u64 },

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
