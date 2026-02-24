//! Group sender key operations.
//!
//! Handles creating, distributing, and processing sender keys for
//! group channel encryption using Signal's Sender Keys protocol.

use libsignal_protocol::{
    ProtocolAddress, DeviceId,
    SenderKeyDistributionMessage,
};
use uuid::Uuid;

use crate::crypto::{
    SignalStore,
    signal::sender_key::SenderKeyManager,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::proto::content_v1::{GroupEncrypted, SenderKeyDistribution};
use crate::storage::repositories::settings_repo::keys;
use crate::error::{CryptoError, ServiceError};
use crate::util::time::TimestampNanos;

use super::SignalService;

impl SignalService {
    /// Get the current user's protocol address.
    pub(crate) fn get_our_address(&self) -> Result<ProtocolAddress, ServiceError> {
        let user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ServiceError::Storage)?
            .ok_or(ServiceError::NotLoggedIn)?;

        let device_id = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(ServiceError::Storage)?
            .unwrap_or(1);

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;

        Ok(ProtocolAddress::new(user_id, device_id_struct))
    }

    /// Create a sender key distribution message for a channel.
    ///
    /// Call this when:
    /// - Creating an encrypted channel
    /// - Joining an encrypted channel
    /// - Rotating sender keys (e.g., after member leaves)
    ///
    /// The returned SKDM must be distributed to all channel members.
    pub async fn create_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        let our_address = self.get_our_address()?;

        // Use channel ID as distribution ID (UUID)
        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        tracing::info!(
            channel_id = %channel_id,
            our_address = %our_address,
            distribution_id = %distribution_id,
            "[GROUP_MSG] Creating sender key for channel"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        let skdm = SenderKeyManager::create_distribution_message(
            &our_address,
            distribution_id,
            &mut store,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "[GROUP_MSG] Failed to create sender key"
            );
            ServiceError::Crypto(e)
        })?;

        tracing::info!(
            channel_id = %channel_id,
            skdm_len = skdm.serialized().len(),
            "[GROUP_MSG] Created sender key for channel successfully"
        );

        Ok(SenderKeyDistribution {
            channel_id: channel_id.to_string(),
            distribution_message: skdm.serialized().to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        })
    }

    /// Process a received sender key distribution message.
    ///
    /// Call this when receiving an SKDM from another channel member.
    /// After processing, you can decrypt messages from that sender.
    pub async fn process_sender_key(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        skdm: &SenderKeyDistribution,
    ) -> Result<(), ServiceError> {
        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %skdm.channel_id,
            skdm_len = skdm.distribution_message.len(),
            "[GROUP_MSG] Processing sender key distribution from member"
        );

        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        tracing::debug!(
            sender_address = %sender_address,
            "[GROUP_MSG] Constructed sender address for SKDM processing"
        );

        let skdm_msg = SenderKeyDistributionMessage::try_from(skdm.distribution_message.as_slice())
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    channel_id = %skdm.channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to deserialize SKDM"
                );
                CryptoError::Protocol(e.to_string())
            })?;

        let mut store = SignalStore::new(self.key_store.clone());

        SenderKeyManager::process_distribution_message(&sender_address, &skdm_msg, &mut store)
            .await
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    channel_id = %skdm.channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to process SKDM"
                );
                ServiceError::Crypto(e)
            })?;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %skdm.channel_id,
            "[GROUP_MSG] Successfully stored sender key from member"
        );

        Ok(())
    }

    /// Encrypt a message for a group channel using sender keys.
    ///
    /// All members who have our sender key can decrypt.
    /// Automatically creates a sender key if one doesn't exist.
    ///
    /// # Returns
    ///
    /// A tuple of (encrypted_message, optional_skdm). The SKDM is returned
    /// when a new sender key was created and must be distributed to all
    /// channel members so they can decrypt future messages.
    pub async fn encrypt_for_group(
        &self,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(GroupEncrypted, Option<SenderKeyDistribution>), ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        tracing::info!(
            channel_id = %channel_id,
            our_address = %our_address,
            plaintext_len = plaintext.len(),
            "[GROUP_MSG] Encrypting message for group"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        // Check if rotation was requested (e.g., member left the channel)
        // If so, clear the existing key to force creation of a new one
        if self.check_and_clear_rotation_needed(channel_id)? {
            tracing::info!(
                channel_id = %channel_id,
                "[GROUP_MSG] Rotation needed - clearing existing sender key"
            );
            // Clear existing key - this will force creation of a new one below
            let _ = self.clear_channel_sender_keys(channel_id);
        }

        // Check if we need to create a sender key and return SKDM for distribution
        let has_key = self.has_sender_key(channel_id)?;
        tracing::debug!(
            channel_id = %channel_id,
            has_sender_key = has_key,
            "[GROUP_MSG] Sender key check"
        );

        let skdm_to_distribute = if !has_key {
            tracing::info!(
                channel_id = %channel_id,
                "[GROUP_MSG] No sender key found, creating new SKDM"
            );
            let skdm = SenderKeyManager::create_distribution_message(
                &our_address,
                distribution_id,
                &mut store,
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to create SKDM for encryption"
                );
                ServiceError::Crypto(e)
            })?;

            tracing::info!(
                channel_id = %channel_id,
                skdm_len = skdm.serialized().len(),
                "[GROUP_MSG] Created new SKDM to distribute"
            );

            Some(SenderKeyDistribution {
                channel_id: channel_id.to_string(),
                distribution_message: skdm.serialized().to_vec(),
                timestamp: TimestampNanos::now().as_u64(),
            })
        } else {
            tracing::debug!(
                channel_id = %channel_id,
                "[GROUP_MSG] Using existing sender key"
            );
            None
        };

        let ciphertext =
            SenderKeyManager::encrypt(&our_address, distribution_id, plaintext, &mut store)
                .await
                .map_err(|e| {
                    tracing::error!(
                        channel_id = %channel_id,
                        error = %e,
                        "[GROUP_MSG] Sender key encryption failed"
                    );
                    ServiceError::Crypto(e)
                })?;

        tracing::info!(
            channel_id = %channel_id,
            plaintext_len = plaintext.len(),
            ciphertext_len = ciphertext.serialized().len(),
            skdm_created = skdm_to_distribute.is_some(),
            "[GROUP_MSG] Encrypted message for group successfully"
        );

        let group_encrypted = GroupEncrypted {
            channel_id: channel_id.to_string(),
            ciphertext: ciphertext.serialized().to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        Ok((group_encrypted, skdm_to_distribute))
    }

    /// Decrypt a group message from a sender.
    ///
    /// Requires having processed that sender's SKDM first.
    pub async fn decrypt_from_group(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        encrypted: &GroupEncrypted,
    ) -> Result<Vec<u8>, ServiceError> {
        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %encrypted.channel_id,
            ciphertext_len = encrypted.ciphertext.len(),
            "[GROUP_MSG] Decrypting message from group"
        );

        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        tracing::debug!(
            sender_address = %sender_address,
            "[GROUP_MSG] Constructed sender address for decryption"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        let plaintext =
            SenderKeyManager::decrypt(&encrypted.ciphertext, &sender_address, &mut store)
                .await
                .map_err(|e| {
                    tracing::error!(
                        sender_id = %sender_id,
                        sender_device_id = sender_device_id,
                        channel_id = %encrypted.channel_id,
                        error = %e,
                        "[GROUP_MSG] Group decryption failed - sender key may be missing"
                    );
                    ServiceError::Crypto(e)
                })?;

        tracing::info!(
            sender_id = %sender_id,
            channel_id = %encrypted.channel_id,
            plaintext_len = plaintext.len(),
            "[GROUP_MSG] Decrypted message from group successfully"
        );

        Ok(plaintext)
    }

    /// Check if we have a sender key for a channel.
    pub fn has_sender_key(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        let key = self
            .key_store
            .load_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        let has_key = key.is_some();
        tracing::debug!(
            channel_id = %channel_id,
            our_address = %our_address,
            distribution_id = %distribution_id,
            has_key = has_key,
            "[GROUP_MSG] Checked sender key existence"
        );

        Ok(has_key)
    }

    /// Rotate our sender key for a channel.
    ///
    /// Call this when a member leaves an encrypted channel to maintain
    /// forward secrecy. The old key is deleted and a new SKDM is returned
    /// for distribution to remaining members.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to rotate keys for
    ///
    /// # Returns
    ///
    /// A new SenderKeyDistribution to send to remaining channel members.
    pub async fn rotate_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        // Delete existing sender key
        self.key_store
            .delete_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        tracing::info!(
            channel_id = %channel_id,
            "Deleted old sender key for rotation"
        );

        // Create new sender key and SKDM
        let skdm = self.create_sender_key(channel_id).await?;

        tracing::info!(
            channel_id = %channel_id,
            "Created new sender key after rotation"
        );

        Ok(skdm)
    }

    /// Clear all sender keys for a channel.
    ///
    /// Call this when leaving a channel to clean up stored keys.
    pub fn clear_channel_sender_keys(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        // Delete our own sender key
        self.key_store
            .delete_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        tracing::debug!(
            channel_id = %channel_id,
            "Cleared sender keys for channel"
        );

        Ok(())
    }

    /// Mark a channel as needing sender key rotation.
    ///
    /// Call this when a member leaves the channel to ensure forward secrecy.
    /// The rotation will happen automatically on the next message send.
    pub fn mark_channel_for_rotation(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        self.settings_repo
            .set_string(&key, "1")
            .map_err(ServiceError::Storage)?;
        tracing::info!(
            channel_id = %channel_id,
            "Marked channel for sender key rotation"
        );
        Ok(())
    }

    /// Check if a channel needs sender key rotation and clear the flag.
    ///
    /// Returns true if rotation was needed (and clears the flag).
    pub(crate) fn check_and_clear_rotation_needed(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        let needs_rotation = self
            .settings_repo
            .get_string(&key)
            .map_err(ServiceError::Storage)?
            .map(|v| v == "1")
            .unwrap_or(false);

        if needs_rotation {
            // Clear the flag
            self.settings_repo
                .delete(&key)
                .map_err(ServiceError::Storage)?;
            tracing::debug!(
                channel_id = %channel_id,
                "Cleared sender key rotation flag"
            );
        }

        Ok(needs_rotation)
    }
}
