//! Message routing operations for the message gateway.
//!
//! Handles routing of encrypted messages to appropriate handlers
//! based on message type (sender key, control, distribution, etc.).

use prost::Message as ProstMessage;

use crate::domain::ids::{ChannelId, UserId};
use crate::error::ServiceError;
use crate::proto::content_v1::{Content, content};

use super::{DecryptResult, MessageGateway, RouteResult};

impl MessageGateway {
    /// Check if an EncryptedData is a sender key message.
    ///
    /// Recognizes two formats:
    /// - `sender_key` (direct channel messages)
    /// - `sender:{user_id}:channel:{channel_id}` (group mailbox fanout)
    pub fn is_sender_key_message(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        if encrypted_data.associated_data == b"sender_key" {
            return true;
        }
        // Also check for group mailbox format: sender:{user_id}:channel:{channel_id}
        let associated_str = String::from_utf8_lossy(&encrypted_data.associated_data);
        associated_str.starts_with("sender:") && associated_str.contains(":channel:")
    }

    /// Check if an EncryptedData is a sender key control message (typing, reaction, etc.).
    pub fn is_sender_key_control(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_control"
    }

    /// Check if an EncryptedData is a sender key distribution message.
    pub fn is_sender_key_distribution(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_distribution"
    }

    /// Check if an EncryptedData is a sender key request.
    pub fn is_sender_key_request(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_request"
    }

