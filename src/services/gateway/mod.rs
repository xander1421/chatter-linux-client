//! Message gateway module.
//!
//! Central message routing and validation pipeline.
//! Decrypts sealed sender messages, parses DataMessage proto, and stores to repository.

pub mod builders;
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
    build_read_receipt_content, build_read_receipt_content_with_channel,
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer, build_call_key_answer,
    // Group call key distribution (pairwise sealed sender)
    build_group_call_key_content,
    // Call hangup
    build_call_hangup_content,
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
        /// Sender's profile key (32 bytes) for decrypting their display_name.
        /// Present when sender has enabled profile sharing with us.
        profile_key: Option<Vec<u8>>,
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
    /// Call key offer (ephemeral key exchange for DM calls with PFS).
    ///
    /// Caller sends their ephemeral X25519 public key. Callee responds
    /// with their ephemeral key via CallKeyAnswer.
    CallKeyOffer {
        /// Call ID.
        call_id: String,
        /// Caller's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Timestamp when offer was sent.
        timestamp: u64,
        /// Call type: "Voice Call" or "Video Call".
        call_type: String,
        /// DM channel ID for context.
        channel_id: String,
    },
    /// Call key answer (response to ephemeral key offer).
    ///
    /// Callee responds with their ephemeral X25519 public key.
    /// Both parties then derive: HKDF(ECDH(eph, peer_eph) || identities)
    CallKeyAnswer {
        /// Call ID.
        call_id: String,
        /// Callee's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Timestamp when answer was sent.
        timestamp: u64,
    },
    /// Group call key received (pairwise distribution).
    ///
    /// Symmetric key for group call E2EE, distributed via sealed sender.
    GroupCallKey {
        /// Call ID.
        call_id: String,
        /// AES-256 encryption key (32 bytes).
        key_material: [u8; 32],
        /// Key epoch (increments on rotation).
        key_epoch: u32,
        /// Group channel ID.
        channel_id: String,
        /// Timestamp when key was sent.
        timestamp: u64,
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
    // ========================================================================
    // Anonymous Group Messages (NDSS'21 style)
    // ========================================================================
    /// Invite to join an anonymous group.
    AnonGroupInvite {
        /// Group ID (16 bytes UUID).
        group_id: Vec<u8>,
        /// Human-readable group name.
        group_name: String,
        /// Inviter's mailbox public key for this group (32 bytes Ed25519).
        inviter_mailbox_pk: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
        /// Inviter's profile key (32 bytes).
        profile_key: Option<Vec<u8>>,
        /// Inviter's encrypted display name.
        encrypted_display_name: Option<Vec<u8>>,
        /// Inviter's encrypted avatar.
        encrypted_avatar: Option<Vec<u8>>,
    },
    /// Acknowledgment of joining an anonymous group.
    AnonGroupJoinAck {
        /// Group ID.
        group_id: Vec<u8>,
        /// Joiner's mailbox public key (32 bytes Ed25519).
        joiner_mailbox_pk: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
        /// Joiner's profile key (32 bytes).
        profile_key: Option<Vec<u8>>,
        /// Joiner's encrypted display name.
        encrypted_display_name: Option<Vec<u8>>,
        /// Joiner's encrypted avatar.
        encrypted_avatar: Option<Vec<u8>>,
    },
    /// Member list update for an anonymous group.
    AnonGroupMemberList {
        /// Group ID.
        group_id: Vec<u8>,
        /// List of members with their mailbox keys.
        members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Leave notification for an anonymous group.
    AnonGroupLeave {
        /// Group ID.
        group_id: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Kick notification for an anonymous group.
    AnonGroupKick {
        /// Group ID.
        group_id: Vec<u8>,
        /// User being kicked.
        kicked_user_id: Vec<u8>,
        /// Admin signature.
        admin_signature: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Group update (name change, role change).
    AnonGroupUpdate {
        /// Group ID.
        group_id: Vec<u8>,
        /// Update payload.
        update: crate::proto::content_v1::AnonGroupUpdate,
        /// Timestamp.
        timestamp: u64,
    },
    /// Message within an anonymous group.
    AnonGroupMessage {
        /// Group ID.
        group_id: Vec<u8>,
        /// Inner message content.
        inner_content: Box<crate::proto::content_v1::Content>,
    },
    // ========================================================================
    // Anonymous DM Messages (NDSS'21 style stranger messaging)
    // ========================================================================
    /// Request to start an anonymous DM conversation.
    AnonDmRequest {
        /// Conversation ID (16 bytes UUID).
        conversation_id: Vec<u8>,
        /// Initiator's mailbox public key (32 bytes Ed25519).
        initiator_mailbox_pk: Vec<u8>,
        /// Optional intro message.
        intro_message: Option<String>,
        /// Optional profile key for decrypting sender's profile.
        profile_key: Option<Vec<u8>>,
        /// Encrypted display name (AES-256-GCM with profile_key).
        encrypted_display_name: Option<Vec<u8>>,
        /// Encrypted avatar (AES-256-GCM with profile_key).
        encrypted_avatar: Option<Vec<u8>>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Acceptance of an anonymous DM request.
    AnonDmAccept {
        /// Conversation ID.
        conversation_id: Vec<u8>,
        /// Responder's mailbox public key (32 bytes Ed25519).
        responder_mailbox_pk: Vec<u8>,
        /// Optional profile key for decrypting responder's profile.
        profile_key: Option<Vec<u8>>,
        /// Encrypted display name (AES-256-GCM with profile_key).
        encrypted_display_name: Option<Vec<u8>>,
        /// Encrypted avatar (AES-256-GCM with profile_key).
        encrypted_avatar: Option<Vec<u8>>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Message within an anonymous DM conversation.
    AnonDmMessage {
        /// Conversation ID.
        conversation_id: Vec<u8>,
        /// Inner message content.
        inner_content: Box<crate::proto::content_v1::Content>,
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
    /// Received a sender key request - need to respond with our SKDM.
    RespondWithSkdm {
        channel_id: ChannelId,
        requester_id: UserId,
        skdm: crate::proto::content_v1::SenderKeyDistribution,
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
    /// Regular message stored successfully (no details needed for UI).
    MessageStored,
    /// Data message received and stored - includes details for UI notification.
    DataMessageReceived {
        /// Who sent the message (from SSDecVer).
        sender_id: UserId,
        /// Channel the message belongs to.
        channel_id: ChannelId,
        /// Message ID (server-assigned or generated).
        message_id: MessageId,
        /// Message text body.
        body: String,
        /// Sender's timestamp.
        timestamp: u64,
        /// Attachment pointers if any.
        attachments: Vec<AttachmentPointer>,
    },
    /// Key exchange message received - needs bootstrap handling.
    KeyExchange {
        sender_id: UserId,
        conversation_id: String,
        ephemeral_public_key: Vec<u8>,
        timestamp: u64,
    },
    /// Sender key request received - need to respond with our SKDM.
    SenderKeyRequested {
        /// User who requested our sender key.
        requester_id: UserId,
        /// Channel for which sender key is requested.
        channel_id: ChannelId,
    },
    /// Group mailbox announcement received (NDSS '21 paper bootstrap).
    /// Member announcing their per-group ephemeral mailbox address.
    MailboxAnnouncement {
        /// Who sent the announcement.
        sender_id: UserId,
        /// Channel this mailbox is for.
        channel_id: ChannelId,
        /// The announcement proto.
        announcement: GroupMailboxAddress,
    },
    /// Anonymous group message received.
    AnonGroupMessageReceived {
        /// Who sent the message (revealed via SSDecVer).
        sender_id: UserId,
        /// Group ID (16 bytes).
        group_id: Vec<u8>,
        /// Inner message content.
        inner_content: Box<crate::proto::content_v1::Content>,
    },
    /// Anonymous group join acknowledgment received.
    ///
    /// The joiner is sending their mailbox public key so we can send them messages.
    AnonGroupJoinAckReceived {
        /// Who sent the ack (the joiner).
        sender_id: UserId,
        /// Group ID (16 bytes).
        group_id: Vec<u8>,
        /// Joiner's mailbox public key (32 bytes Ed25519).
        joiner_mailbox_pk: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
        /// Joiner's profile key (32 bytes).
        profile_key: Option<Vec<u8>>,
        /// Joiner's encrypted display name.
        encrypted_display_name: Option<Vec<u8>>,
        /// Joiner's encrypted avatar.
        encrypted_avatar: Option<Vec<u8>>,
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

        // Convert attachments: generate deterministic attachment IDs from CDN keys
        // CDN keys are base64 strings, we hash them to generate consistent UUIDs
        // NOTE: Actual Attachment records are created by AttachmentService.create_from_pointers()
        let attachments: Vec<crate::domain::ids::AttachmentId> = data_message
            .attachments
            .iter()
            .filter(|ptr| !ptr.cdn_key.is_empty())
            .map(|ptr| crate::domain::ids::AttachmentId::from_cdn_key(&ptr.cdn_key))
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
            delivery_status: String::new(),
        };

        // Store in repository
        self.message_repo.insert(&message)?;

        tracing::info!(
            message_id = %message.id,
            channel_id = %message.channel_id,
            attachment_count = message.attachments.len(),
            attachment_ids = ?message.attachments.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "Stored incoming message with attachments"
        );

        // Return details so caller can emit UI events
        Ok(SealedSenderResult::DataMessageReceived {
            sender_id: sender_id.unwrap_or_else(|| UserId::from_uuid(uuid::Uuid::nil())),
            channel_id,
            message_id,
            body: data_message.body,
            timestamp: data_message.timestamp,
            attachments: data_message.attachments,
        })
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

    /// Get the local identity private key for ECIES decryption.
    ///
    /// Returns the 32-byte X25519 private scalar used to decrypt
    /// NDSS'21 initial contact messages.
    pub async fn get_identity_key(&self) -> Result<[u8; 32], crate::error::ServiceError> {
        self.signal_service.get_identity_private_key().await
    }
}
