//! Decryption operations for the message gateway.

use prost::Message as ProstMessage;
use zeroize::Zeroize;

use crate::crypto::dm_crypto::decrypt_dm_bytes;
use crate::crypto::signal::sesame::ConvergenceResult;
use crate::domain::ids::UserId;
use crate::error::ServiceError;
use crate::proto::content_v1::{Content, DataMessage, GroupEncrypted, content};

use super::{CallHangupReason, DecryptResult, DmContent, MessageGateway, SealedSenderResult};

impl MessageGateway {
    /// Process an incoming sealed sender message.
    ///
    /// Decrypts the message and processes it based on content type:
    /// - DataMessage: stores to repository
    /// - KeyExchange: returns for bootstrap handling
    pub async fn process_incoming_message(
        &self,
        envelope: crate::proto::chat_v1::SealedSenderMessage,
    ) -> Result<SealedSenderResult, ServiceError> {
        // Decrypt the sealed sender envelope
        let (sender_address, mut plaintext) = self
            .signal_service
            .decrypt_message(&envelope.sealed_body)
            .await?;

        // Use a scope guard to ensure plaintext is always zeroized
        let result = self.process_incoming_message_inner(&sender_address, &plaintext).await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// Inner implementation that processes decrypted plaintext.
    /// Separated to ensure plaintext zeroization happens in the outer function.
    async fn process_incoming_message_inner(
        &self,
        sender_address: &libsignal_protocol::ProtocolAddress,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        tracing::info!(
            sender_id = %sender_id,
            plaintext_len = plaintext.len(),
            "GATEWAY: Decrypted sealed sender, parsing Content proto"
        );

        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = Content::decode(plaintext) {
            // Log what content type we got
            let content_type_name = match &content_proto.content {
                Some(content::Content::DataMessage(_)) => "DataMessage",
                Some(content::Content::Typing(_)) => "Typing",
                Some(content::Content::Receipt(_)) => "Receipt",
                Some(content::Content::Reaction(_)) => "Reaction",
                Some(content::Content::Edit(_)) => "Edit",
                Some(content::Content::Pin(_)) => "Pin",
                Some(content::Content::KeyExchange(_)) => "KeyExchange",
                Some(content::Content::Attachment(_)) => "Attachment",
                Some(content::Content::SenderKey(_)) => "SenderKey",
                Some(content::Content::GroupEncrypted(_)) => "GroupEncrypted",
                Some(content::Content::SenderKeyRequest(_)) => "SenderKeyRequest",
                Some(content::Content::GroupMailboxAddress(_)) => "GroupMailboxAddress",
                Some(content::Content::CallKey(_)) => "CallKey",
                Some(content::Content::CallHangup(_)) => "CallHangup",
                Some(content::Content::DeleteForMeSync(_)) => "DeleteForMeSync",
                None => "None",
            };

            tracing::info!(
                sender_id = %sender_id,
                content_type = content_type_name,
                timestamp = content_proto.timestamp,
                "GATEWAY: Parsed Content proto, content type = {}", content_type_name
            );

            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::info!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "GATEWAY: Found KeyExchange! key_len={} (0=REQUEST, 64=MESSAGE)",
                    key_exchange.ephemeral_pk.len()
                );
                return Ok(SealedSenderResult::KeyExchange {
                    sender_id,
                    conversation_id: key_exchange.conversation_id,
                    ephemeral_public_key: key_exchange.ephemeral_pk,
                    timestamp: key_exchange.timestamp,
                });
            }

            // Handle DataMessage inside Content
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                return self.store_data_message(data_message, Some(sender_id)).await;
            }

