//! Message gateway module.
//!
//! Central message routing and validation pipeline.
//! Decrypts sealed sender messages, parses DataMessage proto, and stores to repository.

mod builders;
mod decryption;
mod routing;

use std::sync::Arc;

use crate::crypto::signal::sesame::{SesameManager, SqliteSesameStore};
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent};
use crate::proto::content_v1::{AttachmentPointer, DataMessage, GroupMailboxAddress};
use crate::services::SignalService;
use crate::storage::repositories::MessageRepository;
use crate::util::time::TimestampNanos;

// Re-export all content builders
pub use builders::{
    build_text_content, build_data_message_content,
    build_typing_content, build_typing_content_with_channel,
    build_read_receipt_content, build_delivery_receipt_content,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    build_call_key_content, build_call_hangup_content,
};

// ============================================================================
// Types
// ============================================================================

/// Reason for call hangup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHangupReason {
    /// User ended call normally
    Normal,
    /// Call was declined
    Declined,
    /// User is busy (on another call)
    Busy,
    /// Call not answered in time
    Timeout,
    /// Network connectivity issue
    NetworkError,
}

/// Parsed DM content after decryption.
#[derive(Debug, Clone)]
pub enum DmContent {
    /// Text message with optional attachments
    Text {
        body: String,
        timestamp: u64,
        /// Attachment pointers for any files included
        attachments: Vec<AttachmentPointer>,
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
    /// Call key exchange (incoming call notification).
    ///
    /// When someone initiates a call, they send this to notify the peer
    /// and share the call encryption key material.
    CallKey {
        /// Call ID.
        call_id: String,
        /// DTLS fingerprint or SRTP key material.
        key_material: Vec<u8>,
        /// Timestamp when call was initiated.
        timestamp: u64,
        /// Call type: "Voice Call", "Video Call", or "Group Call".
        call_type: String,
        /// Channel/conversation ID for context.
        channel_id: String,
    },
    /// Call hangup notification.
    ///
    /// Sent when a call participant ends the call to notify others.
    CallHangup {
        /// Call ID.
        call_id: String,
        /// Hangup reason.
        reason: CallHangupReason,
        /// Timestamp when hangup was sent.
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

// ============================================================================
// MessageGateway
// ============================================================================

/// Message gateway for routing and validation.
pub struct MessageGateway {
    pub(crate) signal_service: SignalService,
    pub(crate) message_repo: Arc<MessageRepository>,
    pub(crate) sesame_manager: Option<Arc<SesameManager<SqliteSesameStore>>>,
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
            sesame_manager: None,
        }
    }

    /// Create a new MessageGateway with Sesame session management.
    pub fn with_sesame(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            sesame_manager: Some(sesame_manager),
        }
    }

    /// Store a DataMessage to the repository.
    pub(crate) async fn store_data_message(
        &self,
        data_message: DataMessage,
        sender_id: Option<UserId>,
    ) -> Result<SealedSenderResult, crate::error::ServiceError> {
        use crate::error::ServiceError;

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

        // Convert timestamp to datetime (nanoseconds since epoch)
        let created_at = if data_message.timestamp > 0 {
            TimestampNanos::from_nanos(data_message.timestamp)
                .to_datetime()
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

        // Create domain message with sender's timestamp for Signal-style dedup
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
            sent_at: data_message.timestamp,
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
            timestamp: TimestampNanos::now().as_u64(),
        }
    }
}
