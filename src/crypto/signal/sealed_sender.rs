//! Sealed sender for anonymous messaging.
//!
//! Sealed sender allows messages to be sent without revealing the sender's
//! identity to the server. The sender's identity is encrypted within the
//! message and can only be decrypted by the intended recipient.

use std::time::SystemTime;

use libsignal_protocol::{
    sealed_sender_decrypt, sealed_sender_decrypt_to_usmc, sealed_sender_encrypt,
    sealed_sender_encrypt_from_usmc, CiphertextMessageType, DeviceId, IdentityKeyStore,
    KyberPreKeyStore, PreKeyStore, ProtocolAddress, PublicKey, SessionStore, SignedPreKeyStore,
    Timestamp,
};
use rand::rng;

use crate::error::CryptoError;

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
pub struct SealedSenderManager;

impl SealedSenderManager {
    /// Create a new SealedSenderManager.
    pub fn new() -> Self {
        Self
    }

    /// Encrypt plaintext to sealed sender format.
    ///
    /// This is the simplest way to send a sealed sender message. It encrypts
    /// the plaintext, creates the USMC, and produces the final sealed envelope.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The message content to encrypt
    /// * `sender_cert` - The sender's certificate (obtained from server)
    /// * `destination` - The recipient's protocol address
    /// * `session_store` - Session storage for the encryption
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The encrypted sealed sender message bytes.
    pub async fn encrypt<S, I>(
        plaintext: &[u8],
        sender_cert: &SenderCertificate,
        destination: &ProtocolAddress,
        session_store: &mut S,
        identity_store: &mut I,
    ) -> Result<Vec<u8>, CryptoError>
    where
        S: SessionStore,
        I: IdentityKeyStore,
    {
        let mut rng = rng();
        let now = SystemTime::now();

        sealed_sender_encrypt(
            destination,
            sender_cert,
            plaintext,
            session_store,
            identity_store,
            now,
            &mut rng,
        )
        .await
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }

    /// Decrypt sealed sender message and validate the sender certificate.
    ///
    /// This validates the sender certificate against the trust root and
    /// decrypts the message, revealing the sender's identity.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The sealed sender message bytes
    /// * `trust_root` - Server's public key for certificate validation
    /// * `timestamp` - Current timestamp for certificate expiration check
    /// * `local_e164` - Local user's phone number (for self-send detection)
    /// * `local_uuid` - Local user's UUID
    /// * `local_device_id` - Local device ID
    /// * `identity_store` - Identity key storage
    /// * `session_store` - Session storage
    /// * `pre_key_store` - One-time prekey storage
    /// * `signed_pre_key_store` - Signed prekey storage
    /// * `kyber_pre_key_store` - Kyber prekey storage
    ///
    /// # Returns
    ///
    /// The decryption result containing sender identity and decrypted message.
    #[allow(clippy::too_many_arguments)]
    pub async fn decrypt<I, S, P, SP, K>(
        ciphertext: &[u8],
        trust_root: &PublicKey,
        timestamp: Timestamp,
        local_e164: Option<String>,
        local_uuid: String,
        local_device_id: DeviceId,
        identity_store: &mut I,
        session_store: &mut S,
        pre_key_store: &mut P,
        signed_pre_key_store: &SP,
        kyber_pre_key_store: &mut K,
    ) -> Result<SealedSenderDecryptionResult, CryptoError>
    where
        I: IdentityKeyStore,
        S: SessionStore,
        P: PreKeyStore,
        SP: SignedPreKeyStore,
        K: KyberPreKeyStore,
    {
        sealed_sender_decrypt(
            ciphertext,
            trust_root,
            timestamp,
            local_e164,
            local_uuid,
            local_device_id,
            identity_store,
            session_store,
            pre_key_store,
            signed_pre_key_store,
            kyber_pre_key_store,
        )
        .await
        .map_err(|e| {
            // Map specific libsignal errors to our error types
            let msg = e.to_string();
            if msg.contains("SealedSenderSelfSend") {
                CryptoError::SealedSenderSelfSend
            } else if msg.contains("trust root") || msg.contains("certificate") {
                CryptoError::InvalidSenderCertificate(msg)
            } else {
                CryptoError::InvalidSealedSenderMessage(msg)
            }
        })
    }

    /// Create UnidentifiedSenderMessageContent from pre-encrypted message.
    ///
    /// Use this when you already have an encrypted message (WhisperMessage or
    /// PreKeyMessage) and want to wrap it in sealed sender format.
    ///
    /// # Arguments
    ///
    /// * `msg_type` - The type of the inner message (Whisper or PreKey)
    /// * `sender_cert` - The sender's certificate
    /// * `contents` - The encrypted message bytes
    /// * `content_hint` - Hint about the content type for recipient
    /// * `group_id` - Optional group ID for group messages
    ///
    /// # Returns
    ///
    /// The USMC ready for encryption with `encrypt_from_content()`.
    pub fn create_content(
        msg_type: CiphertextMessageType,
        sender_cert: SenderCertificate,
        contents: Vec<u8>,
        content_hint: ContentHint,
        group_id: Option<Vec<u8>>,
    ) -> Result<UnidentifiedSenderMessageContent, CryptoError> {
        UnidentifiedSenderMessageContent::new(msg_type, sender_cert, contents, content_hint, group_id)
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Encrypt a pre-built USMC to sealed sender format.
    ///
    /// This is useful when sending to multiple recipients - you can create the
    /// USMC once and encrypt it for each recipient.
    ///
    /// # Arguments
    ///
    /// * `usmc` - The UnidentifiedSenderMessageContent to encrypt
    /// * `destination` - The recipient's protocol address
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The encrypted sealed sender message bytes.
    pub async fn encrypt_from_content<I>(
        usmc: &UnidentifiedSenderMessageContent,
        destination: &ProtocolAddress,
        identity_store: &I,
    ) -> Result<Vec<u8>, CryptoError>
    where
        I: IdentityKeyStore,
    {
        let mut rng = rng();

        sealed_sender_encrypt_from_usmc(destination, usmc, identity_store, &mut rng)
            .await
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }

    /// Decrypt sealed sender message to USMC without inner decryption.
    ///
    /// This extracts the sender information and inner ciphertext without
    /// decrypting the inner message. Useful for:
    /// - Inspecting sender identity before full decryption
    /// - Forwarding messages without decrypting content
    /// - Validating certificates separately
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The sealed sender message bytes
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The USMC containing sender certificate and encrypted inner message.
    pub async fn decrypt_to_content<I>(
        ciphertext: &[u8],
        identity_store: &I,
    ) -> Result<UnidentifiedSenderMessageContent, CryptoError>
    where
        I: IdentityKeyStore,
    {
        sealed_sender_decrypt_to_usmc(ciphertext, identity_store)
            .await
            .map_err(|e| CryptoError::InvalidSealedSenderMessage(e.to_string()))
    }
}

impl Default for SealedSenderManager {
    fn default() -> Self {
        Self::new()
    }
}