            // Other content types in sealed sender (typing, receipt, etc.)
            // For now just log and return success
            tracing::info!(
                sender_id = %sender_id,
                content_type = content_type_name,
                "GATEWAY: Non-data/non-keyexchange content - returning MessageStored"
            );
            return Ok(SealedSenderResult::MessageStored);
        } else {
            tracing::warn!(
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "GATEWAY: Failed to parse as Content proto, trying DataMessage fallback"
            );
        }

        // Fallback: Parse as raw DataMessage proto
        let data_message = DataMessage::decode(plaintext).map_err(|e| {
            ServiceError::Validation(format!("Failed to parse DataMessage: {}", e))
        })?;

        self.store_data_message(data_message, Some(sender_id)).await
    }

    /// Decrypt an identified sender message (PreKey or Ciphertext).
    ///
    /// This is the fallback path when sealed sender is not used.
    /// The server knows the sender and passes source_user_id/source_device_id.
    ///
    /// When SesameManager is available, uses session convergence to try
    /// multiple sessions if the primary session fails to decrypt.
    pub async fn decrypt_identified_message(
        &self,
        envelope: &[u8],
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<SealedSenderResult, ServiceError> {
        // Try to decrypt using session convergence if Sesame is available
        let mut plaintext = if let Some(ref sesame_manager) = self.sesame_manager {
            match self
                .signal_service
                .decrypt_with_convergence(envelope, &sender_id.to_string(), sender_device_id, sesame_manager.as_ref())
                .await?
            {
                ConvergenceResult::ActiveSession { plaintext, session_id } => {
                    tracing::debug!(
                        sender_id = %sender_id,
                        session_id_hex = %hex::encode(&session_id[..8.min(session_id.len())]),
                        "Decrypted with active session"
                    );
                    plaintext
                }
                ConvergenceResult::PromotedSession { plaintext, session_id } => {
                    tracing::info!(
                        sender_id = %sender_id,
                        session_id_hex = %hex::encode(&session_id[..8.min(session_id.len())]),
                        "Decrypted with promoted session (Sesame convergence)"
                    );
                    plaintext
                }
                ConvergenceResult::AllFailed { errors } => {
                    tracing::warn!(
                        sender_id = %sender_id,
                        error_count = errors.len(),
                        first_error = %errors.first().map(|s| s.as_str()).unwrap_or("none"),
                        "All session convergence attempts failed"
                    );
                    return Err(ServiceError::Crypto(
                        crate::error::CryptoError::Protocol(
                            format!("Session convergence failed: {} sessions tried", errors.len())
                        )
                    ));
                }
            }
        } else {
            // No Sesame manager - use direct decryption
            self.signal_service
                .decrypt_message_identified(envelope, &sender_id.to_string(), sender_device_id)
                .await?
        };

        // Process plaintext and ensure zeroization
        let result = self.decrypt_identified_message_inner(sender_id, &plaintext).await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// Inner implementation for identified message decryption.
    /// Separated to ensure plaintext zeroization in the outer function.
    async fn decrypt_identified_message_inner(
        &self,
        sender_id: &UserId,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = Content::decode(plaintext) {
            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::debug!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "Received KeyExchange in identified sender message"
                );
                return Ok(SealedSenderResult::KeyExchange {
                    sender_id: *sender_id,
                    conversation_id: key_exchange.conversation_id,
                    ephemeral_public_key: key_exchange.ephemeral_pk,
                    timestamp: key_exchange.timestamp,
                });
            }

            // Handle DataMessage inside Content
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                return self.store_data_message(data_message, Some(*sender_id)).await;
            }

            // Other content types
            tracing::debug!(
                sender_id = %sender_id,
                "Received non-data content in identified sender message"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        // Fallback: Parse as raw DataMessage proto
        let data_message = DataMessage::decode(plaintext).map_err(|e| {
            ServiceError::Validation(format!("Failed to parse DataMessage: {}", e))
        })?;

        self.store_data_message(data_message, Some(*sender_id)).await
    }

    /// Process an incoming DM message with layered decryption.
    ///
    /// DM messages have two encryption layers:
    /// 1. Mailbox layer (XChaCha20-Poly1305) - provides sender anonymity from server
    /// 2. Signal layer (sealed sender) - provides forward secrecy via Double Ratchet
    ///
    /// This method decrypts both layers, parses the Content proto,
    /// and returns the parsed content.
    ///
    /// # Arguments
    ///
    /// * `mailbox_encrypted` - The doubly-encrypted envelope from the mailbox
    /// * `shared_secret` - The X25519 shared secret for mailbox layer decryption
    ///
    /// # Returns
    ///
    /// A tuple of (sender_user_id, content) on success.
    pub async fn process_dm_message(
        &self,
        mailbox_encrypted: &[u8],
        shared_secret: &[u8; 32],
    ) -> Result<(UserId, DmContent), ServiceError> {
        // Layer 1: Decrypt mailbox layer (XChaCha20-Poly1305)
        let signal_ciphertext = decrypt_dm_bytes(mailbox_encrypted, shared_secret)?;

        // Layer 2: Decrypt Signal layer (sealed sender with Double Ratchet)
        let (sender_address, mut plaintext) = self
            .signal_service
            .decrypt_message(&signal_ciphertext)
            .await?;

        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        // Try to parse as Content proto first (preferred)
        if let Ok(content_proto) = Content::decode(plaintext.as_slice()) {
            let dm_content = Self::parse_content(content_proto)?;
            tracing::debug!(sender_id = %sender_id, "Decrypted DM (Content proto)");
            // Zeroize plaintext before returning
            plaintext.zeroize();
            return Ok((sender_id, dm_content));
        }

        // Fallback: try as DataMessage proto (backwards compat)
        if let Ok(data_message) = DataMessage::decode(plaintext.as_slice()) {
            tracing::debug!(
                sender_id = %sender_id,
                text_len = data_message.body.len(),
                "Decrypted DM (DataMessage proto fallback)"
            );
            // Zeroize plaintext before returning
            plaintext.zeroize();
            return Ok((sender_id, DmContent::Text {
                body: data_message.body,
                timestamp: data_message.timestamp,
                attachments: data_message.attachments,
            }));
        }

        // Last fallback: raw UTF-8 text
        // Clone to String first, then zeroize the original bytes
        let raw_text = match std::str::from_utf8(&plaintext) {
            Ok(s) => s.to_string(),
            Err(e) => {
                plaintext.zeroize();
                return Err(ServiceError::Validation(format!("DM content is not valid UTF-8: {}", e)));
            }
        };
        // Zeroize plaintext after extracting the text
        plaintext.zeroize();

        tracing::debug!(
            sender_id = %sender_id,
            text_len = raw_text.len(),
            "Decrypted DM (raw text fallback)"
        );
        Ok((sender_id, DmContent::Text {
            body: raw_text,
            timestamp: 0,
            attachments: vec![],
        }))
    }

    /// Parse Content proto into DmContent.
    pub(crate) fn parse_content(content_proto: Content) -> Result<DmContent, ServiceError> {
        match content_proto.content {
            Some(content::Content::DataMessage(msg)) => {
                // Check if this is a delete request
                if let Some(delete) = msg.delete {
                    return Ok(DmContent::DeleteForEveryone {
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                }
                // Log timestamp for debugging
                tracing::debug!(
                    dm_timestamp = msg.timestamp,
                    content_timestamp = content_proto.timestamp,
                    body_len = msg.body.len(),
                    "parse_content: DataMessage extracted"
                );
                // Otherwise it's a text message (possibly with attachments)
                Ok(DmContent::Text {
                    body: msg.body,
                    timestamp: msg.timestamp,
                    attachments: msg.attachments,
                })
            }
            Some(content::Content::Typing(msg)) => {
                Ok(DmContent::Typing {
                    action: msg.action,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::Receipt(msg)) => {
                Ok(DmContent::Receipt {
                    receipt_type: msg.r#type,
                    timestamps: msg.timestamps,
                })
            }
            Some(content::Content::Reaction(msg)) => {
                Ok(DmContent::Reaction {
                    emoji: msg.emoji,
                    remove: msg.remove,
                    target_author_aci: msg.target_author_aci,
                    target_sent_timestamp: msg.target_sent_timestamp,
                })
            }
            Some(content::Content::Pin(msg)) => {
                Ok(DmContent::Pin {
                    target_author_aci: msg.target_author_aci,
                    target_sent_timestamp: msg.target_sent_timestamp,
                    unpin: msg.unpin,
                })
            }
            Some(content::Content::Edit(msg)) => {
                let new_body = msg.data_message
                    .map(|dm| dm.body)
                    .unwrap_or_default();
                Ok(DmContent::Edit {
                    target_sent_timestamp: msg.target_sent_timestamp,
                    new_body,
                })
            }
            Some(content::Content::DeleteForMeSync(msg)) => {
                Ok(DmContent::DeleteForMe {
                    conversation_id: msg.conversation_id,
                    target_author_aci: msg.target_author_aci,
                    target_sent_timestamp: msg.target_sent_timestamp,
                })
            }
            Some(content::Content::KeyExchange(kx)) => {
                // Key exchange received in DM mailbox (bootstrap response from peer)
                Ok(DmContent::KeyExchange {
                    conversation_id: kx.conversation_id,
                    ephemeral_pk: kx.ephemeral_pk,
                    timestamp: kx.timestamp,
                })
            }
            Some(content::Content::CallKey(call_key)) => {
                // Call key exchange received (incoming call notification)
                Ok(DmContent::CallKey {
                    call_id: call_key.call_id,
                    key_material: call_key.key_material,
                    timestamp: call_key.timestamp,
                    call_type: String::new(), // Not in proto, will be set by caller context
                    channel_id: String::new(), // Not in proto, derived from conversation
                })
            }
            Some(content::Content::CallHangup(hangup)) => {
                use crate::proto::content_v1::call_hangup;
                // Call hangup notification received
                let reason = match call_hangup::Reason::try_from(hangup.reason) {
                    Ok(call_hangup::Reason::Normal) => CallHangupReason::Normal,
                    Ok(call_hangup::Reason::Declined) => CallHangupReason::Declined,
                    Ok(call_hangup::Reason::Busy) => CallHangupReason::Busy,
                    Ok(call_hangup::Reason::Timeout) => CallHangupReason::Timeout,
                    Ok(call_hangup::Reason::NetworkError) => CallHangupReason::NetworkError,
                    _ => CallHangupReason::Normal,
                };
                Ok(DmContent::CallHangup {
                    call_id: hangup.call_id,
                    reason,
                    timestamp: hangup.timestamp,
                })
            }
            Some(content::Content::SenderKey(_)) |
            Some(content::Content::GroupEncrypted(_)) |
            Some(content::Content::SenderKeyRequest(_)) => {
                // Group messages shouldn't come through DM path
                Err(ServiceError::Validation("Group message received on DM path".into()))
            }
            _ => {
                Err(ServiceError::Validation("Unknown or empty content type".into()))
            }
        }
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

        // Check if this is a sender key message
        if encrypted_data.associated_data != b"sender_key" {
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

        // Try to decrypt using sender keys
        match self
            .signal_service
            .decrypt_from_group(sender_id, sender_device_id, &group_encrypted)
            .await
        {
            Ok(plaintext) => {
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
