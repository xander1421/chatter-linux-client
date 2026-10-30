//! Sender key for groups.
//!
//! Sender key distribution and management for efficient group encryption.
//! Sender keys allow a sender to encrypt once for a group, rather than
//! encrypting separately for each recipient (as in pairwise sessions).
//!
//! # Rotation cadence
//!
//! Per the Signal whitepaper section on group encryption, sender keys must be
//! rotated periodically to bound forward-secrecy windows. The cadence is:
//!
//! - **Time-based**: every 24 hours
//! - **Message-count-based**: every 2000 messages
//!
//! The `SignalService` sender-key send path enforces this cadence using
//! persisted age and message-count metadata. Libsignal advances the sender
//! chain during encryption but does not rotate it at these thresholds. The
//! wrapper functions in this module only perform the libsignal operations.
//!
//! # Distribution transport invariant
//!
//! Sender key distribution messages (SKDMs) carry the symmetric chain key for
//! a group. They MUST be transported to each recipient via that recipient's
//! existing 1:1 Signal session (Double Ratchet), per the Signal spec. Sending
//! SKDMs over any other channel — plaintext, broadcast, or an unauthenticated
//! group fanout — breaks the security model.
//!
//! # References
//!
//! - Signal whitepaper, "Group encryption" / sender key section
//! - libsignal-protocol crate: `group_cipher` and `sender_keys` modules

use libsignal_protocol::{
    ProtocolAddress, SenderKeyDistributionMessage, SenderKeyMessage, SenderKeyStore,
    create_sender_key_distribution_message, group_decrypt, group_encrypt,
    process_sender_key_distribution_message,
};
// SECURITY: each sender-key draw must hit the OS directly so a process
// memory disclosure cannot rewind the RNG. See the long comment in
// `sealed_sender.rs` for why we use `OsRng.unwrap_err()` rather than
// `StdRng::from_os_rng()`.
use rand::TryRngCore;
use rand::rngs::OsRng;
use uuid::Uuid;

use crate::error::CryptoError;

/// Sender key manager for group messaging.
///
/// This manager provides methods for creating and processing sender key
/// distribution messages, as well as encrypting and decrypting group messages.
///
/// # Design
///
/// Following the stateless pattern established in Phase 11, all methods take
/// the sender key store as a parameter rather than storing it in the struct.
/// This avoids mutable aliasing issues with libsignal's async trait methods.
///
/// # Workflow
///
/// 1. When joining a group or rotating keys, call `create_distribution_message`
/// 2. Send the SKDM to all group members via their individual sessions
/// 3. When receiving an SKDM, call `process_distribution_message`
/// 4. After processing, use `encrypt`/`decrypt` for group messages
pub struct SenderKeyManager;

impl SenderKeyManager {
    /// Create a new SenderKeyManager.
    ///
    /// This is a zero-sized handle; all methods are stateless and accept the
    /// `SenderKeyStore` as a parameter. Constructing the manager performs no
    /// cryptographic work.
    pub fn new() -> Self {
        Self
    }

