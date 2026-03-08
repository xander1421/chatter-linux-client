//! Group message operations.
//!
//! Sender key distribution, group mailbox announcements, and N-way fanout delivery.

use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::GroupFanout;
use crate::mailbox::TokenManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::network::grpc::MailboxClient;
use crate::proto::chat_v1::{self, SealedSenderMessage};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    /// Send a GroupMailboxAddress announcement to channel members.
    ///
    /// NDSS '21 bootstrap: Mailbox announcements MUST be sent pairwise via
    /// Double Ratchet to each member's device inbox, NOT via sender keys.
    /// This is because members don't have each other's mailbox addresses yet.
    pub async fn send_group_mailbox_announcement(
        &self,
        _message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        _channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        announcement: crate::proto::content_v1::GroupMailboxAddress,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{content, Content};
        use prost::Message;

        // Get our user ID
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Get channel members (excluding ourselves)
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        let other_members: Vec<_> = members
            .iter()
            .filter(|m| m.user_id.to_string() != our_user_id)
            .collect();

        if other_members.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping mailbox announcement (no other members)"
            );
            return Ok(());
        }

        // Build the Content proto with our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();
        let content = Content {
            content: Some(content::Content::GroupMailboxAddress(announcement)),
            ephemeral_reply_to: None,
            timestamp,
            padding: vec![],
        };
        let plaintext = content.encode_to_vec();

        // Use shared token manager for anonymous credential sending
        let token_manager = self.token_manager.clone();

        // Send pairwise to each member via Sealed Sender (Double Ratchet)
        let mut success_count = 0;
        let mut error_count = 0;

        for member in &other_members {
            let member_id = member.user_id.to_string();

            // Encrypt pairwise via Double Ratchet (X3DH if no session exists)
            match self.signal_service
                .encrypt_message(auth_client, &member_id, 1, &plaintext)
                .await
            {
                Ok(sealed_bytes) => {
                    // Get anonymous credential token for this send
                    let token = self.get_or_request_token_for_skdm(&token_manager, &member_id).await;

                    match token {
                        Some(token) => {
                            // Create SealedSenderMessage proto
                            let sealed_message = SealedSenderMessage {
                                sealed_body: sealed_bytes,
                                server_key_id: 1,
                                timestamp,
                            };

                            // Create a fresh message client for the anonymous send
                            let mut message_client = MessageClient::new(
                                self.grpc_pool.channel(),
                                self.grpc_pool.auth_clone(),
                            );

                            // Send with anonymous credential (NDSS '21 blind signature)
                            match message_client
                                .send_sealed_message_with_credential(
                                    &member_id,
                                    0, // All devices
                                    sealed_message,
                                    &token.public_key,
                                    &token.signature,
                                    token.randomizer.as_deref(),
                                )
                                .await
                            {
                                Ok(_) => {
                                    let _ = token_manager.mark_token_used(&token.id);
                                    success_count += 1;
                                    tracing::debug!(
                                        channel_id = %channel_id,
                                        member_id = %member_id,
                                        "Sent mailbox announcement to member via sealed sender"
                                    );
                                }
                                Err(e) => {
                                    error_count += 1;
                                    tracing::warn!(
                                        channel_id = %channel_id,
                                        member_id = %member_id,
                                        error = %e,
                                        "Failed to send mailbox announcement to member"
                                    );
                                }
                            }
                        }
                        None => {
                            error_count += 1;
                            tracing::warn!(
                                channel_id = %channel_id,
                                member_id = %member_id,
                                "No anonymous tokens available for mailbox announcement"
                            );
                        }
                    }
                }
                Err(e) => {
                    error_count += 1;
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to encrypt mailbox announcement for member"
                    );
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            total_members = other_members.len(),
            success = success_count,
            errors = error_count,
            "Distributed mailbox announcement via pairwise sealed sender"
        );

        // Consider it success if at least one member received the announcement
        if success_count == 0 && !other_members.is_empty() {
            return Err(MessageError::EncryptionFailed(
                "Failed to send mailbox announcement to any member".into()
            ));
        }

        Ok(())
    }

    /// Send a GroupMailboxAddress announcement (simplified API - no client params).
    pub async fn announce_group_mailbox(
        &self,
        channel_id: &ChannelId,
        announcement: crate::proto::content_v1::GroupMailboxAddress,
    ) -> Result<(), MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_group_mailbox_announcement(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            announcement,
        )
        .await
    }

    /// Respond to a member's mailbox announcement with our own mailbox.
    ///
    /// Called when we receive a GroupMailboxAddress announcement from a new member.
    /// We respond with our own mailbox so they can send messages to us.
    ///
    /// NDSS '21 mailbox exchange: Both parties need each other's mailbox addresses.
    pub async fn respond_with_mailbox_to_member(
        &self,
        channel_id: &ChannelId,
        to_member_id: &UserId,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{content, Content, GroupMailboxAddress};
        use prost::Message;

        // Get our mailbox for this channel
        let group_state = self.group_mailbox_manager
            .load_group_state(channel_id)
            .map_err(|e| MessageError::MailboxError(e.to_string()))?
            .ok_or_else(|| MessageError::MailboxError("No group state for channel".into()))?;

        let our_mailbox = group_state.our_mailbox
            .ok_or_else(|| MessageError::MailboxError("No mailbox for channel".into()))?;

        // Build announcement proto
        let announcement = GroupMailboxAddress {
            channel_id: channel_id.to_string(),
            mailbox_pk: our_mailbox.public_key.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let timestamp = TimestampNanos::now().as_u64();
        let content = Content {
            timestamp,
            content: Some(content::Content::GroupMailboxAddress(announcement)),
            ..Default::default()
        };
        let plaintext = content.encode_to_vec();

        // Encrypt for this specific member
        let mut auth_client = self.auth_client();
        let sealed_bytes = self.signal_service
            .encrypt_message(&mut auth_client, &to_member_id.to_string(), 1, &plaintext)
            .await
            .map_err(|e| MessageError::EncryptionFailed(e.to_string()))?;

        // Get token for anonymous send
        let database = self.settings_repo.database().clone();
        let token_manager = TokenManager::new(database);
        let token = self.get_or_request_token_for_skdm(&token_manager, &to_member_id.to_string()).await
            .ok_or_else(|| MessageError::MailboxError("No tokens for mailbox response".into()))?;

        // Send via sealed sender
        let sealed_message = SealedSenderMessage {
            sealed_body: sealed_bytes,
            server_key_id: 1,
            timestamp,
        };

        let mut message_client = MessageClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        message_client
            .send_sealed_message_with_credential(
                &to_member_id.to_string(),
                0, // All devices
                sealed_message,
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
            )
            .await?;

        let _ = token_manager.mark_token_used(&token.id);

        tracing::info!(
            channel_id = %channel_id,
            to_member_id = %to_member_id,
            "Sent our mailbox announcement in response to new member"
        );

        Ok(())
    }

    /// Distribute a sender key distribution message to channel members.
    ///
    /// Implements NDSS '21 pairwise distribution:
    /// - Each member receives SKDM + our mailbox announcement
    /// - Encrypted pairwise via Double Ratchet (Sealed Sender envelope)
    /// - Sent to member's device inbox with anonymous credential (server cannot see sender)
    pub async fn distribute_sender_key(
        &self,
        auth_client: &mut AuthClient,
        _message_client: &mut MessageClient,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        // Get our user ID
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Get channel members (excluding ourselves)
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        let other_members: Vec<_> = members
            .iter()
            .filter(|m| m.user_id.to_string() != our_user_id)
            .collect();

        if other_members.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping sender key distribution (no other members)"
            );
            return Ok(());
        }

        // Get our mailbox public key for this channel (if available)
        let ephemeral_reply_to: Option<Vec<u8>> = self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec());
        let has_mailbox = ephemeral_reply_to.is_some();

        // Build the Content proto with SKDM + our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();

        let skdm_content = content_v1::Content {
            ephemeral_reply_to,
            timestamp,
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm.clone())),
        };

        let plaintext = skdm_content.encode_to_vec();

        // Use shared token manager for anonymous credential sending
        let token_manager = self.token_manager.clone();

        // Distribute to each member pairwise via Sealed Sender with anonymous credential
        let mut success_count = 0;
        let mut error_count = 0;

        for member in &other_members {
            let member_id = member.user_id.to_string();

            // Encrypt pairwise via Double Ratchet (X3DH if no session exists)
            match self.signal_service
                .encrypt_message(auth_client, &member_id, 1, &plaintext)
                .await
            {
                Ok(sealed_bytes) => {
                    // Get anonymous credential token for this send
                    let token = self.get_or_request_token_for_skdm(&token_manager, &member_id).await;

                    match token {
                        Some(token) => {
                            // Create SealedSenderMessage proto
                            let sealed_message = SealedSenderMessage {
                                sealed_body: sealed_bytes,
                                server_key_id: 1,
                                timestamp,
                            };

                            // Create a fresh message client for the anonymous send
                            let mut message_client = MessageClient::new(
                                self.grpc_pool.channel(),
                                self.grpc_pool.auth_clone(),
                            );

                            // Send with anonymous credential (NDSS '21 blind signature)
                            match message_client
                                .send_sealed_message_with_credential(
                                    &member_id,
                                    0, // All devices
                                    sealed_message,
                                    &token.public_key,
                                    &token.signature,
                                    token.randomizer.as_deref(),
                                )
                                .await
                            {
                                Ok(_) => {
                                    let _ = token_manager.mark_token_used(&token.id);
                                    success_count += 1;
                                    tracing::debug!(
                                        channel_id = %channel_id,
                                        member_id = %member_id,
                                        "Distributed SKDM to member via sealed sender with credential"
                                    );
                                }
                                Err(e) => {
                                    error_count += 1;
                                    tracing::warn!(
                                        channel_id = %channel_id,
                                        member_id = %member_id,
                                        error = %e,
                                        "Failed to send SKDM to member"
                                    );
                                }
                            }
                        }
                        None => {
                            error_count += 1;
                            tracing::warn!(
                                channel_id = %channel_id,
                                member_id = %member_id,
                                "No anonymous tokens available for SKDM distribution"
                            );
                        }
                    }
                }
                Err(e) => {
                    error_count += 1;
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to encrypt SKDM for member"
                    );
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            total_members = other_members.len(),
            success = success_count,
            errors = error_count,
            has_mailbox = has_mailbox,
            "Distributed sender key via pairwise sealed sender"
        );

        // Consider it success if at least one member received the SKDM
        if success_count == 0 && !other_members.is_empty() {
            return Err(MessageError::EncryptionFailed(
                "Failed to distribute SKDM to any member".into()
            ));
        }

        Ok(())
    }

    /// Distribute sender key to channel members (simplified API - no client params).
    pub async fn distribute_skdm(
        &self,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.distribute_sender_key(&mut auth_client, &mut message_client, channel_id, skdm)
            .await
    }

    /// Send SKDM to a single member (for when a new member joins).
    ///
    /// This is called when we receive a MemberAdded event - we send our
    /// existing SKDM to the new member so they can decrypt our messages.
    pub async fn send_skdm_to_member(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        let mut auth_client = self.auth_client();

        // Get our mailbox public key for this channel (if available)
        let ephemeral_reply_to: Option<Vec<u8>> = self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec());

        // Build the Content proto with SKDM + our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();

        let skdm_content = content_v1::Content {
            ephemeral_reply_to,
            timestamp,
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm.clone())),
        };

        let plaintext = skdm_content.encode_to_vec();
        let member_id_str = member_id.to_string();

        // Encrypt pairwise via Double Ratchet (X3DH if no session exists)
        let sealed_bytes = self.signal_service
            .encrypt_message(&mut auth_client, &member_id_str, 1, &plaintext)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Failed to encrypt SKDM: {}", e)))?;

        // Get anonymous credential token for this send
        let database = self.settings_repo.database().clone();
        let token_manager = TokenManager::new(database);
        let token = self.get_or_request_token_for_skdm(&token_manager, &member_id_str).await
            .ok_or_else(|| MessageError::EncryptionFailed("No anonymous tokens available for SKDM".into()))?;

        // Create SealedSenderMessage proto
        let sealed_message = SealedSenderMessage {
            sealed_body: sealed_bytes,
            server_key_id: 1,
            timestamp,
        };

        // Create a fresh message client for the anonymous send
        let mut message_client = MessageClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        // Send with anonymous credential (NDSS '21 blind signature)
        message_client
            .send_sealed_message_with_credential(
                &member_id_str,
                0, // All devices
                sealed_message,
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| MessageError::Network(e))?;

        let _ = token_manager.mark_token_used(&token.id);

        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            "Sent SKDM to new channel member via sealed sender with credential"
        );

        Ok(())
    }

    /// Create a sender key for a channel (generates or retrieves existing SKDM).
    ///
    /// This creates or retrieves our sender key for the specified channel.
    /// Used when responding to sender key requests.
    pub async fn create_sender_key_for_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::proto::content_v1::SenderKeyDistribution, MessageError> {
        self.signal_service
            .create_sender_key(channel_id)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Failed to create sender key: {}", e)))
    }

    /// Send a sender key request to a channel member.
    ///
    /// Call this when you receive an encrypted message but don't have
    /// the sender's key. The sender should respond with their SKDM.
    pub async fn send_sender_key_request(
        &self,
        channel_id: &ChannelId,
        target_user_id: &UserId,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content, SenderKeyRequest};
        use prost::Message;

        let mut auth_client = self.auth_client();

        // Build the SenderKeyRequest proto
        let timestamp = TimestampNanos::now().as_u64();
        let request = SenderKeyRequest {
            channel_id: channel_id.to_string(),
            target_user_id: target_user_id.to_string(),
            timestamp,
        };

        // Wrap in Content proto
        let content = content_v1::Content {
            content: Some(content::Content::SenderKeyRequest(request.clone())),
            ephemeral_reply_to: None,
            timestamp,
            padding: vec![],
        };

        let plaintext = content.encode_to_vec();
        let target_id_str = target_user_id.to_string();

        // Encrypt via sealed sender
        let sealed_bytes = self.signal_service
            .encrypt_message(&mut auth_client, &target_id_str, 1, &plaintext)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Failed to encrypt sender key request: {}", e)))?;

        // Get anonymous credential token for this send
        let database = self.settings_repo.database().clone();
        let token_manager = TokenManager::new(database);
        let token = self.get_or_request_token_for_skdm(&token_manager, &target_id_str).await
            .ok_or_else(|| MessageError::EncryptionFailed("No anonymous tokens available for sender key request".into()))?;

        // Create SealedSenderMessage proto
        let sealed_message = SealedSenderMessage {
            sealed_body: sealed_bytes,
            server_key_id: 1,
            timestamp,
        };

        // Create a fresh message client for the anonymous send
        let mut message_client = MessageClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        // Send with anonymous credential (NDSS '21 blind signature)
        message_client
            .send_sealed_message_with_credential(
                &target_id_str,
                0, // All devices
                sealed_message,
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| MessageError::Network(e))?;

        let _ = token_manager.mark_token_used(&token.id);

        tracing::info!(
            channel_id = %channel_id,
            target_user_id = %target_user_id,
            "Sent sender key request via sealed sender with credential"
        );

        Ok(())
    }

    /// Send encrypted content to group member mailboxes via GroupFanout.
    ///
    /// This implements the NDSS '21 paper's per-group ephemeral mailboxes for
    /// privacy-preserving group messaging. Messages are fan-out to N member
    /// mailboxes, preventing server-side correlation of group membership.
    pub async fn send_via_group_mailboxes(
        &self,
        group_fanout: &GroupFanout,
        channel_id: &ChannelId,
        encrypted_content: &[u8],
        sender_id: &UserId,
    ) -> Result<(), MessageError> {
        // Check if group mailboxes are ready
        match group_fanout.is_ready_to_send(channel_id) {
            Ok(true) => {
                // Build associated_data with sender info for recipient to extract
                let associated_data = format!("sender:{}:channel:{}", sender_id, channel_id);

                // Wrap in EncryptedData for mailbox delivery
                use prost::Message as ProstMessage;
                let mailbox_envelope = chat_v1::EncryptedData {
                    version: chat_v1::CryptoVersion::SignalProtocol as i32,
                    ciphertext: encrypted_content.to_vec(),
                    associated_data: associated_data.into_bytes(),
                    nonce: vec![],
                    auth_tag: vec![],
                    signature: vec![],
                    device_id: String::new(),
                };
                let envelope_bytes = mailbox_envelope.encode_to_vec();

                // Fan-out to all member mailboxes
                match group_fanout
                    .send_to_group(channel_id, &envelope_bytes, EnvelopeType::Message)
                    .await
                {
                    Ok(result) => {
                        if result.is_complete_success() {
                            tracing::debug!(
                                channel_id = %channel_id,
                                total = result.total_members,
                                "Sent message to all group mailboxes"
                            );
                        } else {
                            tracing::warn!(
                                channel_id = %channel_id,
                                successful = result.successful,
                                failed = result.failed,
                                "Partial delivery to group mailboxes"
                            );
                        }
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel_id,
                            error = %e,
                            "Failed to send to group mailboxes (channel delivery still succeeded)"
                        );
                        // Don't fail - channel delivery already succeeded
                        Ok(())
                    }
                }
            }
            Ok(false) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    "Group mailboxes not ready - skipping fanout"
                );
                Ok(())
            }
            Err(e) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %e,
                    "Could not check group mailbox readiness - skipping fanout"
                );
                Ok(())
            }
        }
    }

    /// Send pairwise-encrypted message to group member mailboxes.
    ///
    /// Per NDSS '21 and Signal Protocol spec: Group messages MUST be encrypted
    /// separately for each member using their pairwise Double Ratchet session
    /// (sealed sender). This provides proper forward secrecy and post-compromise
    /// security per member.
    ///
    /// The sender's identity is cryptographically bound inside each sealed sender
    /// ciphertext and is revealed during SSDecVer (decryption), NOT in metadata.
    pub async fn send_pairwise_to_group_mailboxes(
        &self,
        group_fanout: &GroupFanout,
        channel_id: &ChannelId,
        plaintext: &[u8],
        auth_client: &mut AuthClient,
    ) -> Result<(), MessageError> {
        use std::collections::HashMap;

        // Check if group mailboxes are ready
        match group_fanout.is_ready_to_send(channel_id) {
            Ok(true) => {}
            Ok(false) => {
                // Queue message for later - will be sent when member mailboxes arrive
                // Use a nil UUID to indicate "send to any new member"
                let nil_member = UserId::from_uuid(uuid::Uuid::nil());
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);

                if let Err(e) = group_fanout.queue_pending_message(
                    channel_id,
                    &nil_member,
                    plaintext,
                    timestamp,
                ) {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Failed to queue pending channel message during bootstrap"
                    );
                } else {
                    tracing::info!(
                        channel_id = %channel_id,
                        plaintext_len = plaintext.len(),
                        "[BOOTSTRAP] Queued channel message - no member mailboxes available yet"
                    );
                }
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %e,
                    "Could not check group mailbox readiness - skipping pairwise fanout"
                );
                return Ok(());
            }
        }

        // Get group state to know which members to encrypt for
        let group_state = group_fanout
            .get_group_state(channel_id)
            .map_err(|e| MessageError::MailboxError(e.to_string()))?
            .ok_or_else(|| MessageError::MailboxError("Group state not found".into()))?;

        if group_state.member_mailboxes.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "No member mailboxes - skipping pairwise fanout"
            );
            return Ok(());
        }

        // Encrypt pairwise for each member using sealed sender (Double Ratchet)
        // TODO: Multi-device support for groups would require encrypting to all
        // devices of all members. Current implementation sends to primary device.
        // For full multi-device, need: encrypt_message_all_devices per member.
        let mut member_ciphertexts: HashMap<UserId, Vec<u8>> = HashMap::new();
        let mut success_count = 0;
        let mut error_count = 0;

        for member_id in group_state.member_mailboxes.keys().copied() {
            let member_id: UserId = member_id;
            match self
                .signal_service
                .encrypt_message(
                    auth_client,
                    &member_id.to_string(),
                    1, // Primary device - group multi-device not yet implemented
                    plaintext,
                )
                .await
            {
                Ok(sealed_ciphertext) => {
                    member_ciphertexts.insert(member_id, sealed_ciphertext);
                    success_count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to encrypt pairwise for member"
                    );
                    error_count += 1;
                }
            }
        }

        if member_ciphertexts.is_empty() {
            tracing::warn!(
                channel_id = %channel_id,
                "No successful pairwise encryptions - fanout aborted"
            );
            return Err(MessageError::EncryptionFailed(
                "Failed to encrypt for any group member".into(),
            ));
        }

        // Fan-out pairwise ciphertexts to member mailboxes
        match group_fanout
            .send_to_group_pairwise(channel_id, member_ciphertexts, EnvelopeType::Message)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    channel_id = %channel_id,
                    encrypted = success_count,
                    encrypt_errors = error_count,
                    delivered = result.successful,
                    delivery_errors = result.failed,
                    "Pairwise group fanout complete"
                );

                // TIMING FIX: Queue message for members whose mailboxes haven't arrived yet.
                // This handles the race condition where we send before all member mailboxes
                // have been exchanged. When their mailbox arrives via GroupMailboxReceived,
                // the message will be flushed to them.
                self.queue_for_missing_members(
                    group_fanout,
                    channel_id,
                    plaintext,
                    &group_state.member_mailboxes,
                );

                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Pairwise group fanout failed"
                );
                Err(MessageError::MailboxError(e.to_string()))
            }
        }
    }

    /// Queue a message for channel members whose mailboxes haven't arrived yet.
    ///
    /// This fixes the timing race condition where messages are sent before all
    /// member mailboxes have been exchanged during bootstrap. Uses a nil member ID
    /// to queue for "any new member who arrives", and the message is flushed when
    /// their mailbox is received via GroupMailboxReceived event.
    fn queue_for_missing_members(
        &self,
        group_fanout: &GroupFanout,
        channel_id: &ChannelId,
        plaintext: &[u8],
        known_mailboxes: &std::collections::HashMap<UserId, crate::mailbox::group::MemberMailbox>,
    ) {
        use crate::storage::repositories::settings_repo::keys;
        use std::collections::HashSet;

        // Get our user ID to exclude from comparison
        let our_user_id = match self.settings_repo.get_string(keys::CURRENT_USER_ID) {
            Ok(Some(id)) => id,
            _ => return, // Can't check without our ID
        };

        // Get full member list from channel
        let all_members = match self.channel_repo.list_members(channel_id) {
            Ok(members) => members,
            Err(_) => return, // Can't check without member list
        };

        // Build set of members we've already sent to
        let sent_to: HashSet<String> = known_mailboxes
            .keys()
            .map(|id| id.to_string())
            .collect();

        // Find members without mailboxes (excluding ourselves)
        let missing_members: Vec<_> = all_members
            .iter()
            .filter(|m| {
                let member_id_str = m.user_id.to_string();
                member_id_str != our_user_id && !sent_to.contains(&member_id_str)
            })
            .collect();

        if missing_members.is_empty() {
            // All members have mailboxes - no queueing needed
            return;
        }

        // Queue with nil member ID so it gets sent when ANY new mailbox arrives
        let nil_member = UserId::from_uuid(uuid::Uuid::nil());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        if let Err(e) = group_fanout.queue_pending_message(
            channel_id,
            &nil_member,
            plaintext,
            timestamp,
        ) {
            tracing::warn!(
                channel_id = %channel_id,
                missing_count = missing_members.len(),
                error = %e,
                "[TIMING_FIX] Failed to queue message for members without mailboxes"
            );
        } else {
            tracing::info!(
                channel_id = %channel_id,
                sent_to_count = sent_to.len(),
                missing_count = missing_members.len(),
                "[TIMING_FIX] Queued message for {} member(s) without mailboxes yet",
                missing_members.len()
            );
        }
    }

    /// Get an available anonymous token or request new ones from the server.
    ///
    /// Used for SKDM distribution and sender key requests to ensure messages
    /// are sent with anonymous credentials per NDSS '21.
    async fn get_or_request_token_for_skdm(
        &self,
        token_manager: &TokenManager,
        recipient_id: &str,
    ) -> Option<crate::mailbox::MailboxToken> {
        match token_manager.get_available_token() {
            Ok(Some(t)) => Some(t),
            Ok(None) => {
                tracing::info!(
                    recipient_id = %recipient_id,
                    "No tokens for SKDM send, requesting from server"
                );
                let mut mailbox_client = MailboxClient::new(
                    self.grpc_pool.channel(),
                    self.grpc_pool.auth_clone(),
                );
                match token_manager.request_tokens(&mut mailbox_client, 5).await {
                    Ok(_) => token_manager.get_available_token().ok().flatten(),
                    Err(e) => {
                        tracing::warn!(
                            recipient_id = %recipient_id,
                            error = %e,
                            "Failed to request anonymous tokens for SKDM"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    recipient_id = %recipient_id,
                    error = %e,
                    "Failed to get available token for SKDM"
                );
                None
            }
        }
    }
}
