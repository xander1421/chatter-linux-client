//! Inbound message types after decryption.
//!
//! These types represent the successful output of decrypting incoming messages.
//! They are the "normalized" representation that downstream handlers consume,
//! regardless of which encryption layer (sealed sender, DM mailbox, sender key)
//! the message arrived through.
//!
//! Phase 5 of messaging consolidation: inbound normalization (successful paths).

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::proto::content_v1::{AttachmentPointer, GroupMailboxAddress, content};

use super::CallHangupReason;

// =============================================================================
// Content type name (for logging)
// =============================================================================

/// Get a human-readable name for the Content proto's oneof variant.
///
/// Used for consistent logging across all decryption paths.
pub fn content_type_name(content: &Option<content::Content>) -> &'static str {
    match content {
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
        Some(content::Content::GroupCallKey(_)) => "GroupCallKey",
        Some(content::Content::AnonGroupInvite(_)) => "AnonGroupInvite",
        Some(content::Content::AnonGroupJoinAck(_)) => "AnonGroupJoinAck",
        Some(content::Content::AnonGroupMemberList(_)) => "AnonGroupMemberList",
        Some(content::Content::AnonGroupLeave(_)) => "AnonGroupLeave",
        Some(content::Content::AnonGroupKick(_)) => "AnonGroupKick",
        Some(content::Content::AnonGroupUpdate(_)) => "AnonGroupUpdate",
        Some(content::Content::AnonGroupMessage(_)) => "AnonGroupMessage",
        Some(content::Content::AnonDmRequest(_)) => "AnonDmRequest",
        Some(content::Content::AnonDmAccept(_)) => "AnonDmAccept",
        Some(content::Content::AnonDmMessage(_)) => "AnonDmMessage",
        Some(content::Content::AnonGroupProfileShare(_)) => "AnonGroupProfileShare",
        Some(content::Content::AnonCallOffer(_)) => "AnonCallOffer",
        Some(content::Content::AnonCallAnswer(_)) => "AnonCallAnswer",
        Some(content::Content::AnonCallHangup(_)) => "AnonCallHangup",
        Some(content::Content::AnonDmPresence(_)) => "AnonDmPresence",
        Some(content::Content::AnonDmProfileUpdate(_)) => "AnonDmProfileUpdate",
        Some(content::Content::SavedMessageSync(_)) => "SavedMessageSync",
        None => "None",
    }
}

// =============================================================================
// DmContent — parsed content after decryption
// =============================================================================

/// Parsed content after decryption, regardless of transport layer.
///
/// This is the normalized output of `parse_content()`. All decryption paths
/// (sealed sender, DM mailbox, sender key) produce this enum.
/// Handlers match on variants to process specific message types.
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
    KeyExchange {
        conversation_id: String,
        ephemeral_pk: Vec<u8>,
        timestamp: u64,
    },
    /// Call key offer (ephemeral key exchange for DM calls with PFS).
    CallKeyOffer {
        call_id: String,
        ephemeral_public_key: [u8; 32],
        timestamp: u64,
        call_type: String,
        channel_id: String,
    },
    /// Call key answer (response to ephemeral key offer).
    CallKeyAnswer {
        call_id: String,
        ephemeral_public_key: [u8; 32],
        timestamp: u64,
    },
    /// Group call key received (pairwise distribution).
    GroupCallKey {
        call_id: String,
        key_material: [u8; 32],
        key_epoch: u32,
        channel_id: String,
        timestamp: u64,
    },
    /// Call hangup notification.
    CallHangup {
        call_id: String,
        reason: CallHangupReason,
        timestamp: u64,
    },
    /// Saved message sync from another device (Note to Self).
    SavedMessageSync {
        message_id: String,
        text: String,
        sent_at: u64,
        attachment_ids: Vec<String>,
    },
    // ========================================================================
    // Anonymous Group Messages (NDSS'21 style)
    // ========================================================================
    AnonGroupInvite {
        group_id: Vec<u8>,
        group_name: String,
        inviter_mailbox_pk: Vec<u8>,
        timestamp: u64,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        encrypted_avatar: Option<Vec<u8>>,
    },
    AnonGroupJoinAck {
        group_id: Vec<u8>,
        joiner_mailbox_pk: Vec<u8>,
        timestamp: u64,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        encrypted_avatar: Option<Vec<u8>>,
    },
    AnonGroupMemberList {
        group_id: Vec<u8>,
        members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
        timestamp: u64,
    },
    AnonGroupLeave {
        group_id: Vec<u8>,
        timestamp: u64,
    },
    AnonGroupKick {
        group_id: Vec<u8>,
        kicked_user_id: Vec<u8>,
        admin_signature: Vec<u8>,
        timestamp: u64,
    },
    AnonGroupUpdate {
        group_id: Vec<u8>,
        update: crate::proto::content_v1::AnonGroupUpdate,
        timestamp: u64,
    },
    AnonGroupMessage {
        group_id: Vec<u8>,
        inner_content: Box<crate::proto::content_v1::Content>,
    },
    // ========================================================================
    // Anonymous DM Messages (NDSS'21 style stranger messaging)
    // ========================================================================
    AnonDmRequest {
        conversation_id: Vec<u8>,
        initiator_mailbox_pk: Vec<u8>,
        intro_message: Option<String>,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        encrypted_avatar: Option<Vec<u8>>,
        timestamp: u64,
    },
    AnonDmAccept {
        conversation_id: Vec<u8>,
        responder_mailbox_pk: Vec<u8>,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        encrypted_avatar: Option<Vec<u8>>,
        timestamp: u64,
    },
    AnonDmMessage {
        conversation_id: Vec<u8>,
        inner_content: Box<crate::proto::content_v1::Content>,
    },
}

