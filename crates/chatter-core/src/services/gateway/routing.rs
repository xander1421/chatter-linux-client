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
    pub fn is_sender_key_distribution(
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
    ) -> bool {
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
        // sender-key SIGNATURE, not by source_user_id) is NOT this bug and is
        // preserved below; the cleartext `is_sender_key_request` is refused
        // below for the same reason as this path.
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
                DecryptResult::NeedsSenderKey {
                    channel_id,
                    sender_id,
                } => {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_ROUTE] Cannot decrypt - missing sender key"
                    );
                    Ok(RouteResult::NeedsSenderKey {
                        channel_id,
                        sender_id,
                    })
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
                        if let Some(content::Content::GroupMailboxAddress(announcement)) =
                            content.content
                        {
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
                DecryptResult::NeedsSenderKey {
                    channel_id,
                    sender_id,
                } => {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_ROUTE] Cannot decrypt control - missing sender key"
                    );
                    Ok(RouteResult::NeedsSenderKey {
                        channel_id,
                        sender_id,
                    })
                }
            }
        } else if Self::is_sender_key_request(encrypted_data) {
            // Same rule as the cleartext SKDM install above: honest clients
            // send SenderKeyRequest only over sealed sender, where the sender
            // certificate names the requester and membership is checked
            // (MessageService::answer_sender_key_request). The cleartext form
            // names the requester by server attestation alone, so it is refused.
            tracing::warn!(
                sender_id = %sender_id,
                "SECURITY: refusing cleartext SenderKeyRequest from server-attested sender"
            );
            Err(ServiceError::Validation(
                "cleartext sender-key request refused (unauthenticated sender)".into(),
            ))
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
}
