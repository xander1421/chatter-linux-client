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
        // libsignal returns the EXISTING key's SKDM when we already have one
        // (every SenderKeyRequest reply lands here). Only a genuinely new key
        // restarts the rotation clock — resetting it on every reply meant a
        // peer's requests kept one key alive past its 24h / 2000-message cap.
        let is_new = !self.actor.has_sender_key(*channel_id, our_address.clone()).await?;
        let skdm_bytes = self.actor.create_sender_key(*channel_id, our_address).await?;

        if is_new {
            if let Err(e) = self.key_store.init_sender_key_metadata(&channel_id.to_string()) {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to initialize sender key metadata"
                );
            }
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
    /// - Rotate after 2000 messages
    ///
    /// Returns an error if message sending is blocked due to a pending
    /// sender key rotation (e.g., during member kick/ban). This prevents
    /// messages from being encrypted with the old key that the removed
    /// member still possesses.
    pub async fn encrypt_for_group(
        &self,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(GroupEncrypted, Option<SenderKeyDistribution>), ServiceError> {
        // Block if sender key rotation is in progress (member removal).
        // The old key must not be used — the removed member still has it.
        if self.is_channel_send_blocked(channel_id)? {
            return Err(ServiceError::Validation(
                "sender key rotation in progress — message sending blocked until new key is distributed".into(),
            ));
        }

        let our_address = self.get_our_address()?;
        let channel_id_str = channel_id.to_string();

        // Check if rotation was manually requested
        if self.check_and_clear_rotation_needed(channel_id)? {
            tracing::info!(channel_id = %channel_id, "Manual sender key rotation requested");
            self.drop_own_sender_key(channel_id)?;
        }

        // Check for automatic rotation based on age or message count
        if let Ok((should_rotate, reason)) = self.key_store.should_rotate_sender_key(&channel_id_str) {
            if should_rotate {
                tracing::info!(
                    channel_id = %channel_id,
                    reason = reason,
                    "Automatic sender key rotation triggered"
                );
                self.drop_own_sender_key(channel_id)?;
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
        self.drop_own_sender_key(channel_id)?;
        self.create_sender_key(channel_id).await
    }

    /// Delete OUR sending key for a channel so the next send creates a fresh
    /// one. Only our own record: the keys other members sent us stay, because
    /// their chains did not rotate — deleting them (what a rotation used to do
    /// via `clear_channel_sender_keys`) made every peer's next message land on
    /// "missing sender key", and the SKDM they answer with starts at their
    /// CURRENT iteration, so everything they sent in between was lost for good.
    fn drop_own_sender_key(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let our_address = self.get_our_address()?;
        self.key_store
            .delete_sender_key(
                &format!("{}:{}", our_address.name(), our_address.device_id()),
                &channel_id.to_string(),
            )
            .map_err(ServiceError::Storage)?;
        self.reset_sender_key_recipients(channel_id)
    }

    /// Clear all sender keys for a channel — ours and every member's. Only for
    /// leaving the channel; a rotation uses [`Self::rotate_sender_key`].
    pub async fn clear_channel_sender_keys(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        self.actor.clear_channel_sender_keys(*channel_id).await?;
        self.reset_sender_key_recipients(channel_id)
    }

    // =========================================================================
    // Who holds our current sender key (Signal-Desktop `senderKeyInfo.memberDevices`)
    // =========================================================================
    //
    // A sender-key message is decryptable only by a member who processed an
    // SKDM at or before its iteration: libsignal builds the SKDM from the
    // CURRENT chain key (`create_sender_key_distribution_message`), so an SKDM
    // sent after message N can never unlock N. The only way a member reads
    // everything we send is to get the SKDM BEFORE the first message they are
    // meant to read — so we record who has it and top up the rest pre-send.
    //
    // ponytail: tracked per user, not per device (the device list costs a
    // prekey fetch). A member's newly linked device heals through its
    // SenderKeyRequest; per-device tracking if that proves too lossy.

    fn sender_key_recipients_key(channel_id: &ChannelId) -> String {
        format!("sender_key_delivered:{}", channel_id)
    }

    /// Users that have received an SKDM for our CURRENT sender key.
    pub fn sender_key_recipients(
        &self,
        channel_id: &ChannelId,
    ) -> Result<std::collections::HashSet<String>, ServiceError> {
        Ok(self
            .settings_repo
            .get_string(&Self::sender_key_recipients_key(channel_id))
            .map_err(ServiceError::Storage)?
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Record that `user_id` now holds our current sender key.
    pub fn mark_sender_key_delivered(&self, channel_id: &ChannelId, user_id: &str) -> Result<(), ServiceError> {
        let mut recipients = self.sender_key_recipients(channel_id)?;
        if !recipients.insert(user_id.to_string()) {
            return Ok(());
        }
        let joined = recipients.into_iter().collect::<Vec<_>>().join(",");
        self.settings_repo
            .set_string(&Self::sender_key_recipients_key(channel_id), &joined)
            .map_err(ServiceError::Storage)
    }

    fn reset_sender_key_recipients(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        self.settings_repo
            .delete(&Self::sender_key_recipients_key(channel_id))
            .map_err(ServiceError::Storage)?;
        Ok(())
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

    /// Block message sending on a channel during sender key rotation.
    ///
    /// This MUST be set before a member removal (kick/ban) to prevent
    /// messages from being encrypted with the old key during the rotation
    /// window. The kicked/banned member still has the old key, so any
    /// messages encrypted with it would violate forward secrecy.
    pub fn block_channel_send(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let key = format!("sender_key_send_blocked:{}", channel_id);
        self.settings_repo.set_string(&key, "1").map_err(ServiceError::Storage)
    }

    /// Unblock message sending on a channel after rotation + SKDM distribution.
    ///
    /// Call this ONLY after the new SKDM has been distributed to all
    /// remaining members, ensuring they can decrypt messages with the new key.
    pub fn unblock_channel_send(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let key = format!("sender_key_send_blocked:{}", channel_id);
        self.settings_repo.delete(&key).map_err(ServiceError::Storage)?;
        Ok(())
    }

    /// Check if message sending is blocked on a channel due to pending rotation.
    pub fn is_channel_send_blocked(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let key = format!("sender_key_send_blocked:{}", channel_id);
        Ok(self.settings_repo.get_string(&key)
            .map_err(ServiceError::Storage)?
            .map(|v| v == "1")
            .unwrap_or(false))
    }
}
