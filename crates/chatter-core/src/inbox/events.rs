//! Inbox event types.
//!
//! Events emitted by the inbox system for monitoring message flow
//! and coordinating UI updates.

use crate::domain::UserId;
use crate::domain::ids::ChannelId;
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
        /// The session with this peer is desynced: reset it (archive, rebuild).
        session_desync: bool,
    },
    /// A 1:1 inner decrypt failed (desynced session) and a
    /// `DecryptionErrorMessage` must be shipped back to the sender so their
    /// session archives and re-establishes (Signal 1:1 self-heal, receiver
    /// half). Handled by `InboxEventProcessor` →
    /// `MessageService::send_decryption_error_report`.
    DmDecryptionErrorNeedsReport {
        /// Authenticated sender of the message we could not decrypt.
        peer_id: UserId,
        /// Their sending device (the session that is desynced).
        peer_device_id: u32,
        /// Serialized libsignal `DecryptionErrorMessage`.
        dem_serialized: Vec<u8>,
    },
    /// A member could not decrypt a sender-key message WE sent and asked for
    /// it again (`DecryptionErrorMessage` without a ratchet key). Handled by
    /// `InboxEventProcessor` → `MessageService::resend_on_decryption_error`.
    GroupResendRequested {
        /// Authenticated member that failed to decrypt.
        peer_id: UserId,
        /// Their device that failed.
        peer_device_id: u32,
        /// Server timestamp of the failed message, epoch millis.
        timestamp_ms: u64,
    },
    /// A friend's device could not decrypt a DM WE sent and asked for it again
    /// (`DecryptionErrorMessage` with a ratchet key). Handled by
    /// `InboxEventProcessor` → `MessageService::resend_dm_on_decryption_error`.
    DmResendRequested {
        /// Authenticated peer that failed to decrypt.
        peer_id: UserId,
        /// Their device that failed.
        peer_device_id: u32,
        /// Server timestamp of the failed envelope, epoch millis.
        timestamp_ms: u64,
    },
    /// DM mailbox poll completed.
    DmPollCompleted {
        /// Total messages received across all mailboxes.
        total_messages: u32,
    },
    /// Typing indicator received.
    DmTyping {
        /// Peer user ID.
        peer_id: UserId,
        /// Typing action: 1=Started, 2=Stopped.
        action: i32,
        /// Sender's TypingMessage.timestamp (ns).
        timestamp: u64,
    },
    /// Read/delivery receipt received.
    DmReceipt {
        /// Peer user ID.
        peer_id: UserId,
        /// Receipt type: 1=Delivery, 2=Read, 3=Viewed.
        receipt_type: i32,
        /// Message timestamps being acknowledged.
        timestamps: Vec<u64>,
        /// Authenticated sender device, never a device claimed inside the receipt.
        sender_device_id: u32,
    },
    /// Reaction received.
    DmReaction {
        /// Transport conversation peer when the delivery path names one (DM
        /// mailbox polling). `None` from `dispatch_control_content`, whose
        /// envelope carries no peer; admission then takes the peer from the
        /// target's encoded destination (own-device reactions) or the reactor.
        conversation_peer_id: Option<UserId>,
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
        /// When the action was made (its own `timestamp`, else the server's;
        /// epoch nanos): orders actions held for a message not here yet.
        timestamp: u64,
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
        /// When the action was made (its own `timestamp`, else the server's;
        /// epoch nanos): orders actions held for a message not here yet.
        timestamp: u64,
    },
    /// Message edit received.
    DmEdit {
        /// Peer user ID.
        peer_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// New message body.
        new_body: String,
        /// The edit's own time (`EditMessage.data_message.timestamp`, epoch
        /// nanos): orders edits and dates the bubble.
        edit_timestamp: u64,
        /// When the server received the edit (epoch nanos; 0 if unknown):
        /// the 48h edit window is measured on it, as Signal does.
        server_timestamp: u64,
    },
    /// Snapshot of one Saved record (Note to Self) from another of our
    /// devices: creation, edit or pin. `sender_id` is the sealed-sender
    /// identity; only our own is accepted.
    SavedMessageSync {
        /// Sender (must be our own user).
        sender_id: UserId,
        /// The snapshot, applied whole by `MessageService::apply_saved_message_sync`.
        sync: crate::proto::content_v1::SavedMessageSync,
    },
    /// Snapshot already durably applied before acknowledging its envelope.
    SavedSnapshotApplied {
        sender_id: UserId,
        outcome: crate::services::message::SavedSyncOutcome,
    },
    /// Delete for everyone received (peer wants us to delete).
    DmDeleteForEveryone {
        target_author_aci: Option<String>,
        /// Encrypted self-device destination; accepted only when peer_id is us.
        destination_user_id: Option<String>,
        target_call_id: Option<String>,
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
        /// Sender's TypingMessage.timestamp (ns).
        timestamp: u64,
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
        /// When the action was made (its own `timestamp`, else the server's;
        /// epoch nanos): orders actions held for a message not here yet.
        timestamp: u64,
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
        /// When the action was made (its own `timestamp`, else the server's;
        /// epoch nanos): orders actions held for a message not here yet.
        timestamp: u64,
    },
    /// A channel call started or ended (`content_v1::CallEvent`).
    ChannelCallEvent {
        /// Channel ID.
        channel_id: ChannelId,
        /// Member who posted it (not yet checked against the roster).
        sender_id: UserId,
        /// The call's id as sent.
        call_id: String,
        /// Started or ended.
        phase: crate::services::message::ChannelCallPhase,
        /// Ended only: seconds the call ran.
        duration_secs: u32,
        /// Sender's timestamp, else the server's (epoch nanos).
        timestamp: u64,
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
        /// The edit's own time; see [`InboxEvent::DmEdit`].
        edit_timestamp: u64,
        /// When the server received the edit; see [`InboxEvent::DmEdit`].
        server_timestamp: u64,
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
    /// Messages read on another device (`ReadSync`). `sender_id` is the
    /// sealed sender: only our own user may mark our messages read.
    ReadSync {
        sender_id: UserId,
        sync: crate::proto::content_v1::ReadSync,
    },
    /// Our profile key and its visibility from another of our devices
    /// (`ProfileKeySync`). Only our own user may send one.
    ProfileKeySync {
        sender_id: UserId,
        sync: crate::proto::content_v1::ProfileKeySync,
    },
    /// Our privacy settings from another of our devices
    /// (`ConfigurationSync`). Only our own user may send one.
    ConfigurationSync {
        sender_id: UserId,
        sync: crate::proto::content_v1::ConfigurationSync,
    },
    /// Delete-for-me from another of our devices (DM or channel).
    /// `sender_id` is the sealed sender: only our own user may send one.
    ChannelDeleteForMe {
        sender_id: UserId,
        /// Conversation/channel ID.
        conversation_id: String,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        saved_message_id: String,
        saved_messages_scope: bool,
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
        /// Authenticated sender device, never a device claimed inside the receipt.
        sender_device_id: u32,
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
        /// When the server received the offer (ns) — what the ring timeout
        /// ages it by, as Signal does. The caller's own stamp only when the
        /// server gave none.
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
        reason: crate::services::gateway::CallHangupReason,
        /// Timestamp when hangup was sent.
        timestamp: u64,
    },
    // =========================================================================
    // Group Call Events (pairwise key distribution)
    // =========================================================================
    /// Channel group-call signal received via pairwise sealed sender: a
    /// ring (no key), the key authority's rotation, or a key request.
    /// Per NDSS '21: distribution is client-side fanout, to preserve privacy.
    GroupCallSignalReceived {
        /// Authenticated sender.
        sender_id: UserId,
        /// Call ID.
        call_id: String,
        /// Channel ID (group channel).
        channel_id: ChannelId,
        signal: crate::domain::call::GroupCallSignal,
        /// Timestamp when the signal was sent.
        timestamp: u64,
    },
    // =========================================================================
    // Anonymous Group Events (NDSS'21 style server-oblivious groups)
    // =========================================================================
    /// Anonymous group invite received.
    AnonGroupInvite {
        /// Peer user ID (inviter).
        peer_id: UserId,
        /// The sealed-sender-authenticated device it came from: the one
        /// device this secret chat is bound to.
        sender_device_id: u32,
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
        /// Pointer to the inviter's avatar upload.
        avatar: Option<crate::proto::content_v1::AttachmentPointer>,
    },
    /// Anonymous group join acknowledgment received.
    AnonGroupJoinAck {
        /// Peer user ID (joiner).
        peer_id: UserId,
        /// The sealed-sender-authenticated device it came from: the one
        /// device this secret chat is bound to.
        sender_device_id: u32,
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
        /// Pointer to the joiner's avatar upload.
        avatar: Option<crate::proto::content_v1::AttachmentPointer>,
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
        /// The sealed-sender-authenticated device it came from: the one
        /// device this secret chat is bound to.
        sender_device_id: u32,
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
        /// Pointer to the peer's avatar upload (fetched over Tor).
        avatar: Option<crate::proto::content_v1::AttachmentPointer>,
        /// Timestamp.
        timestamp: u64,
    },
    /// Anonymous DM acceptance received (our request was accepted).
    AnonDmAccept {
        /// Peer user ID (responder).
        peer_id: UserId,
        /// The sealed-sender-authenticated device it came from: the one
        /// device this secret chat is bound to.
        sender_device_id: u32,
        /// Conversation ID.
        conversation_id: Vec<u8>,
        /// Responder's mailbox public key.
        responder_mailbox_pk: Vec<u8>,
        /// Optional profile key for decrypting their profile.
        profile_key: Option<Vec<u8>>,
        /// Encrypted display name (AES-256-GCM with profile_key).
        encrypted_display_name: Option<Vec<u8>>,
        /// Pointer to the peer's avatar upload (fetched over Tor).
        avatar: Option<crate::proto::content_v1::AttachmentPointer>,
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
