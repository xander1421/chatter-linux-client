//! Message gateway.
//!
//! Central message routing and validation pipeline.
//! Decrypts sealed sender messages, parses DataMessage proto, and stores to repository.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::crypto::dm_crypto::decrypt_dm_bytes;
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent};
use crate::error::ServiceError;
use crate::proto::content_v1::{
    Content, DataMessage, Delete, DeleteForMeSync, EditMessage, GroupEncrypted,
    GroupMailboxAddress, PinMessage, Reaction, ReceiptMessage, TypingMessage, content,
    typing_message, receipt_message,
};
use crate::services::SignalService;
use crate::storage::repositories::MessageRepository;

// ============================================================================
// Content Builders (for sending)
// ============================================================================

/// Create a Content proto for a text message.
pub fn build_text_content(body: &str) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: now,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for typing indicator.
pub fn build_typing_content(started: bool) -> Content {
    build_typing_content_with_channel(started, None)
}

/// Create a Content proto for typing indicator with optional channel context.
pub fn build_typing_content_with_channel(started: bool, channel_id: Option<&str>) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::Typing(TypingMessage {
            action: if started { typing_message::Action::Started as i32 } else { typing_message::Action::Stopped as i32 },
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for read receipt.
pub fn build_read_receipt_content(message_timestamps: Vec<u64>) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: message_timestamps,
            timestamp: now,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delivery receipt.
pub fn build_delivery_receipt_content(message_timestamps: Vec<u64>) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Delivery as i32,
            timestamps: message_timestamps,
            timestamp: now,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for reaction.
pub fn build_reaction_content(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
) -> Content {
    build_reaction_content_with_channel(emoji, remove, target_author_aci, target_sent_timestamp, None)
}

/// Create a Content proto for reaction with optional channel context.
pub fn build_reaction_content_with_channel(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
    channel_id: Option<&str>,
) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    // Note: Reaction proto doesn't have channel_id field - we encode it in target_author_aci
    // as "channel:{channel_id}:{author_aci}" for channels, or just author_aci for DMs
    let encoded_author = if let Some(ch_id) = channel_id {
        format!("channel:{}:{}", ch_id, target_author_aci)
    } else {
        target_author_aci.to_string()
    };
    Content {
        timestamp: now,
        content: Some(content::Content::Reaction(Reaction {
            emoji: emoji.to_string(),
            remove,
            target_author_aci: encoded_author,
            target_sent_timestamp,
            timestamp: now,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for pin/unpin.
pub fn build_pin_content(
    target_author_aci: &str,
    target_sent_timestamp: u64,
    unpin: bool,
) -> Content {
    build_pin_content_with_channel(target_author_aci, target_sent_timestamp, unpin, None)
}

/// Create a Content proto for pin/unpin with optional channel context.
pub fn build_pin_content_with_channel(
    target_author_aci: &str,
    target_sent_timestamp: u64,
    unpin: bool,
    channel_id: Option<&str>,
) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::Pin(PinMessage {
            target_author_aci: target_author_aci.to_string(),
            target_sent_timestamp,
            unpin,
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for message edit.
pub fn build_edit_content(target_sent_timestamp: u64, new_body: &str) -> Content {
    build_edit_content_with_channel(target_sent_timestamp, new_body, None)
}

/// Create a Content proto for message edit with optional channel context.
pub fn build_edit_content_with_channel(
    target_sent_timestamp: u64,
    new_body: &str,
    channel_id: Option<&str>,
) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::Edit(EditMessage {
            target_sent_timestamp,
            data_message: Some(DataMessage {
                body: new_body.to_string(),
                timestamp: now,
                channel_id: channel_id.unwrap_or_default().to_string(),
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delete-for-me sync.
pub fn build_delete_for_me_content(
    conversation_id: &str,
    target_author_aci: &str,
    target_sent_timestamp: u64,
) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::DeleteForMeSync(DeleteForMeSync {
            conversation_id: conversation_id.to_string(),
            target_author_aci: target_author_aci.to_string(),
            target_sent_timestamp,
            timestamp: now,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delete-for-everyone.
/// This sends a DataMessage with a Delete field to notify the peer.
pub fn build_delete_for_everyone_content(target_sent_timestamp: u64) -> Content {
    build_delete_for_everyone_content_with_channel(target_sent_timestamp, None)
}

/// Create a Content proto for delete-for-everyone with optional channel context.
pub fn build_delete_for_everyone_content_with_channel(
    target_sent_timestamp: u64,
    channel_id: Option<&str>,
) -> Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    Content {
        timestamp: now,
        content: Some(content::Content::DataMessage(DataMessage {
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
            delete: Some(Delete {
                target_sent_timestamp,
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Parsed DM content after decryption.
#[derive(Debug, Clone)]
pub enum DmContent {
    /// Text message
    Text {
        body: String,
        timestamp: u64,
    },
    /// Typing indicator
    Typing {
        action: i32, // 1=Started, 2=Stopped
        timestamp: u64,
    },
    /// Read/delivery receipt
    Receipt {
        receipt_type: i32, // 1=Delivery, 2=Read, 3=Viewed
        timestamps: Vec<u64>,
    },
    /// Emoji reaction
    Reaction {
        emoji: String,
        remove: bool,
        target_author_aci: String,
        target_sent_timestamp: u64,
    },
    /// Pin/unpin message
    Pin {
        target_author_aci: String,
        target_sent_timestamp: u64,
        unpin: bool,
    },
    /// Edit message
    Edit {
        target_sent_timestamp: u64,
        new_body: String,
    },
    /// Delete for me (multi-device sync)
    DeleteForMe {
        conversation_id: String,
        target_author_aci: String,
        target_sent_timestamp: u64,
    },
    /// Delete for everyone (peer notification)
    DeleteForEveryone {
        target_sent_timestamp: u64,
    },
    /// Key exchange for DM bootstrap (response from peer).
    ///
    /// When receiving a bootstrap response, this contains the peer's
    /// mailbox public key and conversation ephemeral key.
    KeyExchange {
        /// Conversation ID this key exchange is for.
        conversation_id: String,
        /// Ephemeral public key (32 or 64 bytes).
        ephemeral_pk: Vec<u8>,
        /// Timestamp of the key exchange.
        timestamp: u64,
    },
}

/// Result of attempting to decrypt a group message.
#[derive(Debug)]
pub enum DecryptResult {
    /// Successfully decrypted the message.
    Plaintext(Vec<u8>),
    /// Missing sender's key - need to request their SKDM.
    NeedsSenderKey {
        channel_id: ChannelId,
        sender_id: UserId,
    },
}

/// Result of routing an encrypted message.
#[derive(Debug)]
pub enum RouteResult {
    /// Successfully decrypted content message (DataMessage).
    Decrypted(Vec<u8>),
    /// Decrypted control content (Content proto with typing, reaction, edit, etc.).
    ControlContent(Vec<u8>),
    /// Processed control message (SKDM, etc.) - no content.
    Processed,
    /// Missing sender's key - need to request their SKDM.
    NeedsSenderKey {
        channel_id: ChannelId,
        sender_id: UserId,
    },
    /// Group mailbox announcement (NDSS '21 paper).
    /// Member announcing their per-group ephemeral mailbox address.
    MailboxAnnouncement {
        /// Channel this mailbox is for.
        channel_id: ChannelId,
        /// The announcement proto.
        announcement: GroupMailboxAddress,
    },
}

/// Result of processing a sealed sender message.
#[derive(Debug)]
pub enum SealedSenderResult {
    /// Regular message stored successfully.
    MessageStored,
    /// Key exchange message received - needs bootstrap handling.
    KeyExchange {
        sender_id: UserId,
        conversation_id: String,
        ephemeral_public_key: Vec<u8>,
        timestamp: u64,
    },
}

/// Message gateway for routing and validation.
pub struct MessageGateway {
    signal_service: SignalService,
    message_repo: Arc<MessageRepository>,
}

impl MessageGateway {
    /// Create a new MessageGateway.
    pub fn new(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
        }
    }

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
        let (sender_address, plaintext) = self
            .signal_service
            .decrypt_message(&envelope.sealed_body)
            .await?;

        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = Content::decode(plaintext.as_slice()) {
            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::debug!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "Received KeyExchange in sealed sender message"
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
            tracing::debug!(
                sender_id = %sender_id,
                "Received non-data content in sealed sender (typing/receipt/etc)"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        // Fallback: Parse as raw DataMessage proto
        let data_message = DataMessage::decode(plaintext.as_slice()).map_err(|e| {
            ServiceError::Validation(format!("Failed to parse DataMessage: {}", e))
        })?;

        self.store_data_message(data_message, Some(sender_id)).await
    }

    /// Store a DataMessage to the repository.
    async fn store_data_message(
        &self,
        data_message: DataMessage,
        sender_id: Option<UserId>,
    ) -> Result<SealedSenderResult, ServiceError> {
        // Parse channel ID (required for group messages)
        let channel_id: ChannelId = if data_message.channel_id.is_empty() {
            // For DMs, derive channel from sender (would need conversation lookup)
            return Err(ServiceError::Validation("Missing channel_id in DataMessage".into()));
        } else {
            data_message.channel_id.parse().map_err(|e| {
                ServiceError::Validation(format!("Invalid channel_id: {}", e))
            })?
        };

        // Generate message ID from timestamp (Signal's approach: timestamp + author = unique)
        let message_id = MessageId::new();

        // Convert timestamp to datetime
        let created_at = if data_message.timestamp > 0 {
            chrono::DateTime::from_timestamp_millis(data_message.timestamp as i64)
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };

        // Extract content
        let content = MessageContent::Text(data_message.body.clone());

        // Convert attachments: extract attachment IDs from AttachmentPointers
        let attachments: Vec<crate::domain::ids::AttachmentId> = data_message
            .attachments
            .iter()
            .filter_map(|ptr| {
                // cdn_key should be a UUID string
                ptr.cdn_key.parse().ok()
            })
            .collect();

        // Create domain message
        let message = Message {
            id: message_id,
            channel_id,
            sender_id,
            content,
            thread_id: None,
            reply_to: None,
            attachments,
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            created_at,
            updated_at: created_at,
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: data_message
                .mentioned_integrations
                .iter()
                .filter_map(|id| id.parse().ok())
                .collect(),
            targets_ai_bot: data_message.targets_ai_bot,
        };

        // Store in repository
        self.message_repo.insert(&message)?;

        tracing::debug!(
            message_id = %message.id,
            channel_id = %message.channel_id,
            "Stored incoming message"
        );

        Ok(SealedSenderResult::MessageStored)
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
        let (sender_address, plaintext) = self
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
            return Ok((sender_id, dm_content));
        }

        // Fallback: try as DataMessage proto (backwards compat)
        if let Ok(data_message) = DataMessage::decode(plaintext.as_slice()) {
            tracing::debug!(
                sender_id = %sender_id,
                text_len = data_message.body.len(),
                "Decrypted DM (DataMessage proto fallback)"
            );
            return Ok((sender_id, DmContent::Text {
                body: data_message.body,
                timestamp: data_message.timestamp,
            }));
        }

        // Last fallback: raw UTF-8 text
        let raw_text = String::from_utf8(plaintext).map_err(|e| {
            ServiceError::Validation(format!("DM content is not valid UTF-8: {}", e))
        })?;
        tracing::debug!(
            sender_id = %sender_id,
            text_len = raw_text.len(),
            "Decrypted DM (raw text fallback)"
        );
        Ok((sender_id, DmContent::Text {
            body: raw_text,
            timestamp: 0,
        }))
    }

    /// Parse Content proto into DmContent.
    fn parse_content(content_proto: Content) -> Result<DmContent, ServiceError> {
        match content_proto.content {
            Some(content::Content::DataMessage(msg)) => {
                // Check if this is a delete request
                if let Some(delete) = msg.delete {
                    return Ok(DmContent::DeleteForEveryone {
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                }
                // Otherwise it's a text message
                Ok(DmContent::Text {
                    body: msg.body,
                    timestamp: msg.timestamp,
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
    /// The decrypted plaintext bytes on success.
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

    /// Check if an EncryptedData is a sender key message.
    pub fn is_sender_key_message(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key"
    }

    /// Check if an EncryptedData is a sender key control message (typing, reaction, etc.).
    pub fn is_sender_key_control(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_control"
    }

    /// Check if an EncryptedData is a sender key distribution message.
    pub fn is_sender_key_distribution(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_distribution"
    }

    /// Process an incoming sender key distribution message.
    ///
    /// When a channel member sends their first encrypted message, they distribute
    /// their SKDM so other members can decrypt future messages from them.
    ///
    /// # Arguments
    ///
    /// * `encrypted_data` - The EncryptedData containing serialized SenderKeyDistribution
    /// * `sender_id` - The sender's user ID
    /// * `sender_device_id` - The sender's device ID
    pub async fn process_sender_key_distribution(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<(), ServiceError> {
        use crate::proto::content_v1::SenderKeyDistribution;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            ciphertext_len = encrypted_data.ciphertext.len(),
            "[SKDM_RECV] Received sender key distribution message"
        );

        // Check if this is a SKDM
        if encrypted_data.associated_data != b"sender_key_distribution" {
            tracing::warn!(
                associated_data = ?String::from_utf8_lossy(&encrypted_data.associated_data),
                "[SKDM_RECV] Not a sender key distribution message"
            );
            return Err(ServiceError::Validation(
                "Not a sender key distribution message".into(),
            ));
        }

        // Deserialize the SenderKeyDistribution proto
        let skdm = SenderKeyDistribution::decode(encrypted_data.ciphertext.as_slice())
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    error = %e,
                    "[SKDM_RECV] Failed to decode SenderKeyDistribution proto"
                );
                ServiceError::Validation(format!("Failed to decode SenderKeyDistribution: {}", e))
            })?;

        tracing::debug!(
            sender_id = %sender_id,
            channel_id = %skdm.channel_id,
            distribution_message_len = skdm.distribution_message.len(),
            timestamp = skdm.timestamp,
            "[SKDM_RECV] Decoded SKDM, processing sender key"
        );

        // Process the SKDM so we can decrypt future messages from this sender
        self.signal_service
            .process_sender_key(sender_id, sender_device_id, &skdm)
            .await
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    channel_id = %skdm.channel_id,
                    error = %e,
                    "[SKDM_RECV] Failed to process sender key"
                );
                e
            })?;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %skdm.channel_id,
            "[SKDM_RECV] Successfully stored sender key from channel member"
        );

        Ok(())
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
        if Self::is_sender_key_distribution(encrypted_data) {
            tracing::info!(
                sender_id = %sender_id,
                "[MSG_ROUTE] Detected SKDM (sender_key_distribution)"
            );
            // Process SKDM - no plaintext returned
            self.process_sender_key_distribution(encrypted_data, sender_id, sender_device_id)
                .await?;
            Ok(RouteResult::Processed)
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
            // Someone is requesting our sender key
            self.process_sender_key_request(encrypted_data, sender_id)
                .await?;
            Ok(RouteResult::Processed)
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

    /// Check if an EncryptedData is a sender key request.
    pub fn is_sender_key_request(encrypted_data: &crate::proto::chat_v1::EncryptedData) -> bool {
        encrypted_data.associated_data == b"sender_key_request"
    }

    /// Process an incoming sender key request.
    ///
    /// When another member requests our sender key, we respond with our SKDM
    /// for the specified channel.
    async fn process_sender_key_request(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        requester_id: &UserId,
    ) -> Result<(), ServiceError> {
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

        tracing::info!(
            requester_id = %requester_id,
            channel_id = %channel_id,
            "Received sender key request, will respond with SKDM"
        );

        // Create and store our SKDM for this channel
        // This ensures our sender key exists and is up to date
        let _skdm = self.signal_service.create_sender_key(&channel_id).await?;

        tracing::debug!(
            channel_id = %channel_id,
            requester_id = %requester_id,
            "Created/refreshed SKDM in response to request"
        );

        // Note: The SKDM will be distributed when we send our next message
        // to the channel (via distribute_sender_key in message_service)

        Ok(())
    }

    /// Create a sender key request message for a channel member.
    ///
    /// Call this when you receive an encrypted message but don't have
    /// the sender's key.
    pub fn create_sender_key_request(
        channel_id: &ChannelId,
        target_user_id: &UserId,
    ) -> crate::proto::content_v1::SenderKeyRequest {
        crate::proto::content_v1::SenderKeyRequest {
            channel_id: channel_id.to_string(),
            target_user_id: target_user_id.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        }
    }
}
