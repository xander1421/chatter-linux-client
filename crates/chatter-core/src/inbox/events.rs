//! Inbox event types.
//!
//! Events emitted by the inbox system for monitoring message flow
//! and coordinating UI updates.

use crate::domain::ids::ChannelId;
use crate::domain::UserId;
use crate::proto::content_v1::AttachmentPointer;
use crate::proto::mailbox_v1::EnvelopeType;

/// Events emitted by the inbox system for monitoring and coordination.
#[derive(Debug, Clone)]
pub enum InboxEvent {
    /// A new message was received from the server.
    MessageReceived {
        /// Unique message identifier.
        message_id: String,
        /// Type of envelope (SealedSender, PrekeyMessage, etc.).
        envelope_type: EnvelopeType,
    },
    /// A message was successfully processed through the pipeline.
    MessageProcessed {
        /// Unique message identifier.
        message_id: String,
    },
    /// A message failed processing.
    MessageFailed {
        /// Unique message identifier.
        message_id: String,
        /// Error description.
        error: String,
    },
    /// A poll cycle completed.
    PollCompleted {
        /// Number of messages received.
        count: u32,
        /// Whether more messages are available.
        has_more: bool,
    },
    /// A poll request failed.
    PollFailed {
        /// Error description.
        error: String,
    },
    /// Message acknowledgment failed.
    AcknowledgeFailed {
        /// Message IDs that failed to acknowledge.
        message_ids: Vec<String>,
        /// Error description.
        error: String,
    },
    /// A DM was received and decrypted.
    DmReceived {
        /// Peer user ID.
        peer_id: UserId,
        /// DM channel ID (for reliable active-conversation matching).
        channel_id: ChannelId,
        /// Decrypted message text.
        text: String,
        /// Server timestamp.
        timestamp: u64,
        /// Message ID (for acknowledgment).
        message_id: String,
        /// Attachment pointers (if any files were included).
        attachments: Vec<AttachmentPointer>,
    },
    /// DM decryption failed.
    DmDecryptionFailed {
        /// Peer user ID.
        peer_id: UserId,
        /// Error description.
        error: String,
        /// Whether all sessions were tried (Sesame convergence failed).
        convergence_failed: bool,
    },
    /// DM retry requested - all sessions failed, need sender to resend.
    ///
    /// This is emitted when Sesame convergence fails and we need the peer
    /// to re-establish a session and resend the message.
    DmRetryRequested {
        /// Peer user ID (who should retry).
        peer_id: UserId,
        /// Original message timestamp (for deduplication).
        original_timestamp: u64,
    },
    /// DM mailbox poll completed.
    DmPollCompleted {
        /// Total messages received across all mailboxes.
        total_messages: u32,
    },
    /// DM unread count was updated.
    DmUnreadCountUpdated {
        /// DM channel ID.
        channel_id: ChannelId,
        /// Peer user ID.
        peer_id: UserId,
        /// New unread count.
        unread_count: u32,
    },
    /// Typing indicator received.
    DmTyping {
        /// Peer user ID.
        peer_id: UserId,
        /// Typing action: 1=Started, 2=Stopped.
        action: i32,
    },
    /// Read/delivery receipt received.
    DmReceipt {
        /// Peer user ID.
        peer_id: UserId,
        /// Receipt type: 1=Delivery, 2=Read, 3=Viewed.
        receipt_type: i32,
        /// Message timestamps being acknowledged.
        timestamps: Vec<u64>,
    },
    /// Reaction received.
    DmReaction {
        /// Peer user ID (who reacted).
        peer_id: UserId,
        /// Emoji.
        emoji: String,
        /// True if removing reaction.
        remove: bool,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Pin/unpin received.
    DmPin {
        /// Peer user ID.
        peer_id: UserId,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// True if unpinning.
        unpin: bool,
    },
    /// Message edit received.
    DmEdit {
        /// Peer user ID.
        peer_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// New message body.
        new_body: String,
    },
    /// Delete for me sync received.
    DmDeleteForMe {
        /// Conversation ID.
        conversation_id: String,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Saved message synced from another device (Note to Self).
    SavedMessageSync {
        /// Message ID (UUID, same across all devices).
        message_id: String,
        /// Message text.
        text: String,
        /// When the message was sent (nanoseconds since epoch).
        sent_at: u64,
        /// Attachment IDs.
        attachment_ids: Vec<String>,
    },
    /// Delete for everyone received (peer wants us to delete).
    DmDeleteForEveryone {
        /// Peer user ID (who sent the delete request).
        peer_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// A channel message was received and decrypted.
    ChannelMessageReceived {
        /// Channel ID.
        channel_id: ChannelId,
        /// Sender user ID.
        sender_id: UserId,
        /// Message ID.
        message_id: String,
        /// Decrypted message text.
        text: String,
        /// Server timestamp (ms since epoch).
        timestamp: u64,
        /// Attachment pointers (if any files were included).
        attachments: Vec<AttachmentPointer>,
    },
    // =========================================================================
    // Channel Control Events (via sender key encryption)
    // =========================================================================
    /// Typing indicator received from channel member.
    ChannelTyping {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who is typing.
        sender_id: UserId,
        /// Typing action: 1=Started, 2=Stopped.
        action: i32,
    },
    /// Reaction received from channel member.
    ChannelReaction {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who reacted.
        sender_id: UserId,
        /// Emoji.
        emoji: String,
        /// True if removing reaction.
        remove: bool,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Pin/unpin received from channel member.
    ChannelPin {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who pinned.
        sender_id: UserId,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// True if unpinning.
        unpin: bool,
    },
    /// Message edit received from channel member.
    ChannelEdit {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who edited.
        sender_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// New message body.
        new_body: String,
    },
    /// Delete for everyone received from channel member.
    ChannelDeleteForEveryone {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who deleted.
        sender_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Delete for me sync received in channel context.
    ChannelDeleteForMe {
        /// Conversation/channel ID.
        conversation_id: String,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Read/delivery receipt received from channel member.
    ChannelReceipt {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who sent the receipt.
        sender_id: UserId,
        /// Receipt type: 1=Delivery, 2=Read, 3=Viewed.
        receipt_type: i32,
        /// Message timestamps being acknowledged.
        timestamps: Vec<u64>,
    },
    /// Channel unread count was updated.
    ChannelUnreadCountUpdated {
        /// Channel ID.
        channel_id: ChannelId,
        /// New unread count.
        unread_count: u32,
    },
    // =========================================================================
    // Group Mailbox Events (ephemeral mailbox routing for privacy)
    // =========================================================================
    /// Group mailbox poll completed.
    GroupMailboxPollCompleted {
        /// Number of groups polled.
        total_groups: u32,
        /// Total messages received across all group mailboxes.
        total_messages: u32,
    },
    /// Group mailbox announcement received from another member.
    GroupMailboxAnnouncement {
        /// Channel ID.
        channel_id: ChannelId,
        /// Sender/member who announced their mailbox.
        member_id: UserId,
        /// Member's mailbox public key (32 bytes Ed25519).
        mailbox_pk: Vec<u8>,
        /// When the mailbox announcement was sent (ms since epoch).
        timestamp: u64,
    },
    /// Missing sender's key - need to request their SKDM.
    SenderKeyNeeded {
        /// Channel ID where message couldn't be decrypted.
        channel_id: ChannelId,
        /// User ID whose sender key we need.
        sender_id: UserId,
    },
    /// Need to respond with our SKDM to a requester.
    RespondWithSkdm {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who requested our sender key.
        requester_id: UserId,
        /// Our SKDM to send (serialized SenderKeyDistribution proto).
        skdm_bytes: Vec<u8>,
    },
    // =========================================================================
    // Friend Request Events
    // =========================================================================
    /// Friend requests were updated (received list changed).
    FriendRequestsUpdated {
        /// Received friend requests.
        received: Vec<FriendRequestInfo>,
        /// Sent friend requests.
        sent: Vec<FriendRequestInfo>,
    },
    /// Friend request was accepted. DMs are channel-free (Signal-style):
    /// the local DM conversation is keyed by the friend's user_id, not a
    /// server-issued channel_id.
    FriendAccepted {
        /// User ID of the friend (who accepted our request).
        friend_user_id: String,
    },
    /// A Signal-style profile-key handshake was received: an empty-body,
    /// no-attachment DataMessage sent at friend-accept time
    /// (`MessageService::send_profile_key_handshake`) whose only job was to
    /// carry the sender's profile key. The gateway already persisted the
    /// key and created no message row — the processor should fetch +
    /// decrypt the sender's profile and refresh the UI.
    ProfileKeyReceived {
        /// User ID of the sender whose profile key we just received.
        sender_id: UserId,
    },
    // =========================================================================
    // DM Bootstrap Events
    // =========================================================================
    /// DM bootstrap completed - shared secret established with peer.
    /// The UI should retry any pending messages for this peer.
    DmBootstrapCompleted {
        /// Peer user ID.
        peer_id: UserId,
    },
    /// A DM mailbox has expired and needs re-establishment.
    DmMailboxExpired {
        /// Peer user ID.
        peer_id: UserId,
    },
    // =========================================================================
    // Call Events (Signal-style ephemeral key exchange)
    // =========================================================================
    /// Call key offer received (DM call initiation with ephemeral key).
    ///
    /// Caller sends their ephemeral X25519 public key. Callee should:
    /// 1. Generate their own ephemeral keypair
    /// 2. Send CallKeyAnswer with their ephemeral public key
    /// 3. Derive call key: HKDF(ECDH(eph, peer_eph) || identities)
    CallKeyOfferReceived {
        /// Peer user ID (caller).
        peer_id: UserId,
        /// Call ID.
        call_id: String,
        /// Caller's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Call type: "Voice Call", "Video Call", etc.
        call_type: String,
        /// Channel/DM ID for context.
        channel_id: String,
        /// Timestamp when offer was sent.
        timestamp: u64,
    },
    /// Call key answer received (response to our offer).
    ///
    /// Callee responds with their ephemeral X25519 public key. Caller should:
    /// 1. Derive call key: HKDF(ECDH(our_eph, peer_eph) || identities)
    /// 2. Connect to LiveKit with derived key
    /// 3. Delete ephemeral private key
    CallKeyAnswerReceived {
        /// Peer user ID (callee).
        peer_id: UserId,
        /// Call ID.
        call_id: String,
        /// Callee's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Timestamp when answer was sent.
        timestamp: u64,
    },
    /// Call hangup notification received via sealed sender.
    CallHangup {
        /// Peer user ID (who ended the call).
        peer_id: UserId,
        /// Call ID.
        call_id: String,
        /// Hangup reason.
        reason: String,
        /// Timestamp when hangup was sent.
        timestamp: u64,
    },
    // =========================================================================
    // Group Call Events (pairwise key distribution)
    // =========================================================================
    /// Group call key received via pairwise sealed sender.
    ///
    /// Each group member receives an independent copy of the symmetric call key.
    /// Per NDSS '21: Distribution is via client-side fanout to preserve privacy.
    GroupCallKeyReceived {
        /// Sender user ID (call initiator or key rotator).
        sender_id: UserId,
        /// Call ID.
        call_id: String,
        /// AES-256 encryption key (32 bytes) for LiveKit E2EE.
        key_material: [u8; 32],
        /// Key epoch (increments on rotation).
        key_epoch: u32,
        /// Channel ID (group channel).
        channel_id: ChannelId,
        /// Timestamp when key was sent.
        timestamp: u64,
    },
    // =========================================================================
    // Anonymous Group Events (NDSS'21 style server-oblivious groups)
    // =========================================================================
    /// Anonymous group invite received.
    AnonGroupInvite {
        /// Peer user ID (inviter).
        peer_id: UserId,
        /// Group ID (16 bytes UUID).
        group_id: Vec<u8>,
        /// Human-readable group name.
        group_name: String,
        /// Inviter's mailbox public key for this group.
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
    /// Anonymous group join acknowledgment received.
    AnonGroupJoinAck {
        /// Peer user ID (joiner).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// Joiner's mailbox public key.
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
    /// Anonymous group member list received.
    AnonGroupMemberList {
        /// Peer user ID (sender).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// Member information.
        members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous group leave notification received.
    AnonGroupLeave {
        /// Peer user ID (leaver).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous group kick notification received.
    AnonGroupKick {
        /// Peer user ID (admin who kicked).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// User being kicked.
        kicked_user_id: Vec<u8>,
        /// Admin signature.
        admin_signature: Vec<u8>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous group update received (name change, role change).
    AnonGroupUpdate {
        /// Peer user ID (sender).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// Update payload.
        update: crate::proto::content_v1::AnonGroupUpdate,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous group message received.
    AnonGroupMessage {
        /// Peer user ID (sender).
        peer_id: UserId,
        /// Group ID.
        group_id: Vec<u8>,
        /// Inner message content.
        inner_content: Box<crate::proto::content_v1::Content>,
    },
    // =========================================================================
    // Anonymous DM Events (NDSS'21 style stranger messaging)
    // =========================================================================
    /// Anonymous DM request received (stranger wants to message us).
    AnonDmRequest {
        /// Peer user ID (initiator).
        peer_id: UserId,
        /// Conversation ID (16 bytes UUID).
        conversation_id: Vec<u8>,
        /// Initiator's mailbox public key.
        initiator_mailbox_pk: Vec<u8>,
        /// Optional intro message.
        intro_message: Option<String>,
        /// Optional profile key for decrypting their profile.
        profile_key: Option<Vec<u8>>,
        /// Encrypted display name (AES-256-GCM with profile_key).
        encrypted_display_name: Option<Vec<u8>>,
        /// Encrypted avatar (AES-256-GCM with profile_key).
        encrypted_avatar: Option<Vec<u8>>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous DM acceptance received (our request was accepted).
    AnonDmAccept {
        /// Peer user ID (responder).
        peer_id: UserId,
        /// Conversation ID.
        conversation_id: Vec<u8>,
        /// Responder's mailbox public key.
        responder_mailbox_pk: Vec<u8>,
        /// Optional profile key for decrypting their profile.
        profile_key: Option<Vec<u8>>,
        /// Responder's display name (plaintext, revealed upon acceptance).
        responder_display_name: Option<String>,
        /// Encrypted display name (AES-256-GCM with profile_key).
        encrypted_display_name: Option<Vec<u8>>,
        /// Encrypted avatar (AES-256-GCM with profile_key).
        encrypted_avatar: Option<Vec<u8>>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous DM message received.
    AnonDmMessage {
        /// Peer user ID (sender).
        peer_id: UserId,
        /// Conversation ID.
        conversation_id: Vec<u8>,
        /// Inner message content.
        inner_content: Box<crate::proto::content_v1::Content>,
    },
}

/// Friend request information for events.
#[derive(Debug, Clone)]
pub struct FriendRequestInfo {
    /// User ID of the requester/recipient.
    pub user_id: String,
    /// Display name.
    pub display_name: String,
    /// Username.
    pub username: String,
    /// Raw ECIES-encrypted intro note (opaque). Non-empty only on RECEIVED
    /// requests that carried a note. The UI decrypts it via
    /// `ChatterClient::decrypt_friend_request_note` (needs our identity key,
    /// unavailable in the inbox emit paths). Empty = no note.
    pub encrypted_intro: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_event_variants() {
        let events = vec![
            InboxEvent::MessageReceived {
                message_id: "msg-1".to_string(),
                envelope_type: EnvelopeType::SealedSender,
            },
            InboxEvent::MessageProcessed {
                message_id: "msg-1".to_string(),
            },
            InboxEvent::MessageFailed {
                message_id: "msg-1".to_string(),
                error: "validation failed".to_string(),
            },
            InboxEvent::PollCompleted {
                count: 10,
                has_more: true,
            },
            InboxEvent::PollFailed {
                error: "network error".to_string(),
            },
            InboxEvent::AcknowledgeFailed {
                message_ids: vec!["msg-1".to_string(), "msg-2".to_string()],
                error: "server error".to_string(),
            },
        ];

        assert_eq!(events.len(), 6);
    }
}