// =============================================================================
// Decryption result types
// =============================================================================

/// Result of attempting to decrypt a group (sender key) message.
#[derive(Debug)]
pub enum DecryptResult {
    /// Successfully decrypted the message.
    Plaintext(Vec<u8>),
    /// Missing sender's key — need to request their SKDM.
    NeedsSenderKey {
        channel_id: ChannelId,
        sender_id: UserId,
    },
}

/// Result of routing an encrypted channel message.
#[derive(Debug)]
pub enum RouteResult {
    /// Successfully decrypted content message (DataMessage).
    Decrypted(Vec<u8>),
    /// Decrypted control content (Content proto with typing, reaction, edit, etc.).
    ControlContent(Vec<u8>),
    /// Processed control message (SKDM, etc.) — no content.
    Processed,
    /// Missing sender's key — need to request their SKDM.
    NeedsSenderKey {
        channel_id: ChannelId,
        sender_id: UserId,
    },
    /// Received a sender key request — need to respond with our SKDM.
    RespondWithSkdm {
        channel_id: ChannelId,
        requester_id: UserId,
        skdm: crate::proto::content_v1::SenderKeyDistribution,
    },
    /// Group mailbox announcement (NDSS '21 paper).
    MailboxAnnouncement {
        channel_id: ChannelId,
        announcement: GroupMailboxAddress,
    },
}

/// Result of processing a sealed sender message.
#[derive(Debug)]
pub enum SealedSenderResult {
    /// Regular message stored successfully (no details needed for UI).
    MessageStored,
    /// Data message received and stored — includes details for UI notification.
    DataMessageReceived {
        sender_id: UserId,
        channel_id: ChannelId,
        message_id: MessageId,
        body: String,
        timestamp: u64,
        attachments: Vec<AttachmentPointer>,
    },
    /// Key exchange message received — needs bootstrap handling.
    KeyExchange {
        sender_id: UserId,
        conversation_id: String,
        ephemeral_public_key: Vec<u8>,
        timestamp: u64,
    },
    /// Sender key request received — need to respond with our SKDM.
    SenderKeyRequested {
        requester_id: UserId,
        channel_id: ChannelId,
    },
    /// Group mailbox announcement received (NDSS '21 paper bootstrap).
    MailboxAnnouncement {
        sender_id: UserId,
        channel_id: ChannelId,
        announcement: GroupMailboxAddress,
    },
    /// Anonymous group message received.
    AnonGroupMessageReceived {
        sender_id: UserId,
        group_id: Vec<u8>,
        inner_content: Box<crate::proto::content_v1::Content>,
    },
    /// Anonymous group join acknowledgment received.
    AnonGroupJoinAckReceived {
        sender_id: UserId,
        group_id: Vec<u8>,
        joiner_mailbox_pk: Vec<u8>,
        timestamp: u64,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        encrypted_avatar: Option<Vec<u8>>,
    },
    /// Channel control content (reaction, pin, edit, typing, delete) received
    /// via per-device sealed sender — caller dispatches to the right event.
    ControlContent {
        sender_id: UserId,
        content: Box<crate::proto::content_v1::Content>,
    },
}
