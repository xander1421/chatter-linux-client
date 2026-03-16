//! Group sender key operations.
//!
//! Handles creating, distributing, and processing sender keys for
//! group channel encryption using Signal's Sender Keys protocol.
//! All operations are delegated to the SignalActor for thread-safety.

use libsignal_protocol::{ProtocolAddress, DeviceId};
use crate::domain::ids::{ChannelId, UserId};
use crate::proto::content_v1::{GroupEncrypted, SenderKeyDistribution};
use crate::error::ServiceError;
use crate::util::time::TimestampNanos;
use super::SignalService;

impl SignalService {
    /// Get the current user's protocol address.
    pub(crate) fn get_our_address(&self) -> Result<ProtocolAddress, ServiceError> {
        let user_id = self.settings_repo.get_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID)
            .map_err(ServiceError::Storage)?
            .ok_or(ServiceError::NotLoggedIn)?;

        let device_id = self.settings_repo.get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .map_err(ServiceError::Storage)?
            .unwrap_or(1);

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;

        Ok(ProtocolAddress::new(user_id, device_id_struct))
    }

    /// Create a sender key distribution message for a channel.
    pub async fn create_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        let our_address = self.get_our_address()?;
        let skdm_bytes = self.actor.create_sender_key(*channel_id, our_address).await?;

        // Initialize sender key metadata for rotation tracking
        if let Err(e) = self.key_store.init_sender_key_metadata(&channel_id.to_string()) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to initialize sender key metadata"
            );
        }

        Ok(SenderKeyDistribution {
            channel_id: channel_id.to_string(),
            distribution_message: skdm_bytes,
            timestamp: TimestampNanos::now().as_u64(),
        })
    }

    /// Process a received sender key distribution message.
    pub async fn process_sender_key(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        skdm: &SenderKeyDistribution,
    ) -> Result<(), ServiceError> {
        let channel_id = skdm.channel_id.parse().map_err(|_| ServiceError::Validation("Invalid channel ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), DeviceId::try_from(sender_device_id).map_err(|_| ServiceError::Validation("Invalid ID".into()))?);
        
        self.actor.process_sender_key(channel_id, sender_address, skdm.distribution_message.clone()).await
    }

    /// Encrypt a message for a group channel using sender keys.
    ///
    /// Automatically rotates sender keys based on Signal best practices:
    /// - Rotate after 24 hours
    /// - Rotate after 100 messages
    pub async fn encrypt_for_group(
        &self,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(GroupEncrypted, Option<SenderKeyDistribution>), ServiceError> {
        let our_address = self.get_our_address()?;
        let channel_id_str = channel_id.to_string();

        // Check if rotation was manually requested
        if self.check_and_clear_rotation_needed(channel_id)? {
            tracing::info!(channel_id = %channel_id, "Manual sender key rotation requested");
            let _ = self.clear_channel_sender_keys(channel_id).await;
        }

        // Check for automatic rotation based on age or message count
        if let Ok((should_rotate, reason)) = self.key_store.should_rotate_sender_key(&channel_id_str) {
            if should_rotate {
                tracing::info!(
                    channel_id = %channel_id,
                    reason = reason,
                    "Automatic sender key rotation triggered"
                );
                let _ = self.clear_channel_sender_keys(channel_id).await;
            }
        }

        // Check if we need to create a sender key
        let has_key = self.has_sender_key(channel_id).await?;
        let skdm_to_distribute = if !has_key {
            Some(self.create_sender_key(channel_id).await?)
        } else {
            None
        };

        let ciphertext = self.actor.encrypt_for_group(*channel_id, our_address, plaintext.to_vec()).await?;

        // Increment message count for rotation tracking
        if let Err(e) = self.key_store.increment_sender_key_message_count(&channel_id_str) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to increment sender key message count"
            );
        }

        let group_encrypted = GroupEncrypted {
            channel_id: channel_id.to_string(),
            ciphertext,
            timestamp: TimestampNanos::now().as_u64(),
        };

        Ok((group_encrypted, skdm_to_distribute))
    }

    /// Decrypt a group message from a sender.
    pub async fn decrypt_from_group(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        encrypted: &GroupEncrypted,
    ) -> Result<Vec<u8>, ServiceError> {
        let channel_id = encrypted.channel_id.parse().map_err(|_| ServiceError::Validation("Invalid channel ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), DeviceId::try_from(sender_device_id).map_err(|_| ServiceError::Validation("Invalid ID".into()))?);

        self.actor.decrypt_from_group(channel_id, sender_address, encrypted.ciphertext.clone()).await
    }

    /// Check if we have a sender key for a channel.
    pub async fn has_sender_key(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let our_address = self.get_our_address()?;
        self.actor.has_sender_key(*channel_id, our_address).await
    }

    /// Rotate our sender key for a channel.
    pub async fn rotate_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        self.clear_channel_sender_keys(channel_id).await?;
        self.create_sender_key(channel_id).await
    }

    /// Clear all sender keys for a channel.
    pub async fn clear_channel_sender_keys(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        self.actor.clear_channel_sender_keys(*channel_id).await
    }

    /// Mark a channel as needing sender key rotation.
    pub fn mark_channel_for_rotation(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        self.settings_repo.set_string(&key, "1").map_err(ServiceError::Storage)
    }

    /// Check if a channel needs sender key rotation and clear the flag.
    pub(crate) fn check_and_clear_rotation_needed(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        let needs_rotation = self.settings_repo.get_string(&key)
            .map_err(ServiceError::Storage)?
            .map(|v| v == "1")
            .unwrap_or(false);

        if needs_rotation {
            self.settings_repo.delete(&key).map_err(ServiceError::Storage)?;
        }

        Ok(needs_rotation)
    }
}
