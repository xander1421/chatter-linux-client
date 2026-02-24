//! Group message operations.
//!
//! Sender key distribution, group mailbox announcements, and N-way fanout delivery.

use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::GroupFanout;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::{self, SealedSenderMessage};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    /// Send a GroupMailboxAddress announcement to a channel.
    ///
    /// Uses send_control_content to encrypt and send the announcement for
    /// the NDSS '21 ephemeral mailbox routing.
    pub async fn send_group_mailbox_announcement(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        announcement: crate::proto::content_v1::GroupMailboxAddress,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{content, Content};

        let content = Content {
            content: Some(content::Content::GroupMailboxAddress(announcement)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        // No group fanout for mailbox announcements - they bootstrap the process
        self.send_control_content(message_client, auth_client, channel_client, channel_id, content, None)
            .await
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

    /// Distribute a sender key distribution message to channel members.
    ///
    /// Implements NDSS '21 pairwise distribution:
    /// - Each member receives SKDM + our mailbox announcement
    /// - Encrypted pairwise via Double Ratchet (Sealed Sender envelope)
    /// - Sent to member's device inbox (server sees sender but not group correlation)
    pub async fn distribute_sender_key(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
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

        // Distribute to each member pairwise via Sealed Sender
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
                    // Create SealedSenderMessage proto
                    let sealed_message = SealedSenderMessage {
                        sealed_body: sealed_bytes,
                        server_key_id: 0, // Server will use current key
                        timestamp,
                    };

                    // Send to member's device inbox
                    match message_client
                        .send_sealed_message(&member_id, 0, sealed_message) // 0 = all devices
                        .await
                    {
                        Ok(_) => {
                            success_count += 1;
                            tracing::debug!(
                                channel_id = %channel_id,
                                member_id = %member_id,
                                "Distributed SKDM to member via sealed sender"
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
}
