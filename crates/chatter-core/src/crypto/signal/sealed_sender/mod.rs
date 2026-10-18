//! Sealed sender for anonymous messaging.
//!
//! Sealed sender allows messages to be sent without revealing the sender's
//! identity to the server. The sender's identity is encrypted within the
//! message and can only be decrypted by the intended recipient.
//!
//! # UAK (Unidentified Access Key)
//!
//! The UAK is a 16-byte key derived from the user's 32-byte profile key.
//! It allows senders to send anonymous messages without revealing their
//! identity to the server. The server validates the UAK to ensure the
//! sender has permission to send to the recipient.
//!
//! # Certificate Validation
//!
//! Sender certificates are validated against a hardcoded trust root public key.
//! This prevents MITM attacks during message delivery by ensuring only
//! certificates signed by the trusted server are accepted.

// Re-export useful libsignal types for consumers of this module.
pub use libsignal_protocol::{
    ContentHint, SealedSenderDecryptionResult, SenderCertificate, ServerCertificate,
    UnidentifiedSenderMessageContent,
};

/// Sealed sender manager for anonymous messaging.
///
/// This manager provides methods for encrypting and decrypting sealed sender
/// messages, where the sender's identity is hidden from the server and only
/// revealed to the intended recipient.
///
/// # Design
///
/// Following the stateless pattern established in Phase 11, all methods take
/// the necessary stores as parameters rather than storing them in the struct.
/// This avoids mutable aliasing issues with libsignal's async trait methods.
///
/// # Workflow
///
/// ## Sending (Simple)
/// 1. Obtain a `SenderCertificate` from the server
/// 2. Call `encrypt()` with the plaintext and recipient address
/// 3. Send the encrypted bytes to the server
///
/// ## Sending (Advanced)
/// 1. Create the inner message ciphertext (WhisperMessage or PreKeyMessage)
/// 2. Call `create_content()` to build the USMC
/// 3. Call `encrypt_from_content()` for each recipient
///
/// ## Receiving
/// 1. Receive sealed sender message bytes
/// 2. Call `decrypt()` with trust root to validate and decrypt
/// 3. Get sender identity from `SealedSenderDecryptionResult`
#[derive(Clone)]
pub struct SealedSenderManager;

/// Mirrors the server's per-device sealed-body cap. It applies to the entire
/// serialized envelope, including the actual certificate, PQXDH and SPQR data.
pub(crate) const MAX_SEALED_BODY_BYTES: usize = 8192;

/// Exact cap comparison; equality is permitted by the server.
pub(crate) fn validate_sealed_body_size(
    size: usize,
    cap: usize,
) -> Result<(), crate::error::ServiceError> {
    if size > cap {
        return Err(crate::error::ServiceError::Validation(format!(
            "sealed envelope too large: {size} bytes (max {cap})"
        )));
    }
    Ok(())
}

mod certificate;
mod receive;
mod send;

pub use certificate::{
    CERTIFICATE_REFRESH_THRESHOLD, CachedSenderCertificate, DEFAULT_CERTIFICATE_VALIDITY,
    certificate_needs_refresh, validate_trust_root,
};

impl SealedSenderManager {
    /// Create a new SealedSenderManager.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SealedSenderManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// UAK (Unidentified Access Key) Functions
// ============================================================================

/// UAK length in bytes (Signal standard).
pub const UAK_LENGTH: usize = 16;

/// Profile key length in bytes.
pub const PROFILE_KEY_LENGTH: usize = 32;

// ============================================================================
// Multi-Device Sealed Sender Types
// ============================================================================

/// A sealed sender message prepared for a specific device.
///
/// When sending to a user with multiple devices, the sender must create
/// a separate encrypted envelope for each device.
#[derive(Clone)]
pub struct SealedMessageForDevice {
    /// Target device ID.
    pub device_id: u32,
    /// Target device's registration ID (for session verification).
    pub registration_id: u32,
    /// Encrypted sealed sender envelope for this device.
    pub sealed_content: Vec<u8>,
}

/// Collection of sealed sender messages for all of a recipient's devices.
#[derive(Clone)]
pub struct MultiDeviceSealedMessage {
    /// Target user ID.
    pub recipient_user_id: String,
    /// Per-device encrypted envelopes.
    pub device_messages: Vec<SealedMessageForDevice>,
    /// Combined UAK for anonymous delivery (single recipient UAK or group XOR).
    pub unidentified_access_key: [u8; UAK_LENGTH],
}

#[cfg(test)]
mod tests;
