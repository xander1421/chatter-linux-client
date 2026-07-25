//! Errors for the mesh crypto layer.
//!
//! Deliberately coarse on the failure side. A decryption failure must not tell a
//! caller *why* it failed — whether a tag mismatched, a key was wrong, or a nonce
//! repeated — because that distinction is exactly what a padding-oracle style attack
//! feeds on. `Decrypt` covers all of it.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NoiseError {
    /// Handshake could not be constructed — bad pattern, bad key material.
    #[error("handshake setup failed: {0}")]
    Setup(String),

    /// A handshake message was rejected.
    #[error("handshake failed")]
    Handshake,

    /// Encryption failed (buffer limits, exhausted nonce space).
    #[error("encrypt failed")]
    Encrypt,

    /// Decryption or authentication failed. Deliberately not specific.
    #[error("decrypt failed")]
    Decrypt,

    /// A session operation was attempted before the handshake completed, or a
    /// handshake operation after it did.
    #[error("wrong state: {0}")]
    WrongState(&'static str),

    /// The remote static key was required but the pattern had not yet revealed it.
    #[error("remote static key unavailable")]
    MissingRemoteStatic,

    /// A public key was not a valid Curve25519 point, or had the wrong length.
    #[error("invalid public key")]
    InvalidPublicKey,

    /// A signature was malformed or did not verify.
    #[error("invalid signature")]
    InvalidSignature,

    /// A prekey ID had no matching private key on this device.
    #[error("unknown prekey {0}")]
    UnknownPrekey(u32),

    /// A prekey bundle was structurally invalid.
    #[error("malformed prekey bundle: {0}")]
    MalformedBundle(&'static str),

    /// Nonce space for this session is exhausted; it must be rekeyed.
    #[error("session exhausted, rekey required")]
    SessionExhausted,

    /// Propagated from the wire layer when building a signing preimage.
    #[error("wire encoding failed: {0}")]
    Wire(#[from] chatter_mesh::MeshError),
}