    /// Route an incoming encrypted message to the appropriate handler.
    ///
    /// Checks the message type and delegates to the correct processing method.
    ///
    /// # Returns
    ///
    /// - `Ok(RouteResult::Decrypted(bytes))` - Successfully decrypted content
    /// - `Ok(RouteResult::Processed)` - Processed control message (SKDM)
    /// - `Ok(RouteResult::NeedsSenderKey { ... })` - Need to request SKDM from sender
    /// - `Err(_)` - Error processing message
    pub async fn route_encrypted_message(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<RouteResult, ServiceError> {
        let associated_data_str = String::from_utf8_lossy(&encrypted_data.associated_data);
        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            associated_data = %associated_data_str,
            ciphertext_len = encrypted_data.ciphertext.len(),
            "[MSG_ROUTE] Routing encrypted message"
        );

        // Check message type based on associated_data marker
        //
        // SECURITY (ponytail): a cleartext `EncryptedData` only reaches this
        // function from the device-inbox plaintext path
        // (inbox/polling/device.rs), where `sender_id` is parsed from the
        // UNAUTHENTICATED, server-attested `DeviceMessage.source_user_id`.
        // Installing a Sender Key *Distribution* Message (SKDM) — which binds a
        // group signing key to (sender, channel) — off that identity would let
        // a malicious/relaying server register ITS OWN signing key under a
        // victim's identity and then forge group-message authorship.
        //
        // Honest clients ALWAYS transmit SKDMs wrapped in a `Content::SenderKey`
        // proto over sealed sender (services/message/group.rs::
        // distribute_sender_key / send_skdm_to_member), where the
        // cryptographically-verified sender certificate drives installation
        // (services/gateway/decryption.rs). That authenticated path is
        // untouched and keeps working. The cleartext SKDM-install path is dead
        // for honest clients, so it is refused here.
        //
        // NOTE: `is_sender_key_message` (group *content*, authenticated by the
        // sender-key SIGNATURE, not by source_user_id) and
        // `is_sender_key_request` are NOT this bug and are preserved below.
        if Self::is_sender_key_distribution(encrypted_data) {
            tracing::warn!(
                sender_id = %sender_id,
                sender_device_id = sender_device_id,
                "SECURITY: refusing cleartext SKDM install from server-attested sender; \
                 SKDMs are accepted only over sealed sender / pairwise ratchet"
            );
            Err(ServiceError::Validation(
                "cleartext sender-key distribution refused (unauthenticated sender)".into(),
            ))
        } else if Self::is_sender_key_message(encrypted_data) {
            tracing::info!(
                sender_id = %sender_id,
                "[MSG_ROUTE] Detected sender key message (sender_key)"
            );
            // Decrypt sender key message (DataMessage)
            match self
                .process_group_encrypted_message(encrypted_data, sender_id, sender_device_id)
                .await?
            {
                DecryptResult::Plaintext(plaintext) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        plaintext_len = plaintext.len(),
                        "[MSG_ROUTE] Decrypted sender key message successfully"
                    );
                    Ok(RouteResult::Decrypted(plaintext))
                }
                DecryptResult::NeedsSenderKey { channel_id, sender_id } => {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_ROUTE] Cannot decrypt - missing sender key"
                    );
                    Ok(RouteResult::NeedsSenderKey { channel_id, sender_id })
                }
            }
        } else if Self::is_sender_key_control(encrypted_data) {
            tracing::debug!(
                sender_id = %sender_id,
                "[MSG_ROUTE] Detected sender key control message"
            );
            // Decrypt sender key control message (Content proto with typing, reaction, etc.)
            match self
                .process_group_encrypted_message(encrypted_data, sender_id, sender_device_id)
                .await?
            {
                DecryptResult::Plaintext(plaintext) => {
                    // Check if this is a GroupMailboxAddress announcement
                    if let Ok(content) = Content::decode(plaintext.as_slice()) {
                        if let Some(content::Content::GroupMailboxAddress(announcement)) = content.content {
                            // Parse channel_id from announcement
                            if let Ok(channel_id) = announcement.channel_id.parse::<ChannelId>() {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    channel_id = %channel_id,
                                    "[MSG_ROUTE] Detected GroupMailboxAddress announcement"
                                );
                                return Ok(RouteResult::MailboxAnnouncement {
                                    channel_id,
                                    announcement,
                                });
                            }
                        }
                    }
                    // Not a mailbox announcement, return as control content
                    tracing::debug!(
                        sender_id = %sender_id,
                        plaintext_len = plaintext.len(),
                        "[MSG_ROUTE] Decrypted control content"
                    );
                    Ok(RouteResult::ControlContent(plaintext))
                }
                DecryptResult::NeedsSenderKey { channel_id, sender_id } => {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_ROUTE] Cannot decrypt control - missing sender key"
                    );
                    Ok(RouteResult::NeedsSenderKey { channel_id, sender_id })
                }
            }
        } else if Self::is_sender_key_request(encrypted_data) {
            tracing::info!(
                sender_id = %sender_id,
                "[MSG_ROUTE] Detected sender key request"
            );
            // Someone is requesting our sender key - return SKDM for sending
            self.process_sender_key_request(encrypted_data, sender_id).await
        } else {
            tracing::error!(
                sender_id = %sender_id,
                associated_data = %associated_data_str,
                "[MSG_ROUTE] Unknown encrypted message type"
            );
            Err(ServiceError::Validation(
                "Unknown encrypted message type".into(),
            ))
        }
    }

    /// Process an incoming sender key request.
    ///
    /// When another member requests our sender key, we create our SKDM
    /// and return it so the caller can send it to the requester.
    async fn process_sender_key_request(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        requester_id: &UserId,
    ) -> Result<super::RouteResult, ServiceError> {
        use crate::proto::content_v1::SenderKeyRequest;

        // Check if this is a sender key request
        if encrypted_data.associated_data != b"sender_key_request" {
            return Err(ServiceError::Validation(
                "Not a sender key request".into(),
            ));
        }

        // Deserialize the SenderKeyRequest proto
        let request = SenderKeyRequest::decode(encrypted_data.ciphertext.as_slice())
            .map_err(|e| {
                ServiceError::Validation(format!("Failed to decode SenderKeyRequest: {}", e))
            })?;

        let channel_id: ChannelId = request.channel_id.parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid channel_id in SenderKeyRequest: {}", e))
        })?;

        // SECURITY: only honor SKDM requests from current channel members.
        //
        // Without this gate, a kicked or banned user can send a
        // `SenderKeyRequest` for the channel they were removed from to any
        // remaining member; the remaining member would ship them a fresh
        // SKDM, defeating the lazy rotation that runs on
        // `ChannelMemberLeft` events. (Signal does not have a `pull-SKDM`
        // RPC at all - SKDMs are piggy-backed on outbound group sends and
        // the sender filters by current `DecryptedGroup` membership.)
        match self.channel_repo.get_membership(&channel_id, requester_id) {
            Ok(Some(_)) => {} // current member — proceed
            Ok(None) => {
                tracing::warn!(
                    requester_id = %requester_id,
                    channel_id = %channel_id,
                    "SECURITY: dropping SenderKeyRequest from non-member"
                );
                return Err(ServiceError::Validation(
                    "requester is not a current channel member".into(),
                ));
            }
            Err(e) => {
                tracing::error!(
                    requester_id = %requester_id,
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to check requester membership; failing closed"
                );
                return Err(ServiceError::Storage(e));
            }
        }

        tracing::info!(
            requester_id = %requester_id,
            channel_id = %channel_id,
            "Received sender key request from confirmed member, creating SKDM to respond"
        );

        // Create our SKDM for this channel
        let skdm = self.signal_service.create_sender_key(&channel_id).await?;

        tracing::info!(
            channel_id = %channel_id,
            requester_id = %requester_id,
            skdm_len = skdm.distribution_message.len(),
            "Created SKDM to send to requester"
        );

        Ok(super::RouteResult::RespondWithSkdm {
            channel_id,
            requester_id: *requester_id,
            skdm,
        })
    }
}
