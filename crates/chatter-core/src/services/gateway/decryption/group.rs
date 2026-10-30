use super::*;

impl MessageGateway {
    /// Persist a channel member's profile key carried on their SKDM. Returns
    /// whether it was new or rotated — the caller's cue to refetch their
    /// profile. Our own (from our other devices) is not a peer key.
    pub(crate) fn store_skdm_profile_key(&self, sender_id: &UserId, key: Option<&[u8]>) -> bool {
        let Some(key) = key.and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
            return false;
        };
        let Ok(us) = self.settings_repo.current_user_id() else {
            return false;
        };
        if us == *sender_id {
            return false;
        }
        self.user_repo
            .set_friend_profile_key(&us, sender_id, &key)
            .inspect_err(|e| tracing::debug!(sender_id = %sender_id, error = %e, "SKDM: failed to store sender profile key"))
            .unwrap_or(false)
    }

    /// Install a sealed-sender-authenticated SKDM, gated on the channel's
    /// VERIFIED membership (Design B). Its `(revision, state_hash)` stamp is
    /// cross-checked first — a sender ahead of us is recorded for a change
    /// fetch even when not (yet) a member of our state; a fork counts only
    /// from a verified member. A sender outside the verified distribution set
    /// is not stored (`Ok(None)`): the server alone cannot make anyone a
    /// sender in our channels. `Ok(Some(profile_key_changed))` once stored.
    pub(crate) async fn accept_skdm(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        channel_id: &crate::domain::ids::ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<Option<bool>, ServiceError> {
        let gate = crate::services::message::GroupStateGate::new(&self.settings_repo);
        let is_member = gate.admits(channel_id, sender_id);
        gate.cross_check(channel_id, is_member, skdm.state_revision, &skdm.state_hash);
        if !is_member {
            tracing::warn!(%sender_id, %channel_id,
                "GATEWAY: SKDM from a sender outside the verified membership; not stored");
            return Ok(None);
        }
        self.signal_service
            .process_sender_key(sender_id, sender_device_id, skdm)
            .await
            .inspect_err(|e| {
                tracing::error!(%sender_id, %channel_id, error = %e, "GATEWAY: Failed to process sender key");
            })?;
        tracing::info!(%sender_id, %channel_id, "GATEWAY: Successfully stored sender key from channel member");
        // The member's profile key rides their SKDM (Signal shares it with
        // the group), so a co-member who is not a friend can decrypt their
        // name and avatar. Stored against the sealed-sender-authenticated
        // sender only — the key is theirs; a blocked sender never reaches
        // this point (checked by the caller).
        Ok(Some(self.store_skdm_profile_key(
            sender_id,
            skdm.profile_key.as_deref(),
        )))
    }

    /// Process an incoming sender key (group) encrypted message.
    ///
    /// Sender key messages use Signal Protocol's sender keys for efficient
    /// one-to-many encryption in channels.
    ///
    /// # Arguments
    ///
    /// * `encrypted_data` - The EncryptedData containing serialized GroupEncrypted
    /// * `sender_id` - The sender's user ID
    /// * `sender_device_id` - The sender's device ID
    ///
    /// # Returns
    ///
    /// - `Ok(DecryptResult::Plaintext(bytes))` - Successfully decrypted
    /// - `Ok(DecryptResult::NeedsSenderKey { channel_id, sender_id })` - Need to request sender's SKDM
    /// - `Err(_)` - Other error
    pub async fn process_group_encrypted_message(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<DecryptResult, ServiceError> {
        use crate::domain::ids::ChannelId;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            ciphertext_len = encrypted_data.ciphertext.len(),
            associated_data = ?String::from_utf8_lossy(&encrypted_data.associated_data),
            "[MSG_RECV] Processing group encrypted message"
        );

        // Check if this is a sender key message (two formats supported)
        // - `sender_key` (direct channel messages)
        // - `sender:{user_id}:channel:{channel_id}` (group mailbox fanout)
        let is_sender_key_msg = {
            if encrypted_data.associated_data == b"sender_key" {
                true
            } else {
                let associated_str = String::from_utf8_lossy(&encrypted_data.associated_data);
                associated_str.starts_with("sender:") && associated_str.contains(":channel:")
            }
        };
        if !is_sender_key_msg {
            tracing::warn!(
                associated_data = ?String::from_utf8_lossy(&encrypted_data.associated_data),
                "[MSG_RECV] Not a sender key message, rejecting"
            );
            return Err(ServiceError::Validation(
                "Not a sender key encrypted message".into(),
            ));
        }

        // Deserialize the GroupEncrypted proto from ciphertext field
        let group_encrypted = GroupEncrypted::decode(encrypted_data.ciphertext.as_slice())
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "[MSG_RECV] Failed to decode GroupEncrypted proto"
                );
                ServiceError::Validation(format!("Failed to decode GroupEncrypted: {}", e))
            })?;

        let channel_id: ChannelId = group_encrypted.channel_id.parse().map_err(|e| {
            tracing::error!(
                channel_id_str = %group_encrypted.channel_id,
                error = %e,
                "[MSG_RECV] Invalid channel_id in GroupEncrypted"
            );
            ServiceError::Validation(format!("Invalid channel_id in GroupEncrypted: {}", e))
        })?;

        tracing::debug!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            inner_ciphertext_len = group_encrypted.ciphertext.len(),
            "[MSG_RECV] Extracted GroupEncrypted, attempting decryption"
        );

        // Only a member of our VERIFIED membership may speak in the channel
        // (Signal drops group messages from non-members); checked before
        // decrypting so a non-member cannot draw a SenderKeyRequest either.
        let gate = crate::services::message::GroupStateGate::new(&self.settings_repo);
        if !gate.admits(&channel_id, sender_id) {
            tracing::warn!(%channel_id, %sender_id,
                "[MSG_RECV] Group message from a sender outside the verified membership; dropped");
            return Err(ServiceError::Validation(
                "group message from a sender outside the verified channel membership".into(),
            ));
        }

        // Try to decrypt using sender keys
        match self
            .signal_service
            .decrypt_from_group(sender_id, sender_device_id, &group_encrypted)
            .await
        {
            Ok(plaintext) => {
                // The sender's verified (revision, state_hash) stamp. A peer
                // ahead is recorded; `MessageService::sync_channel_state_if_behind`
                // has the channel service fetch the changes.
                if let Some(content::Content::DataMessage(dm)) =
                    crate::network::validation::protovalidate::decode_inbound_content(
                        plaintext.as_slice(),
                        "sender_key",
                    )
                    .ok()
                    .and_then(|c| c.content)
                {
                    gate.cross_check(
                        &channel_id,
                        true,
                        dm.group_state_revision,
                        &dm.group_state_hash,
                    );
                }
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    plaintext_len = plaintext.len(),
                    "[MSG_RECV] Successfully decrypted sender key message"
                );
                Ok(DecryptResult::Plaintext(plaintext))
            }
            Err(e) => {
                // Check if this is a "no sender key" error
                let error_str = e.to_string();
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    error = %error_str,
                    "[MSG_RECV] Decryption failed, checking if sender key is missing"
                );

                if error_str.contains("no sender key")
                    || error_str.contains("SenderKeyRecord")
                    || error_str.contains("not found")
                {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_RECV] MISSING SENDER KEY - need to request SKDM from sender"
                    );
                    Ok(DecryptResult::NeedsSenderKey {
                        channel_id,
                        sender_id: *sender_id,
                    })
                } else {
                    // Other decryption error
                    tracing::error!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        error = %error_str,
                        "[MSG_RECV] Decryption failed with non-recoverable error"
                    );
                    Err(e)
                }
            }
        }
    }
}
