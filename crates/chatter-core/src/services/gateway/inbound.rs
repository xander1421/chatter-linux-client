//! Inbound message types after decryption.
//!
//! These types represent the successful output of decrypting incoming messages.
//! They are the "normalized" representation that downstream handlers consume,
//! regardless of which encryption layer (sealed sender, DM mailbox, sender key)
//! the message arrived through.
//!
//! Phase 5 of messaging consolidation: inbound normalization (successful paths).

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::Message;
use crate::proto::content_v1::{AttachmentPointer, GroupMailboxAddress, Quote, content};
use crate::storage::repositories::UnifiedMessageRepository;

use super::CallHangupReason;

/// Populate a domain `Message`'s reply fields from an inbound DataMessage quote.
///
/// Shared by the non-anon inbound paths (`store_data_message` for ServerDm,
/// `dispatch_dm_content` for the mailbox transport) so the quote is parsed in one place.
/// Resolves the parent's real stored id by (author, sent_at) — so
/// `reply_parent_missing` resolves correctly — and only falls back to a
/// synthetic timestamp id when the parent isn't stored locally (then the UI
/// legitimately shows "Original message not found").
///
/// Anon paths deliberately keep their own snapshot semantics (the quote never
/// needs a parent lookup), so they don't route through here.
pub fn apply_inbound_quote(
    message: &mut Message,
    quote: Option<&Quote>,
    msg_repo: &UnifiedMessageRepository,
) {
    let Some(q) = quote else { return };
    let author: Option<UserId> = q.author_aci.parse().ok();
    let real_parent = author
        .and_then(|a| msg_repo.get_by_sender_timestamp(&a, q.id).ok().flatten())
        .map(|m| m.id);
    message.reply_to = Some(real_parent.unwrap_or_else(|| MessageId::from_timestamp(q.id)));
    message.reply_to_author_id = author;
    message.reply_to_text = (!q.text.is_empty()).then(|| q.text.clone());
    if let Some(qa) = q.attachments.first() {
        message.reply_to_thumbnail =
            (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
        message.reply_to_thumbnail_mime =
            (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
    }
}

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
        Some(content::Content::DmMailboxRotation(_)) => "DmMailboxRotation",
        Some(content::Content::SyncSent(_)) => "SyncSent",
        Some(content::Content::ReadSync(_)) => "ReadSync",
        Some(content::Content::ProfileKeySync(_)) => "ProfileKeySync",
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
        /// Reply quote, so the receiver's stored message keeps its reply
        /// bubble (author + preview + thumbnail). `None` for non-replies.
        quote: Option<Quote>,
        /// Thread root this message replies into (DataMessage.thread_id on
        /// the wire), so a mailbox-delivered reply lands in the thread panel instead
        /// of the main channel — mirrors the ServerDm inbound path in
        /// `gateway/mod.rs`, which already carries this through.
        thread_id: Option<String>,
        /// The disappearing timer this message states for the conversation
        /// (`DataMessage.ephemeral_ttl_seconds`, `0` = off). `None` when the
        /// payload could not carry one (the legacy fallbacks), which must not
        /// be read as "off".
        expire_timer: Option<u32>,
        /// A timer-update announcement (Signal's `EXPIRATION_TIMER_UPDATE`):
        /// nothing to show but the timer change.
        timer_update: bool,
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
    /// Delete for everyone (peer notification)
    DeleteForEveryone {
        target_sent_timestamp: u64,
    },
    /// Key exchange for DM bootstrap (response from peer).
    KeyExchange {
        /// The sending device — the address a bootstrap reply is encrypted to.
        sender_device_id: u32,
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
    /// Mailbox rotation announce (L4): the peer minted a new mailbox and wants
    /// us to send future messages to it. We update the stored peer mailbox.
    MailboxRotation {
        /// The new mailbox public key to send to (32 bytes Ed25519).
        new_mailbox_pk: Vec<u8>,
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
    /// Not content at all: the authenticated sender is blocked, so the
    /// envelope was never parsed. The DM-mailbox twin of
    /// [`SealedSenderResult::DroppedBlockedSender`] — same decision, same
    /// point in the pipeline, a different return shape because
    /// `process_dm_message` hands back content rather than an outcome.
    ///
    /// Dispatching this must do nothing at all; the caller still acks.
    DroppedBlockedSender,
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
        /// The message carried a profile key that is new or changed for this
        /// sender (now stored). Caller emits `InboxEvent::ProfileKeyReceived`
        /// so the sender's profile is re-fetched, as for the pure handshake.
        profile_key_changed: bool,
    },
    /// Pure profile-key handshake received: an empty-body, no-attachment
    /// DataMessage sent at friend-accept time (see
    /// `MessageService::send_profile_key_handshake`) whose only job is to
    /// carry the sender's profile key. The gateway already persisted the
    /// key (`store_data_message`) — no message row was created. Caller
    /// should trigger a profile refresh (fetch + decrypt) for `sender_id`.
    ProfileKeyReceived {
        sender_id: UserId,
    },
    /// Key exchange message received — needs bootstrap handling.
    KeyExchange {
        sender_id: UserId,
        /// The sending device — the address a bootstrap reply is encrypted to.
        sender_device_id: u32,
        conversation_id: String,
        ephemeral_public_key: Vec<u8>,
        timestamp: u64,
    },
    /// Sender key request received — need to respond with our SKDM.
    SenderKeyRequested {
        requester_id: UserId,
        channel_id: ChannelId,
    },
    /// A sender-key distribution message (SKDM) was received and the sender
    /// key was installed. The caller MUST fire `DomainEvent::SenderKeyReceived`
    /// so any group messages that arrived from this (sender, device, channel)
    /// BEFORE the key — and were parked in `pending_group_decrypts` — get
    /// re-decrypted. Without this signal the first message from a new speaker
    /// is lost forever even after the key lands (finding F14c).
    SenderKeyProcessed {
        sender_id: UserId,
        sender_device_id: u32,
        channel_id: ChannelId,
        distribution_message: Vec<u8>,
        timestamp: u64,
        /// The SKDM carried the sender's profile key and it was new or
        /// rotated. Caller emits `InboxEvent::ProfileKeyReceived` so the
        /// member's name and avatar are fetched and decrypted.
        profile_key_changed: bool,
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
    /// RECEIVER half of the 1:1 session self-heal: an inner Whisper/PreKey
    /// message from an authenticated sender failed to decrypt (desynced
    /// session). Carries a serialized `DecryptionErrorMessage` that the caller
    /// must ship back to `(sender_id, sender_device_id)` sealed as
    /// session-less `PlaintextContent`
    /// (`MessageService::send_decryption_error_report`).
    DecryptionErrorNeedsReport {
        sender_id: UserId,
        sender_device_id: u32,
        dem_serialized: Vec<u8>,
    },
    /// SENDER half of the 1:1 session self-heal: a peer's
    /// `DecryptionErrorMessage` about a message we sent was processed.
    /// `archived` is true when our session with `(peer_id, peer_device_id)`
    /// was archived — the next send re-establishes via PreKeySignalMessage.
    SessionHealed {
        peer_id: UserId,
        peer_device_id: u32,
        archived: bool,
    },
    /// SENDER half of the sender-key retry: a peer's `DecryptionErrorMessage`
    /// named a group message we sent (no ratchet key). The caller resends that
    /// content pairwise to `(peer_id, peer_device_id)`
    /// (`MessageService::resend_on_decryption_error`). `timestamp_ms` is the
    /// failed message's server timestamp in epoch millis.
    ResendRequested {
        peer_id: UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    },
    /// The sender is blocked. The envelope decrypted and the sender was
    /// authenticated — by the sealed sender certificate, or by holding the
    /// Double Ratchet session for the address it claimed — and the gateway
    /// then refused to act on it. Nothing was parsed, nothing was stored,
    /// nothing is reported to the user.
    ///
    /// This is the whole of client-side block enforcement for DMs, and it has
    /// to live here rather than on the server. Blocking a sealed send server-
    /// side would require binding a sender identity to it, which is precisely
    /// the guarantee sealed sender exists to remove: `DeviceMessage`'s
    /// `SourceUserID` is empty on that path, the prekey RPC that used to gate
    /// it is skipped entirely once a session exists, and it answers anonymous
    /// callers anyway. Signal-Server holds no block list for the same reason;
    /// Signal-Desktop drops in `MessageReceiver` *after* sealed-sender
    /// decryption, which is this point.
    ///
    /// The caller MUST still acknowledge the envelope — see
    /// [`crate::inbox::ack_policy::drop_and_ack`]. Leaving it queued means
    /// re-fetching and re-decrypting it on every poll for as long as the block
    /// stands.
    DroppedBlockedSender {
        sender_id: UserId,
    },
}