    /// Create a sender key distribution message for a group.
    ///
    /// Call this when joining a group or when rotating sender keys.
    /// The returned SKDM must be sent to all group members via their
    /// individual pairwise sessions (1:1 Signal sessions / Double Ratchet),
    /// per the Signal spec — never over an unauthenticated channel.
    ///
    /// # Contract
    ///
    /// This is a thin wrapper over libsignal's
    /// `create_sender_key_distribution_message`. On our behalf, libsignal:
    ///
    /// - Generates a sender key chain key and signing keypair if no record
    ///   exists for `(sender, distribution_id)`; otherwise uses that record.
    /// - Persists the sender key state into `sender_key_store`.
    /// - Returns an SKDM containing the chain key, iteration, and signing
    ///   public key for recipients to install.
    ///
    /// The service drops the old record before requesting a rotation. This
    /// wrapper does not enforce the 24h / 2000-message cadence.
    ///
    /// # Arguments
    ///
    /// * `sender` - The protocol address of the sender (this device)
    /// * `distribution_id` - UUID identifying the group/distribution
    /// * `sender_key_store` - Mutable reference to the sender key store
    ///
    /// # Returns
    ///
    /// The sender key distribution message to send to group members.
    pub async fn create_distribution_message(
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        sender_key_store: &mut dyn SenderKeyStore,
    ) -> Result<SenderKeyDistributionMessage, CryptoError> {
        tracing::debug!(
            sender = %sender,
            distribution_id = %distribution_id,
            "[SENDER_KEY] Creating distribution message"
        );
        let mut rng = OsRng.unwrap_err();
        let result = create_sender_key_distribution_message(
            sender,
            distribution_id,
            sender_key_store,
            &mut rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                sender = %sender,
                distribution_id = %distribution_id,
                error = %e,
                "[SENDER_KEY] Failed to create distribution message"
            );
            CryptoError::Protocol(e.to_string())
        })?;
        tracing::info!(
            sender = %sender,
            distribution_id = %distribution_id,
            skdm_len = result.serialized().len(),
            "[SENDER_KEY] Created distribution message successfully"
        );
        // Operational metric: rotation/creation visibility for sender keys.
        // No secrets emitted — distribution_id is a UUID and sender is a
        // protocol address (user_id + device_id), both non-secret.
        tracing::info!(
            target: "chatter::crypto::sender_key_distribution",
            distribution_id = %distribution_id,
            channel_id = %distribution_id,
            "sender key distribution emitted"
        );
        Ok(result)
    }

    /// Process a received sender key distribution message from a group member.
    ///
    /// Call this when receiving an SKDM from another member. The SKDM must
    /// have arrived over that member's 1:1 Signal session (Double Ratchet) —
    /// the caller is responsible for decrypting the pairwise envelope before
    /// invoking this function. After processing, you can decrypt subsequent
    /// group messages from that sender.
    ///
    /// # Contract
    ///
    /// This is a thin wrapper over libsignal's
    /// `process_sender_key_distribution_message`. On our behalf, libsignal:
    ///
    /// - Verifies the SKDM signature against the embedded signing public key.
    /// - Installs the chain key, iteration, and signing public key into
    ///   `sender_key_store` under `(sender, distribution_id)`.
    /// - Installs the state carried by the peer's distribution message,
    ///   including a new key when the peer rotates it.
    ///
    /// This wrapper does not enforce or short-circuit any rotation policy.
    ///
    /// # Arguments
    ///
    /// * `sender` - The protocol address of the sender who sent the SKDM
    /// * `skdm` - The sender key distribution message received
    /// * `sender_key_store` - Mutable reference to the sender key store
    pub async fn process_distribution_message(
        sender: &ProtocolAddress,
        skdm: &SenderKeyDistributionMessage,
        sender_key_store: &mut dyn SenderKeyStore,
    ) -> Result<(), CryptoError> {
        tracing::debug!(
            sender = %sender,
            skdm_len = skdm.serialized().len(),
            "[SENDER_KEY] Processing distribution message from sender"
        );
        process_sender_key_distribution_message(sender, skdm, sender_key_store)
            .await
            .map_err(|e| {
                tracing::error!(
                    sender = %sender,
                    error = %e,
                    "[SENDER_KEY] Failed to process distribution message"
                );
                CryptoError::Protocol(e.to_string())
            })?;
        tracing::info!(
            sender = %sender,
            "[SENDER_KEY] Stored sender key from distribution message"
        );
        Ok(())
    }

    /// Encrypt a message for a group using sender keys.
    ///
    /// All members who have processed our distribution message can decrypt.
    /// Callers must have previously published an SKDM (via
    /// `create_distribution_message`) to all intended recipients over their
    /// 1:1 Signal sessions; otherwise recipients will fail to decrypt.
    ///
    /// # Contract
    ///
    /// This is a thin wrapper over libsignal's `group_encrypt`. On our behalf,
    /// libsignal:
    ///
    /// - Loads the current sender key chain for `(sender, distribution_id)`.
    /// - Derives the next message key from the chain key, advances the chain,
    ///   and persists the updated state.
    /// - Signs the ciphertext with the per-distribution signing key.
    /// - Does not track the service's age/count rotation thresholds; the
    ///   service checks those before calling this method.
    ///
    /// This wrapper does not inspect the service's rotation metadata.
    ///
    /// # Arguments
    ///
    /// * `sender` - The protocol address of the sender (this device)
    /// * `distribution_id` - UUID identifying the group/distribution
    /// * `plaintext` - The message to encrypt
    /// * `sender_key_store` - Mutable reference to the sender key store
    ///
    /// # Returns
    ///
    /// The encrypted sender key message.
    pub async fn encrypt(
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        plaintext: &[u8],
        sender_key_store: &mut dyn SenderKeyStore,
    ) -> Result<SenderKeyMessage, CryptoError> {
        tracing::debug!(
            sender = %sender,
            distribution_id = %distribution_id,
            plaintext_len = plaintext.len(),
            "[SENDER_KEY] Encrypting message for group"
        );
        let mut rng = OsRng.unwrap_err();
        let result = group_encrypt(
            sender_key_store,
            sender,
            distribution_id,
            plaintext,
            &mut rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                sender = %sender,
                distribution_id = %distribution_id,
                error = %e,
                "[SENDER_KEY] Group encryption failed"
            );
            CryptoError::EncryptionFailed(e.to_string())
        })?;
        tracing::debug!(
            sender = %sender,
            distribution_id = %distribution_id,
            ciphertext_len = result.serialized().len(),
            "[SENDER_KEY] Group encryption successful"
        );
        Ok(result)
    }

    /// Decrypt a group message from a sender.
    ///
    /// Requires having previously processed that sender's distribution message
    /// via `process_distribution_message`. If the sender has rotated their
    /// chain (per libsignal's 24h / 2000-message policy) and we have not yet
    /// processed the new SKDM, decryption will fail and the caller should
    /// request a fresh distribution message.
    ///
    /// # Contract
    ///
    /// This is a thin wrapper over libsignal's `group_decrypt`. On our behalf,
    /// libsignal:
    ///
    /// - Looks up the stored sender key state for `(sender, distribution_id)`
    ///   using the distribution id embedded in the ciphertext.
    /// - Verifies the ciphertext signature with the stored signing public key.
    /// - Derives the message key for the ciphertext's iteration (advancing or
    ///   skipping within the chain as needed) and updates the stored state.
    /// - Returns the decrypted plaintext.
    ///
    /// This wrapper does not influence rotation; receiver-side rotation is
    /// adopted only when a new SKDM is processed.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The encrypted sender key message bytes
    /// * `sender` - The protocol address of the sender
    /// * `sender_key_store` - Mutable reference to the sender key store
    ///
    /// # Returns
    ///
    /// The decrypted plaintext.
    pub async fn decrypt(
        ciphertext: &[u8],
        sender: &ProtocolAddress,
        sender_key_store: &mut dyn SenderKeyStore,
    ) -> Result<Vec<u8>, CryptoError> {
        tracing::debug!(
            sender = %sender,
            ciphertext_len = ciphertext.len(),
            "[SENDER_KEY] Decrypting group message from sender"
        );
        let result = group_decrypt(ciphertext, sender_key_store, sender)
            .await
            .map_err(|e| {
                // `DuplicatedMessage` ("message with old counter") returns
                // before `store_sender_key`. The persisted chain is unchanged.
                // A missing SKDM and a ciphertext that might still open stay
                // on the failure path below.
                if matches!(
                    e,
                    libsignal_protocol::SignalProtocolError::DuplicatedMessage(_, _)
                ) {
                    tracing::debug!(
                        sender = %sender,
                        ciphertext_len = ciphertext.len(),
                        error = %e,
                        "[SENDER_KEY] Duplicate sender-key ciphertext; sender-key record not written"
                    );
                    tracing::debug!(
                        target: "chatter::crypto::decrypt_failure",
                        kind = "group_duplicate",
                        error = %e,
                        "consumed sender-key counter"
                    );
                    return CryptoError::DecryptionFailed(e.to_string());
                }
                tracing::error!(
                    sender = %sender,
                    ciphertext_len = ciphertext.len(),
                    error = %e,
                    "[SENDER_KEY] Group decryption failed - may need sender key"
                );
                // Operational metric: group decrypt failure rate.
                tracing::warn!(
                    target: "chatter::crypto::decrypt_failure",
                    kind = "group",
                    error = %e,
                    "decryption failure"
                );
                CryptoError::DecryptionFailed(e.to_string())
            })?;
        tracing::debug!(
            sender = %sender,
            plaintext_len = result.len(),
            "[SENDER_KEY] Group decryption successful"
        );
        Ok(result)
    }
}

impl Default for SenderKeyManager {
    fn default() -> Self {
        Self::new()
    }
}
