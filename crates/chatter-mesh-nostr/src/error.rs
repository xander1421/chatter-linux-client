//! Errors for the Nostr transport.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NostrError {
    #[error("invalid public key")]
    InvalidPublicKey,

    #[error("invalid private key")]
    InvalidPrivateKey,

    /// Event was structurally invalid, or its `id` did not match its content.
    #[error("invalid event: {0}")]
    InvalidEvent(&'static str),

    #[error("invalid signature")]
    InvalidSignature,

    /// Ciphertext was malformed: missing the `v2:` prefix, bad base64url, or too
    /// short to contain a nonce and tag.
    #[error("invalid ciphertext")]
    InvalidCiphertext,

    /// Decryption or authentication failed. Deliberately not more specific.
    #[error("decrypt failed")]
    Decrypt,

    #[error("encrypt failed")]
    Encrypt,

    /// JSON serialization produced something unusable, or a relay sent malformed
    /// JSON.
    #[error("json error: {0}")]
    Json(String),

    #[error("invalid geohash: {0}")]
    InvalidGeohash(&'static str),

    /// A presence heartbeat was requested for a geohash finer than precision 5.
    /// This is a privacy guarantee, not a tuning knob — see the P4 design §4.7.
    #[error("presence is not permitted at geohash precision {0}; maximum is 5")]
    PresencePrecisionTooFine(usize),

    #[error("bech32 error: {0}")]
    Bech32(String),

    #[error("relay error: {0}")]
    Relay(String),

    #[error("wire encoding failed: {0}")]
    Wire(#[from] chatter_mesh::MeshError),
}
