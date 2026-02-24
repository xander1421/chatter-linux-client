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
    /// Friend request was accepted and DM channel was created.
    FriendAccepted {
        /// User ID of the friend (who accepted our request).
        friend_user_id: String,
        /// DM channel ID created for this friendship.
        channel_id: String,
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
    // =========================================================================
    // Call Events
    // =========================================================================
    /// Incoming call notification received via sealed sender.
    IncomingCall {
        /// Peer user ID (caller).
        peer_id: UserId,
        /// Call ID.
        call_id: String,
        /// Call encryption key material.
        key_material: Vec<u8>,
        /// Call type: "Voice Call", "Video Call", etc.
        call_type: String,
        /// Channel/DM ID for context.
        channel_id: String,
        /// Timestamp when call was initiated.
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
    /// Avatar URL.
    pub avatar_url: String,
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
