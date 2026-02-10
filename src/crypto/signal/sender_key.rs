//! Sender key for groups.
//!
//! Sender key distribution and management for efficient group encryption.
//! Sender keys allow a sender to encrypt once for a group, rather than
//! encrypting separately for each recipient (as in pairwise sessions).

use libsignal_protocol::{
    create_sender_key_distribution_message, group_decrypt, group_encrypt,
    process_sender_key_distribution_message, ProtocolAddress, SenderKeyDistributionMessage,
    SenderKeyMessage, SenderKeyStore,
};
use rand::{rngs::StdRng, SeedableRng};
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
    pub fn new() -> Self {
        Self
    }

    /// Create a sender key distribution message for a group.
    ///
    /// Call this when joining a group or when rotating sender keys.
    /// The returned SKDM must be sent to all group members via their
    /// individual pairwise sessions.
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
        let mut rng = StdRng::from_os_rng();
        create_sender_key_distribution_message(sender, distribution_id, sender_key_store, &mut rng)
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Process a received sender key distribution message from a group member.
    ///
    /// Call this when receiving an SKDM from another member.
    /// After processing, you can decrypt messages from that sender.
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
        process_sender_key_distribution_message(sender, skdm, sender_key_store)
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Encrypt a message for a group using sender keys.
    ///
    /// All members who have processed our distribution message can decrypt.
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
        let mut rng = StdRng::from_os_rng();
        group_encrypt(sender_key_store, sender, distribution_id, plaintext, &mut rng)
            .await
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }

    /// Decrypt a group message from a sender.
    ///
    /// Requires having previously processed that sender's distribution message.
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
        group_decrypt(ciphertext, sender_key_store, sender)
            .await
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))
    }
}

impl Default for SenderKeyManager {
    fn default() -> Self {
        Self::new()
    }
}
